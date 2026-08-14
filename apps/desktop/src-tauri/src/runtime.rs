//! Managed dsh runtime (M2): versioned installs under the app data dir,
//! an atomic `current` pointer, and a bootstrap flow that installs the
//! runtime with an isolated npm cache on first use.
//!
//! Layout (app data dir):
//! ```
//! runtime/
//!   current                 # pointer file containing the active version
//!   versions/<ver>/…        # one `npm install --prefix` tree per version
//! npm-cache/                # isolated npm cache (never ~/.npm)
//! settings.json             # channel / auto-update / registry / …
//! ```

use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};

pub const RUNTIME_DIR: &str = "runtime";
pub const VERSIONS_DIR: &str = "versions";
pub const CURRENT_FILE: &str = "current";
pub const NPM_CACHE_DIR: &str = "npm-cache";
pub const SETTINGS_FILE: &str = "settings.json";
const PKG: &str = "@deepseek-ai/dsh";
const BIN_REL: &str = "node_modules/@deepseek-ai/dsh/lib/bin.js";
const STAGING_PREFIX: &str = ".staging";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    pub channel: String,
    pub auto_update: bool,
    pub interval_hours: u32,
    pub registry: String,
    pub last_check: Option<u64>,
    pub last_project_dir: Option<String>,
    /// Close-button behaviour: "ask" | "quit" | "background".
    #[serde(default = "default_close_action")]
    pub close_action: String,
    /// Last version that reached a ready handshake (auto-rollback target).
    #[serde(default)]
    pub last_known_good: Option<String>,
    /// Rollback retention: keep this many rollback-able versions (1..=10).
    #[serde(default = "default_keep_versions")]
    pub keep_versions: u32,
}

fn default_keep_versions() -> u32 {
    1
}

/// Clamp retention to the supported 1..=10 range.
pub fn clamp_keep_versions(n: u32) -> u32 {
    n.clamp(1, 10)
}

fn default_close_action() -> String {
    "ask".to_string()
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            channel: "latest".into(),
            auto_update: true,
            interval_hours: 12,
            registry: "https://registry.npmjs.org".into(),
            last_check: None,
            last_project_dir: None,
            close_action: "ask".to_string(),
            last_known_good: None,
            keep_versions: 1,
        }
    }
}

fn app_data(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .app_data_dir()
        .map_err(|e| format!("无法解析应用数据目录: {e}"))
}

fn runtime_dir(app: &AppHandle) -> Result<PathBuf, String> {
    Ok(app_data(app)?.join(RUNTIME_DIR))
}

fn versions_dir(app: &AppHandle) -> Result<PathBuf, String> {
    Ok(runtime_dir(app)?.join(VERSIONS_DIR))
}

fn bin_of(version_dir: &Path) -> PathBuf {
    version_dir.join(BIN_REL)
}

/// Read the installed dsh version from a version dir's package.json.
fn installed_version(version_dir: &Path) -> Result<String, String> {
    let manifest: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(version_dir.join("node_modules/@deepseek-ai/dsh/package.json"))
            .map_err(|e| format!("读取已安装版本失败: {e}"))?,
    )
    .map_err(|e| format!("解析 package.json 失败: {e}"))?;
    manifest["version"]
        .as_str()
        .map(|v| v.to_string())
        .ok_or_else(|| "已安装的 dsh 缺少 version 字段".to_string())
}

/// Check whether a version dir contains a runnable dsh installation.
fn is_complete(version_dir: &Path) -> bool {
    bin_of(version_dir).is_file() && installed_version(version_dir).is_ok()
}

pub fn load_settings(app: &AppHandle) -> Settings {
    let path = match app_data(app) {
        Ok(d) => d.join(SETTINGS_FILE),
        Err(_) => return Settings::default(),
    };
    match fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
        Err(_) => Settings::default(),
    }
}

pub fn save_settings(app: &AppHandle, settings: &Settings) -> Result<(), String> {
    let mut settings = settings.clone();
    settings.keep_versions = clamp_keep_versions(settings.keep_versions);
    let dir = app_data(app)?;
    fs::create_dir_all(&dir).map_err(|e| format!("创建应用数据目录失败: {e}"))?;
    let path = dir.join(SETTINGS_FILE);
    let tmp = dir.join(".settings.tmp");
    let json = serde_json::to_string_pretty(&settings).map_err(|e| format!("序列化设置失败: {e}"))?;
    fs::write(&tmp, json).map_err(|e| format!("写入设置失败: {e}"))?;
    fs::rename(&tmp, &path).map_err(|e| format!("保存设置失败: {e}"))?;
    Ok(())
}

/// Write the `current` pointer atomically (tmp + rename).
fn write_current(app: &AppHandle, version: &str) -> Result<(), String> {
    let dir = runtime_dir(app)?;
    fs::create_dir_all(&dir).map_err(|e| format!("创建运行时目录失败: {e}"))?;
    let tmp = dir.join(".current.tmp");
    let target = dir.join(CURRENT_FILE);
    fs::write(&tmp, version).map_err(|e| format!("写入版本指针失败: {e}"))?;
    fs::rename(&tmp, &target).map_err(|e| format!("切换版本指针失败: {e}"))?;
    Ok(())
}

/// Active version per the `current` pointer (None when unmanaged/DSH_BIN).
pub fn current_version(app: &AppHandle) -> Option<String> {
    read_current(app)
}

fn read_current(app: &AppHandle) -> Option<String> {
    let text = fs::read_to_string(runtime_dir(app).ok()?.join(CURRENT_FILE)).ok()?;
    let version = text.trim().to_string();
    if version.is_empty() {
        None
    } else {
        Some(version)
    }
}

/// Whether a version dir exists and is complete.
pub fn version_complete(app: &AppHandle, version: &str) -> bool {
    versions_dir(app)
        .map(|d| is_complete(&d.join(version)))
        .unwrap_or(false)
}

/// Installed versions, newest first, with the active one marked.
pub fn list_versions(app: &AppHandle) -> Vec<VersionInfo> {
    let Ok(versions) = versions_dir(app) else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(&versions) else {
        return Vec::new();
    };
    let mut list: Vec<VersionInfo> = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') || !is_complete(&entry.path()) {
            continue;
        }
        let mtime = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        list.push(VersionInfo {
            version: name,
            active: false,
            mtime,
        });
    }
    let current = read_current(app);
    for info in &mut list {
        info.active = current.as_deref() == Some(info.version.as_str());
    }
    list.sort_by(|a, b| b.mtime.cmp(&a.mtime));
    list
}

/// Atomically point `current` at an existing, complete version.
pub fn switch_version(app: &AppHandle, version: &str) -> Result<(), String> {
    if !version_complete(app, version) {
        return Err(format!("版本 {version} 不存在或安装不完整"));
    }
    write_current(app, version)
}

/// Prune version dirs not in the keep set (current + last known good +
/// the two newest others). Staging dirs are always removed.
pub fn prune_versions(app: &AppHandle) {
    let Ok(versions) = versions_dir(app) else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(&versions) else {
        return;
    };
    let mut keep: Vec<String> = Vec::new();
    if let Some(c) = read_current(app) {
        keep.push(c);
    }
    let settings = load_settings(app);
    if let Some(lkg) = last_known_good_in(&settings) {
        keep.push(lkg);
    }
    // newest keep_versions complete versions as extra keep
    let n = clamp_keep_versions(settings.keep_versions) as usize;
    let mut list = list_versions(app);
    list.retain(|v| !keep.contains(&v.version));
    keep.extend(list.into_iter().take(n).map(|v| v.version));

    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let is_staging = name.starts_with(STAGING_PREFIX);
        let path = entry.path();
        if is_staging || (!keep.contains(&name) && path.is_dir()) {
            let _ = std::fs::remove_dir_all(&path);
            println!("[dsh] pruned version dir: {name}");
        }
    }
}

/// Last version that reached a ready handshake (auto-rollback target).
pub fn last_known_good(app: &AppHandle) -> Option<String> {
    last_known_good_in(&load_settings(app))
}

pub fn set_last_known_good(app: &AppHandle, version: &str) {
    let mut settings = load_settings(app);
    if settings.last_known_good.as_deref() != Some(version) {
        settings.last_known_good = Some(version.to_string());
        let _ = save_settings(app, &settings);
    }
}

fn last_known_good_in(settings: &Settings) -> Option<String> {
    settings.last_known_good.clone()
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VersionInfo {
    pub version: String,
    pub active: bool,
    pub mtime: u64,
}

/// Crash-recovery: remove every installed version and the current pointer
/// so the next start falls through to a fresh bootstrap.
pub fn reset_all(app: &AppHandle) -> Result<(), String> {
    let runtime = runtime_dir(app)?;
    let versions = runtime.join(VERSIONS_DIR);
    if versions.is_dir() {
        for entry in std::fs::read_dir(&versions)
            .map_err(|e| format!("读取版本目录失败: {e}"))?
            .flatten()
        {
            if entry.path().is_dir() {
                std::fs::remove_dir_all(entry.path())
                    .map_err(|e| format!("删除版本目录失败: {e}"))?;
            }
        }
    }
    let current = runtime.join(CURRENT_FILE);
    let _ = std::fs::remove_file(current);
    // Reset rollback/check state so the fresh install starts clean.
    let mut settings = load_settings(app);
    settings.last_known_good = None;
    settings.last_check = None;
    let _ = save_settings(app, &settings);
    Ok(())
}

/// Resolution outcome for the dsh bin used to launch the child.
#[derive(Debug)]
pub enum Resolve {
    Ready(PathBuf),
    NeedBootstrap,
    Failed(String),
}

/// Resolve the dsh bin: `DSH_BIN` override wins (dev/test), then the managed
/// runtime (current pointer → newest complete version), else bootstrap.
pub fn resolve_active_bin(app: &AppHandle) -> Resolve {
    if let Ok(p) = std::env::var("DSH_BIN") {
        if !p.trim().is_empty() {
            let pb = PathBuf::from(p.trim());
            if pb.is_file() {
                return Resolve::Ready(pb);
            }
            return Resolve::Failed(format!("DSH_BIN 指向的文件不存在: {p}"));
        }
    }

    let versions = match versions_dir(app) {
        Ok(d) => d,
        Err(e) => return Resolve::Failed(e),
    };
    let entries = match fs::read_dir(&versions) {
        Ok(e) => e,
        Err(_) => return Resolve::NeedBootstrap,
    };

    // current pointer first
    if let Some(current) = read_current(app) {
        let dir = versions.join(&current);
        if is_complete(&dir) {
            return Resolve::Ready(bin_of(&dir));
        }
    }

    // fallback: newest complete version dir (sorted by mtime desc)
    let mut complete: Vec<(SystemTime, PathBuf)> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }
        if is_complete(&path) {
            let mtime = entry.metadata().and_then(|m| m.modified()).unwrap_or(SystemTime::UNIX_EPOCH);
            complete.push((mtime, path));
        }
    }
    complete.sort_by(|a, b| b.0.cmp(&a.0));
    match complete.first() {
        Some((_, dir)) => Resolve::Ready(bin_of(dir)),
        None => Resolve::NeedBootstrap,
    }
}

/// Install a fresh runtime. Reports progress lines through `on_line`.
/// Returns the installed version.
/// Install a runtime; `spec` is either a dist-tag (`latest`/`next`) or an
/// exact version (`0.1.0-rc.7`). Same staging → verify → pointer flow.
pub fn bootstrap(
    app: &AppHandle,
    settings: &Settings,
    spec: &str,
    on_line: &(dyn Fn(&str) + Send + Sync),
) -> Result<String, String> {
    // npm ships with the bundled Node; check whichever toolchain applies.
    let (npm_program, npm_prefix) = crate::nodejs::npm_invocation(app);
    let npm_check = Command::new(&npm_program)
        .args(&npm_prefix)
        .arg("--version")
        .output()
        .map_err(|e| format!("找不到 npm（捆绑缺失且 PATH 中无 npm，插件与运行时安装依赖它）: {e}"))?;
    if !npm_check.status.success() {
        return Err("npm --version 执行失败".to_string());
    }
    on_line(&format!("npm {}", String::from_utf8_lossy(&npm_check.stdout).trim()));

    let data = app_data(app)?;
    let versions = versions_dir(app)?;
    fs::create_dir_all(&versions).map_err(|e| format!("创建版本目录失败: {e}"))?;
    let cache = data.join(NPM_CACHE_DIR);
    fs::create_dir_all(&cache).map_err(|e| format!("创建 npm 缓存目录失败: {e}"))?;

    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let staging = versions.join(format!("{}-{secs}", STAGING_PREFIX));
    let _ = fs::remove_dir_all(&staging);

    on_line(&format!("安装 {PKG}@{spec}({})…", settings.registry));
    let mut cmd = Command::new(&npm_program);
    cmd.args(&npm_prefix);
    if let Some(path) = crate::nodejs::enriched_path(app) {
        cmd.env("PATH", path);
    }
    cmd.args(["install", &format!("{PKG}@{spec}")])
        .args(["--prefix", staging.to_str().unwrap_or_default()])
        .args(["--cache", cache.to_str().unwrap_or_default()])
        .args(["--registry", &settings.registry])
        .args(["--no-audit", "--no-fund", "--loglevel", "error"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| format!("启动 npm install 失败: {e}"))?;

    // Collect npm output on a reader thread (npm is quiet while downloading,
    // so the loop below also emits a heartbeat so the UI never looks stuck).
    use std::sync::{Arc, Mutex};
    let npm_out: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let npm_out2 = npm_out.clone();
    if let Some(out) = child.stdout.take() {
        std::thread::spawn(move || {
            for line in BufReader::new(out).lines().map_while(Result::ok) {
                if !line.trim().is_empty() {
                    npm_out2.lock().unwrap().push(line);
                }
            }
        });
    }
    let npm_out3 = npm_out.clone();
    if let Some(err) = child.stderr.take() {
        std::thread::spawn(move || {
            for line in BufReader::new(err).lines().map_while(Result::ok) {
                if !line.trim().is_empty() {
                    npm_out3.lock().unwrap().push(format!("[stderr] {line}"));
                }
            }
        });
    }

    // Activity is communicated by the shell UI's indeterminate progress bar;
    // this loop only waits and enforces nothing else.
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(200)),
            Err(e) => {
                let _ = child.kill();
                let _ = fs::remove_dir_all(&staging);
                return Err(format!("等待 npm install 失败: {e}"));
            }
        }
    };
    if !status.success() {
        let tail: Vec<String> = npm_out.lock().unwrap().iter().rev().take(8).cloned().rev().collect();
        let _ = fs::remove_dir_all(&staging);
        let detail = if tail.is_empty() {
            String::new()
        } else {
            format!("\n{}", tail.join("\n"))
        };
        return Err(format!(
            "npm install 退出码 {:?}（网络、registry 或依赖脚本问题）{detail}",
            status.code()
        ));
    }
    for line in npm_out.lock().unwrap().iter() {
        if !line.trim().is_empty() {
            on_line(line);
        }
    }

    let version = installed_version(&staging)?;
    on_line(&format!("已安装 {PKG}@{version}，正在验证…"));

    let target = versions.join(&version);
    if target.exists() {
        let _ = fs::remove_dir_all(&staging);
        on_line(&format!("版本 {version} 已存在，复用"));
    } else {
        fs::rename(&staging, &target).map_err(|e| format!("归档版本目录失败: {e}"))?;
    }
    if !is_complete(&target) {
        return Err(format!("版本 {version} 安装不完整"));
    }

    // Verify the exact CLI runs and reports the right version (bundled
    // toolchain preferred — a bare PATH must not fail here — with a 10s
    // timeout so a hang can never sit silently).
    let (verify_node, _) = crate::nodejs::node_program(app);
    let mut verify = Command::new(&verify_node)
        .arg(bin_of(&target))
        .arg("--version")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("验证 --version 失败: {e}"))?;
    let mut verify_out = verify.stdout.take();
    let started = std::time::Instant::now();
    let vstatus = loop {
        match verify.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) => {
                if started.elapsed() > std::time::Duration::from_secs(10) {
                    let _ = verify.kill();
                    let _ = verify.wait();
                    return Err("验证 --version 超时（10 秒）".to_string());
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            Err(e) => return Err(format!("验证 --version 失败: {e}")),
        }
    };
    let mut reported = String::new();
    if let Some(mut out) = verify_out.take() {
        let _ = out.read_to_string(&mut reported);
    }
    let reported = reported.trim().to_string();
    if !vstatus.success() || reported != version {
        return Err(format!("版本验证失败: 期望 {version}，实际 {reported}"));
    }

    write_current(app, &version)?;
    on_line(&format!("运行时 {version} 就绪"));
    Ok(version)
}
