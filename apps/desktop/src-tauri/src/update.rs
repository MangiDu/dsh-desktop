//! Manual update check + apply (supplementary requirement #3).
//!
//! Flow: menu "检查更新…" → compare installed version with the registry →
//! dialog asks whether to update now → background install into a fresh
//! version dir (same staging flow as bootstrap) → ask whether to restart →
//! restart the dsh child with the new pointer.

use std::sync::Arc;
use std::thread;

use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons};
use tauri_plugin_notification::NotificationExt;

/// Installed-vs-latest comparison result.
pub struct Check {
    pub current: String,
    pub latest: String,
    pub updatable: bool,
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

fn notify(app: &AppHandle, title: &str, body: &str) {
    println!("[dsh] notify: {title} :: {body}");
    let _ = app.notification().builder().title(title).body(body).show();
}

/// Menu handler: check → ask → install → ask restart → restart.
pub fn check_and_update(app: &AppHandle, shared: &Arc<crate::dsh::Shared>) {
    let app2 = app.clone();
    let shared2 = shared.clone();
    thread::spawn(move || {
        let settings = crate::runtime::load_settings(&app2);
        let result = check(&app2, &settings);
        let info = match result {
            Ok(info) => info,
            Err(e) => {
                let _ = app2.dialog().message(format!("检查更新失败：{e}")).title("dsh 更新").kind(tauri_plugin_dialog::MessageDialogKind::Error).blocking_show();
                return;
            }
        };

        if !info.updatable {
            let msg = if info.current == info.latest {
                format!("已是最新版本：{}", info.current)
            } else {
                format!(
                    "当前 {}（{}），最新 {}",
                    info.current,
                    if info.current == "dev" { "自定义 DSH_BIN，不适用自动更新" } else { "未解析" },
                    info.latest
                )
            };
            let _ = app2.dialog().message(msg).title("dsh 更新").blocking_show();
            return;
        }

        let do_update = app2
            .dialog()
            .message(format!(
                "发现新版本 {}（当前 {}）。\n现在更新吗？安装完成后会询问是否重启 dsh。",
                info.latest, info.current
            ))
            .title("dsh 更新")
            .buttons(MessageDialogButtons::OkCancelCustom("立即更新".into(), "稍后".into()))
            .blocking_show();
        if !do_update {
            notify(&app2, "dsh 更新", "已跳过本次更新，可随时从菜单再次检查。");
            return;
        }

        // Install with progress broadcast to the shell UI.
        let app3 = app2.clone();
        let shared3 = shared2.clone();
        let on_line = move |line: &str| {
            println!("[dsh] update: {line}");
            shared3.push_line(line.to_string());
            let phase = crate::dsh::Phase::Bootstrapping { line: line.to_string() };
            *shared3.phase.lock().unwrap() = phase.clone();
            let _ = app3.emit(crate::dsh::STATE_EVENT, &phase);
        };
        let spec = info.latest.clone();
        match crate::runtime::bootstrap(&app2, &settings, &spec, &on_line) {
            Ok(version) => {
                notify(&app2, "dsh 更新", &format!("v{version} 已就绪。"));
                let restart_now = app2
                    .dialog()
                    .message(format!("v{version} 安装完成。重启 dsh 立即生效？（重启会中断当前会话）"))
                    .title("dsh 更新")
                    .buttons(MessageDialogButtons::OkCancelCustom("立即重启".into(), "稍后重启".into()))
                    .blocking_show();
                if restart_now {
                    let _ = crate::dsh::start(&app2, &shared2);
                } else {
                    notify(&app2, "dsh 更新", "将在下次重启后生效（菜单「重启 dsh」可随时切换）。");
                }
            }
            Err(e) => {
                let _ = app2
                    .dialog()
                    .message(format!("更新安装失败：{e}"))
                    .title("dsh 更新")
                    .kind(tauri_plugin_dialog::MessageDialogKind::Error)
                    .blocking_show();
            }
        }
    });
}
