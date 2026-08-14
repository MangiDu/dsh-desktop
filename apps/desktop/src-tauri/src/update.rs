//! Manual update check + apply (supplementary requirement #3).
//!
//! Flow: menu "检查更新…" opens the shell window's update panel → the UI
//! calls `update_check` (npm view against the configured registry, semver
//! compare) → on newer version the user clicks 立即更新 → `update_apply`
//! installs into a fresh version dir in the background (staging + atomic
//! `current` pointer; the running version is untouched), streaming progress
//! lines to the panel → the panel offers 立即重启 (dsh_restart).

use std::io::{BufReader, Read};
use std::process::Stdio;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager};

/// Installed-vs-latest comparison result (serialized to the update panel).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Check {
    pub current: String,
    pub latest: String,
    pub updatable: bool,
    /// Whether this result came from the short-lived cache.
    #[serde(default)]
    pub cached: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct Done {
    ok: bool,
    version: String,
    error: Option<String>,
}

/// Query the registry for the latest version on the configured channel.
/// Uses `npm view` with the app's isolated cache so a broken user-level
/// npm cache can never affect the check.
pub fn check(app: &AppHandle, settings: &crate::runtime::Settings) -> Result<Check, String> {
    let current = crate::runtime::current_version(app).unwrap_or_else(|| "dev".to_string());

    let data = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("无法解析应用数据目录: {e}"))?;
    let cache = data.join(crate::runtime::NPM_CACHE_DIR);
    let _ = std::fs::create_dir_all(&cache);

    let (npm_program, npm_prefix) = crate::nodejs::npm_invocation(app);
    let mut child = std::process::Command::new(&npm_program)
        .args(&npm_prefix)
        .args(["view", "@deepseek-ai/dsh", "version"])
        .args(["--registry", &settings.registry])
        .args(["--cache", cache.to_str().unwrap_or_default()])
        .args(["--json"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("执行 npm view 失败（npm 可用性）: {e}"))?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) => {
                if started.elapsed() > Duration::from_secs(20) {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err("查询 registry 超时（20 秒），请检查网络或 registry 配置".to_string());
                }
            }
            Err(e) => return Err(format!("npm view 执行失败: {e}")),
        }
        thread::sleep(Duration::from_millis(200));
    };
    if !status.success() {
        let mut err_text = String::new();
        if let Some(err) = stderr {
            let _ = BufReader::new(err).read_to_string(&mut err_text);
        }
        return Err(format!("查询 registry 失败: {}", err_text.trim()));
    }
    // `npm view --json` prints a JSON string literal, e.g. "0.1.0-rc.6"
    let mut raw = String::new();
    if let Some(out) = stdout {
        let _ = BufReader::new(out).read_to_string(&mut raw);
    }
    let latest = raw.trim().trim_matches('"').to_string();

    let updatable = match (semver::Version::parse(&current), semver::Version::parse(&latest)) {
        (Ok(c), Ok(l)) => l > c,
        _ => false,
    };
    Ok(Check {
        current,
        latest,
        updatable,
        cached: false,
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryEntry {
    pub at: u64,
    pub from: String,
    pub to: String,
    pub result: String,
    pub error: Option<String>,
}

const HISTORY_FILE: &str = "update-history.json";
const CHECK_CACHE_FILE: &str = "update-check-cache.json";
const CHECK_FRESH_SECS: u64 = 1800;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CheckCache {
    at: u64,
    current: String,
    latest: String,
    updatable: bool,
}

fn check_cache_path(app: &AppHandle) -> Result<std::path::PathBuf, String> {
    let data = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("无法解析应用数据目录: {e}"))?;
    Ok(data.join(CHECK_CACHE_FILE))
}

fn read_check_cache(app: &AppHandle) -> Option<CheckCache> {
    let path = check_cache_path(app).ok()?;
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

fn write_check_cache(app: &AppHandle, check: &Check) {
    let Ok(path) = check_cache_path(app) else {
        return;
    };
    let cache = CheckCache {
        at: unix_now(),
        current: check.current.clone(),
        latest: check.latest.clone(),
        updatable: check.updatable,
    };
    if let Ok(json) = serde_json::to_string_pretty(&cache) {
        let _ = std::fs::write(path, json);
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn history_path(app: &AppHandle) -> Result<std::path::PathBuf, String> {
    let data = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("无法解析应用数据目录: {e}"))?;
    Ok(data.join(HISTORY_FILE))
}

fn load_history(app: &AppHandle) -> Vec<HistoryEntry> {
    let Ok(path) = history_path(app) else {
        return Vec::new();
    };
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// Append one update-history entry (capped at 50, atomic save).
pub fn record_update(app: &AppHandle, from: &str, to: &str, result: &str, error: Option<String>) {
    let mut entries = load_history(app);
    entries.push(HistoryEntry {
        at: unix_now(),
        from: from.to_string(),
        to: to.to_string(),
        result: result.to_string(),
        error,
    });
    if entries.len() > 50 {
        entries = entries.split_off(entries.len() - 50);
    }
    let Ok(path) = history_path(app) else {
        return;
    };
    let Some(dir) = path.parent().map(|p| p.to_path_buf()) else {
        return;
    };
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(json) = serde_json::to_string_pretty(&entries) {
        let tmp = dir.join(".update-history.tmp");
        let _ = std::fs::write(&tmp, json);
        let _ = std::fs::rename(&tmp, &path);
    }
}

#[tauri::command]
pub fn update_history_list(app: AppHandle) -> Vec<HistoryEntry> {
    let mut entries = load_history(&app);
    entries.reverse();
    entries.truncate(5);
    entries
}

#[tauri::command]
pub fn update_list_versions(app: AppHandle) -> Vec<crate::runtime::VersionInfo> {
    crate::runtime::list_versions(&app)
}

/// Rollback entry: point `current` at an installed version and restart dsh.
#[tauri::command]
pub async fn update_switch(
    app: AppHandle,
    state: tauri::State<'_, crate::AppState>,
    version: String,
) -> Result<(), String> {
    let shared = state.0.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let previous =
            crate::runtime::current_version(&app).unwrap_or_else(|| "未知".to_string());
        crate::runtime::switch_version(&app, &version)?;
        crate::update::record_update(&app, &previous, &version, "switched", None);
        let _ = crate::dsh::start(&app, &shared);
        Ok(())
    })
    .await
    .map_err(|e| format!("切换任务执行失败: {e}"))?
}

/// Panel command: compare installed vs registry.
#[tauri::command]
pub async fn update_check(app: AppHandle, force: Option<bool>) -> Result<Check, String> {
    // Blocking commands run inline on the MAIN thread in Tauri; a live npm
    // view here would freeze the whole app. Run it on a blocking worker.
    tauri::async_runtime::spawn_blocking(move || {
        let settings = crate::runtime::load_settings(&app);
        let now = unix_now();
        let fresh = settings
            .last_check
            .is_some_and(|lc| now.saturating_sub(lc) < CHECK_FRESH_SECS);
        // Serve the cached result instantly for panel switches; 重新检查
        // forces a live registry query.
        if force != Some(true) && fresh {
            if let Some(cache) = read_check_cache(&app) {
                return Ok(Check {
                    current: cache.current,
                    latest: cache.latest,
                    updatable: cache.updatable,
                    cached: true,
                });
            }
        }
        let result = check(&app, &settings)?;
        let mut saved = settings;
        saved.last_check = Some(now);
        let _ = crate::runtime::save_settings(&app, &saved);
        write_check_cache(&app, &result);
        Ok(result)
    })
    .await
    .map_err(|e| format!("检查任务执行失败: {e}"))?
}

/// Panel command: install the newest version in the background. Progress
/// streams as `dsh://update-line`; completion as `dsh://update-done`.
#[tauri::command]
pub fn update_apply(app: AppHandle, state: tauri::State<'_, crate::AppState>) -> Result<(), String> {
    let shared = state.0.clone();
    shared.install_busy.store(true, Ordering::SeqCst);
    thread::spawn(move || {
        let settings = crate::runtime::load_settings(&app);
        let from = crate::runtime::current_version(&app).unwrap_or_else(|| "dev".to_string());
        let latest = match check(&app, &settings) {
            Ok(c) if c.updatable => c.latest,
            Ok(c) => {
                let _ = app.emit(
                    "dsh://update-done",
                    Done {
                        ok: false,
                        version: c.latest,
                        error: Some(format!("已是最新版本 {}，无需更新", c.current)),
                    },
                );
                return;
            }
            Err(e) => {
                let _ = app.emit(
                    "dsh://update-done",
                    Done {
                        ok: false,
                        version: String::new(),
                        error: Some(e),
                    },
                );
                return;
            }
        };

        let app_lines = app.clone();
        let on_line = move |line: &str| {
            println!("[dsh] update: {line}");
            let _ = app_lines.emit("dsh://update-line", line.to_string());
        };
        match crate::runtime::bootstrap(&app, &settings, &latest, &on_line) {
            Ok(version) => {
                record_update(&app, &from, &version, "ok", None);
                let _ = app.emit(
                    "dsh://update-done",
                    Done {
                        ok: true,
                        version,
                        error: None,
                    },
                );
            }
            Err(e) => {
                record_update(&app, &from, &latest, "failed", Some(e.clone()));
                let _ = app.emit(
                    "dsh://update-done",
                    Done {
                        ok: false,
                        version: latest,
                        error: Some(e),
                    },
                );
            }
        }
        shared.push_line("update flow finished".to_string());
        shared.install_busy.store(false, Ordering::SeqCst);
    });
    Ok(())
}

/// Background scheduler: when autoUpdate is on, check every intervalHours
/// (lastCheck persisted, 60s tick), silently install newer versions, and
/// notify that a restart activates them — never restarts by itself.
pub fn spawn_scheduler(app: AppHandle, shared: Arc<crate::dsh::Shared>) {
    thread::spawn(move || loop {
        thread::sleep(Duration::from_secs(60));
        let settings = crate::runtime::load_settings(&app);
        if !settings.auto_update || shared.install_busy.load(Ordering::SeqCst) {
            continue;
        }
        let now = unix_now();
        let interval = u64::from(settings.interval_hours.max(1)) * 3600;
        let due = settings.last_check.is_none_or(|lc| now.saturating_sub(lc) >= interval);
        if !due {
            continue;
        }
        let mut saved = settings.clone();
        saved.last_check = Some(now);
        let _ = crate::runtime::save_settings(&app, &saved);
        match check(&app, &settings) {
            Ok(info) => {
                write_check_cache(&app, &info);
                if info.updatable {
                    auto_install(&app, &shared, &settings, &info);
                }
            }
            Err(e) => println!("[dsh] auto-update check failed: {e}"),
        }
    });
}

fn auto_install(
    app: &AppHandle,
    shared: &Arc<crate::dsh::Shared>,
    settings: &crate::runtime::Settings,
    info: &Check,
) {
    shared.install_busy.store(true, Ordering::SeqCst);
    let app_lines = app.clone();
    let on_line = move |line: &str| {
        println!("[dsh] auto-update: {line}");
        let _ = app_lines.emit("dsh://update-line", line.to_string());
    };
    let result = crate::runtime::bootstrap(app, settings, &info.latest, &on_line);
    match result {
        Ok(version) => {
            record_update(app, &info.current, &version, "ok", None);
            crate::listener::toast(
                app,
                shared,
                "dsh 更新",
                &format!("v{version} 已就绪，重启 dsh 后生效（菜单「重启 dsh」）。"),
            );
        }
        Err(e) => {
            record_update(app, &info.current, &info.latest, "failed", Some(e.clone()));
            crate::listener::toast(app, shared, "dsh 更新", &format!("自动更新失败：{e}"));
        }
    }
    shared.install_busy.store(false, Ordering::SeqCst);
}
