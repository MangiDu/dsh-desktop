//! Manual update check + apply (supplementary requirement #3).
//!
//! Flow: menu "检查更新…" opens the shell window's update panel → the UI
//! calls `update_check` (npm view against the configured registry, semver
//! compare) → on newer version the user clicks 立即更新 → `update_apply`
//! installs into a fresh version dir in the background (staging + atomic
//! `current` pointer; the running version is untouched), streaming progress
//! lines to the panel → the panel offers 立即重启 (dsh_restart).

use std::thread;

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};

/// Installed-vs-latest comparison result (serialized to the update panel).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Check {
    pub current: String,
    pub latest: String,
    pub updatable: bool,
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

    let out = std::process::Command::new("npm")
        .args(["view", "@deepseek-ai/dsh", "version"])
        .args(["--registry", &settings.registry])
        .args(["--cache", cache.to_str().unwrap_or_default()])
        .args(["--json"])
        .output()
        .map_err(|e| format!("执行 npm view 失败（npm 可用性）: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "查询 registry 失败: {}",
            String::from_utf8_lossy(&out.stderr).trim().to_string()
        ));
    }
    // `npm view --json` prints a JSON string literal, e.g. "0.1.0-rc.6"
    let raw = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let latest = raw.trim_matches('"').to_string();

    let updatable = match (semver::Version::parse(&current), semver::Version::parse(&latest)) {
        (Ok(c), Ok(l)) => l > c,
        _ => false,
    };
    Ok(Check {
        current,
        latest,
        updatable,
    })
}

/// Panel command: compare installed vs registry.
#[tauri::command]
pub fn update_check(app: AppHandle) -> Result<Check, String> {
    let settings = crate::runtime::load_settings(&app);
    check(&app, &settings)
}

/// Panel command: install the newest version in the background. Progress
/// streams as `dsh://update-line`; completion as `dsh://update-done`.
#[tauri::command]
pub fn update_apply(app: AppHandle, state: tauri::State<'_, crate::AppState>) -> Result<(), String> {
    let shared = state.0.clone();
    thread::spawn(move || {
        let settings = crate::runtime::load_settings(&app);
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
    });
    Ok(())
}
