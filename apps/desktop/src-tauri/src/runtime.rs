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
use std::io::{BufRead, BufReader};
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
    let dir = app_data(app)?;
    fs::create_dir_all(&dir).map_err(|e| format!("创建应用数据目录失败: {e}"))?;
    let path = dir.join(SETTINGS_FILE);
    let tmp = dir.join(".settings.tmp");
    let json = serde_json::to_string_pretty(settings).map_err(|e| format!("序列化设置失败: {e}"))?;
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
    // npm must exist (it ships with Node, which start() already checked).
    let npm_check = Command::new("npm")
        .arg("--version")
        .output()
        .map_err(|e| format!("找不到 npm（PATH 中无 npm，插件与运行时安装依赖它）: {e}"))?;
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
    let mut cmd = Command::new("npm");
    cmd.args(["install", &format!("{PKG}@{spec}")])
        .args(["--prefix", staging.to_str().unwrap_or_default()])
        .args(["--cache", cache.to_str().unwrap_or_default()])
        .args(["--registry", &settings.registry])
        .args(["--no-audit", "--no-fund", "--loglevel", "error"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| format!("启动 npm install 失败: {e}"))?;
    if let Some(out) = child.stdout.take() {
        for line in BufReader::new(out).lines().map_while(Result::ok) {
            if !line.trim().is_empty() {
                on_line(&line);
            }
        }
    }
    let status = child.wait().map_err(|e| format!("等待 npm install 失败: {e}"))?;
    if !status.success() {
        let _ = fs::remove_dir_all(&staging);
        return Err(format!("npm install 退出码 {:?}（网络或 registry 问题）", status.code()));
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

    // Verify the exact CLI runs and reports the right version.
    let check = Command::new("node")
        .arg(bin_of(&target))
        .arg("--version")
        .output()
        .map_err(|e| format!("验证 --version 失败: {e}"))?;
    let reported = String::from_utf8_lossy(&check.stdout).trim().to_string();
    if !check.status.success() || reported != version {
        return Err(format!("版本验证失败: 期望 {version}，实际 {reported}"));
    }

    write_current(app, &version)?;
    on_line(&format!("运行时 {version} 就绪"));
    Ok(version)
}
