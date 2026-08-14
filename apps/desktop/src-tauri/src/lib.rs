mod dsh;
mod listener;
mod nodejs;
mod runtime;
mod update;

use std::sync::Arc;

use dsh::Phase;
use tauri::menu::{MenuBuilder, MenuItemBuilder, PredefinedMenuItem, SubmenuBuilder};
use tauri_plugin_dialog::DialogExt;
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Emitter, Manager, RunEvent};

/// Managed state: everything the watcher threads and commands share.
pub(crate) struct AppState(pub(crate) Arc<dsh::Shared>);

#[tauri::command]
fn dsh_status(_app: AppHandle, state: tauri::State<'_, AppState>) -> Phase {
    state.0.phase.lock().unwrap().clone()
}

#[tauri::command]
fn dsh_restart(app: AppHandle, state: tauri::State<'_, AppState>) -> Result<(), String> {
    dsh::start(&app, &state.0)
}

#[tauri::command]
fn app_quit(app: AppHandle) {
    app.exit(0);
}

/// Close-confirm panel: act on the user's choice, optionally remembering it.
#[tauri::command]
fn close_choice(app: AppHandle, quit: bool, remember: bool) -> Result<(), String> {
    if remember {
        let mut settings = runtime::load_settings(&app);
        settings.close_action = if quit { "quit" } else { "background" }.to_string();
        runtime::save_settings(&app, &settings)?;
    }
    if quit {
        app.exit(0);
    } else {
        if let Some(w) = app.get_webview_window(dsh::MAIN_LABEL) {
            let _ = w.hide();
        }
        if let Some(s) = app.get_webview_window(dsh::SPLASH_LABEL) {
            let _ = s.close();
        }
    }
    Ok(())
}

#[tauri::command]
fn settings_get(app: AppHandle) -> runtime::Settings {
    runtime::load_settings(&app)
}

#[tauri::command]
fn settings_set(app: AppHandle, settings: runtime::Settings) -> Result<(), String> {
    runtime::save_settings(&app, &settings)
}

#[tauri::command]
fn runtime_bootstrap(app: AppHandle, state: tauri::State<'_, AppState>) -> Result<(), String> {
    dsh::bootstrap_now(&app, &state.0)
}

/// Spawn `dsh plugin --profile web <args…>` and stream output to the shell
/// UI (`dsh://plugin-log` lines, `dsh://plugin-done` exit code).
fn run_plugin_args(app: &AppHandle, args: Vec<String>) -> Result<(), String> {
    let bin = match runtime::resolve_active_bin(app) {
        runtime::Resolve::Ready(b) => b,
        runtime::Resolve::NeedBootstrap => {
            return Err("dsh 运行时尚未安装，请先启动一次应用完成初始化".to_string())
        }
        runtime::Resolve::Failed(e) => return Err(e),
    };
    let mut cmd = std::process::Command::new("node");
    cmd.arg(&bin)
        .arg("plugin")
        .arg("--profile")
        .arg("web")
        .args(&args)
        .env("DSH_DESKTOP_CHILD", "1")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| format!("启动 dsh plugin 失败: {e}"))?;

    use std::io::{BufRead, BufReader};
    let app_out = app.clone();
    if let Some(out) = child.stdout.take() {
        std::thread::spawn(move || {
            for line in BufReader::new(out).lines().map_while(Result::ok) {
                let _ = app_out.emit("dsh://plugin-log", line);
            }
        });
    }
    let app_err = app.clone();
    if let Some(err) = child.stderr.take() {
        std::thread::spawn(move || {
            for line in BufReader::new(err).lines().map_while(Result::ok) {
                let _ = app_err.emit("dsh://plugin-log", format!("[stderr] {line}"));
            }
        });
    }
    let app_done = app.clone();
    std::thread::spawn(move || {
        let code = child.wait().map(|s| s.code().unwrap_or(-1)).unwrap_or(-1);
        let _ = app_done.emit("dsh://plugin-done", code);
    });
    Ok(())
}

/// Online install from a registry package name.
#[tauri::command]
fn dsh_plugin(app: AppHandle, args: Vec<String>) -> Result<(), String> {
    run_plugin_args(&app, args)
}

/// Offline install: pick a zip (extracted to the app data dir) or a plugin
/// directory, then `dsh plugin --profile web add <local path> --offline`.
/// Local-path specs are a first-class `dsh plugin` input (pnpm link:); the
/// reconcile step still wires the package into dsh.profile.bundles.
#[tauri::command]
async fn dsh_plugin_offline(app: AppHandle, kind: String) -> Result<(), String> {
    // The blocking pickers must not run on the main thread (deadlock);
    // whole flow on a blocking worker.
    let target = tauri::async_runtime::spawn_blocking(move || offline_install_target(&app, &kind))
        .await
        .map_err(|e| format!("离线安装任务执行失败: {e}"))??;
    if target.is_none() {
        return Ok(()); // picker cancelled
    }
    Ok(())
}

fn offline_install_target(app: &AppHandle, kind: &str) -> Result<Option<std::path::PathBuf>, String> {
    let target = match kind {
        "zip" => {
            let Some(file) = app
                .dialog()
                .file()
                .add_filter("插件 zip 包", &["zip"])
                .blocking_pick_file()
            else {
                return Ok(None); // cancelled
            };
            let zip_path = file
                .into_path()
                .map_err(|e| format!("读取所选文件路径失败: {e}"))?;
            let base = app
                .path()
                .app_data_dir()
                .map_err(|e| format!("无法解析应用数据目录: {e}"))?
                .join("offline-plugins");
            let secs = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let out = base.join(format!("pkg-{secs}"));
            std::fs::create_dir_all(&out).map_err(|e| format!("创建解压目录失败: {e}"))?;
            let file_handle =
                std::fs::File::open(&zip_path).map_err(|e| format!("打开 zip 失败: {e}"))?;
            let mut archive =
                zip::ZipArchive::new(file_handle).map_err(|e| format!("不是有效的 zip: {e}"))?;
            archive.extract(&out).map_err(|e| format!("解压失败: {e}"))?;
            let _ = app.emit("dsh://plugin-log", format!("已解压: {}", out.display()));
            resolve_plugin_root(&out)?
        }
        "dir" => {
            let Some(folder) = app.dialog().file().blocking_pick_folder() else {
                return Ok(None); // cancelled
            };
            let path = folder
                .into_path()
                .map_err(|e| format!("读取所选目录路径失败: {e}"))?;
            resolve_plugin_root(&path)?
        }
        _ => return Err("未知的离线安装类型".to_string()),
    };
    let target_str = target.to_string_lossy().to_string();
    let _ = app.emit(
        "dsh://plugin-log",
        format!("离线安装: dsh plugin --profile web add {target_str} --offline"),
    );
    run_plugin_args(app, vec!["add".to_string(), target_str, "--offline".to_string()])?;
    Ok(Some(target))
}

/// The folder that contains the plugin's package.json — the picked/extracted
/// root itself, or its single subdirectory (common zip layout).
fn resolve_plugin_root(root: &std::path::Path) -> Result<std::path::PathBuf, String> {
    if root.join("package.json").is_file() {
        return Ok(root.to_path_buf());
    }
    let entries: Vec<_> = std::fs::read_dir(root)
        .map_err(|e| format!("读取目录失败: {e}"))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .collect();
    let with_manifest: Vec<_> = entries
        .iter()
        .filter(|p| p.is_dir() && p.join("package.json").is_file())
        .collect();
    match with_manifest.as_slice() {
        [one] => Ok(one.to_path_buf()),
        _ => Err("未在所选目录（或其唯一子目录）中找到 package.json，请选择插件包根目录".to_string()),
    }
}

/// The latest toast payload, consumed by the toast page on load (the
/// emit can race ahead of a freshly created window's page load).
#[tauri::command]
fn toast_current(state: tauri::State<'_, AppState>) -> Option<(String, String)> {
    state.0.toast.lock().ok()?.clone()
}

/// Toast banner click: focus the main window and dismiss the toast.
#[tauri::command]
fn toast_clicked(app: AppHandle) {
    show_main_or_splash(&app);
    if let Some(w) = app.get_webview_window(listener::TOAST_LABEL) {
        let _ = w.hide();
    }
}

/// The dsh data directory (DSH_HOME; ~/.dsh by default) — same value the
/// dsh child resolves.
#[tauri::command]
fn dsh_home_info(app: AppHandle) -> String {
    match std::env::var("DSH_HOME") {
        Ok(v) if !v.trim().is_empty() => v,
        _ => app
            .path()
            .home_dir()
            .map(|h| h.join(".dsh").to_string_lossy().to_string())
            .unwrap_or_else(|_| "~/.dsh".to_string()),
    }
}

/// The panel the shell UI should show on load, consumed once.
#[tauri::command]
fn ui_intent(state: tauri::State<'_, AppState>) -> Option<String> {
    state.0.ui_intent.lock().ok()?.take()
}

/// Crash recovery: wipe the managed runtime and re-bootstrap.
#[tauri::command]
async fn runtime_reset(app: AppHandle, state: tauri::State<'_, AppState>) -> Result<(), String> {
    let shared = state.0.clone();
    tauri::async_runtime::spawn_blocking(move || {
        crate::runtime::reset_all(&app)?;
        shared.fail_streak.store(0, std::sync::atomic::Ordering::SeqCst);
        let _ = crate::dsh::bootstrap_now(&app, &shared);
        Ok(())
    })
    .await
    .map_err(|e| format!("重置任务执行失败: {e}"))?
}

/// Open the shell window on a specific panel ("plugin" | "update").
fn open_shell_panel(app: &AppHandle, panel: &str) {
    println!("[dsh] open_shell_panel: {panel}");
    if let Ok(mut intent) = app.state::<AppState>().0.ui_intent.lock() {
        *intent = Some(panel.to_string());
    }
    // Push the panel to an ALREADY-LOADED shell window (its UI consumed the
    // invoke-based intent at load); a fresh window picks it up via ui_intent.
    let _ = app.emit_to(dsh::SPLASH_LABEL, "dsh://ui-intent", panel.to_string());
    dsh::ensure_splash(app);
}

/// Set the Dock icon at runtime: dev binaries carry no app bundle, so the
/// icon otherwise stays the generic executable one until M5 packaging.
#[cfg(target_os = "macos")]
fn apply_dock_icon() {
    use objc2::rc::Retained;
    use objc2::AnyThread;
    use objc2_app_kit::{NSApplication, NSImage};
    use objc2_foundation::{MainThreadMarker, NSData};

    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let ns_app = NSApplication::sharedApplication(mtm);
    // Dock shows the 80%-transparent app icon (same visual as the bundle
    // .icns); the menu bar tray uses the solid variant (tray-icon-32.png).
    let bytes: &'static [u8] = include_bytes!("../icons/app-icon.png");
    let data: Retained<NSData> = unsafe {
        NSData::initWithBytes_length(
            NSData::alloc(),
            bytes.as_ptr().cast(),
            bytes.len() as objc2_foundation::NSUInteger,
        )
    };
    let Some(image) = NSImage::initWithData(NSImage::alloc(), &data) else {
        return;
    };
    unsafe {
        ns_app.setApplicationIconImage(Some(&image));
    }
}

/// Show the dsh main window, or the shell window when there is none yet.
fn show_main_or_splash(app: &AppHandle) {
    if let Some(w) = app.get_webview_window(dsh::MAIN_LABEL) {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
    } else {
        dsh::ensure_splash(app);
    }
}

/// Shared handler for the app menu and the tray menu.
fn menu_action(app: &AppHandle, id: &str) {
    println!("[dsh] menu action: {id}");
    match id {
        "menu-check-update" => open_shell_panel(app, "update"),
        "menu-install-plugin" => open_shell_panel(app, "plugin"),
        "menu-settings" => open_shell_panel(app, "settings"),
        "menu-open-logs" => {
            use tauri_plugin_opener::OpenerExt;
            if let Ok(data) = app.path().app_data_dir() {
                let logs = data.join("logs");
                let _ = std::fs::create_dir_all(&logs);
                let _ = app.opener().open_path(logs.to_string_lossy().to_string(), None::<&str>);
            }
        }
        "menu-restart-dsh" => {
            let shared = app.state::<AppState>().0.clone();
            let _ = dsh::start(app, &shared);
        }
        "menu-tray-show" => show_main_or_splash(app),
        "menu-quit" => app.exit(0),
        _ => {}
    }
}

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            // A second launch: focus the existing window instead of
            // starting a duplicate app + dsh child.
            println!("[dsh] single-instance: second launch detected, focusing");
            show_main_or_splash(app);
        }))
        .manage(AppState(Arc::new(dsh::Shared::default())))
        .invoke_handler(tauri::generate_handler![
            dsh_status,
            dsh_restart,
            app_quit,
            settings_get,
            settings_set,
            runtime_bootstrap,
            dsh_plugin,
            ui_intent,
            update::update_check,
            update::update_apply,
            update::update_list_versions,
            update::update_switch,
            update::update_history_list,
            close_choice,
            dsh_plugin_offline,
            runtime_reset,
            dsh_home_info,
            toast_clicked,
            toast_current
        ])
        .setup(|app| {
            let shared = app.state::<AppState>().0.clone();

            // Replace the default SIGTERM/SIGINT disposition so an external
            // kill still cleans up the dsh child.
            dsh::install_term_handler();

            // Branded Dock icon even for the unbundled dev binary.
            #[cfg(target_os = "macos")]
            apply_dock_icon();

            // System notification permission (macOS prompts once; the toast
            // banner stays as the fallback until/unless granted).
            #[cfg(target_os = "macos")]
            listener::sysnotify::request_permission(app.handle());

            // Persist default settings on first run.
            let settings = runtime::load_settings(app.handle());
            let _ = runtime::save_settings(app.handle(), &settings);

            // App menu: manual update check, plugin install, restart, quit.
            {
                let check = MenuItemBuilder::with_id("menu-check-update", "检查更新…")
                    .build(app)?;
                let plugin = MenuItemBuilder::with_id("menu-install-plugin", "安装插件…")
                    .build(app)?;
                let restart = MenuItemBuilder::with_id("menu-restart-dsh", "重启 dsh")
                    .build(app)?;
                let settings = MenuItemBuilder::with_id("menu-settings", "设置…")
                    .accelerator("CmdOrCtrl+,")
                    .build(app)?;
                let logs = MenuItemBuilder::with_id("menu-open-logs", "打开日志目录…")
                    .build(app)?;
                let quit = MenuItemBuilder::with_id("menu-quit", "退出 dsh desktop")
                    .accelerator("CmdOrCtrl+Q")
                    .build(app)?;
                let submenu = SubmenuBuilder::new(app, "dsh desktop")
                    .items(&[&check, &plugin, &restart, &settings, &logs, &quit])
                    .build()?;
                // Standard Edit menu: without it WKWebView never receives
                // Cmd+C/V/X/A (paste/copy are broken in the dsh GUI).
                let edit = SubmenuBuilder::new(app, "编辑")
                    .item(&PredefinedMenuItem::undo(app, None)?)
                    .item(&PredefinedMenuItem::redo(app, None)?)
                    .separator()
                    .item(&PredefinedMenuItem::cut(app, None)?)
                    .item(&PredefinedMenuItem::copy(app, None)?)
                    .item(&PredefinedMenuItem::paste(app, None)?)
                    .item(&PredefinedMenuItem::select_all(app, None)?)
                    .build()?;
                let menu = MenuBuilder::new(app).items(&[&submenu, &edit]).build()?;
                app.set_menu(menu)?;
                app.on_menu_event(|app, event| menu_action(app, event.id().as_ref()));
            }

            // Tray icon (menu bar): the background-mode anchor. Without an
            // app bundle the dev binary has no branded Dock icon, so the
            // tray is the reliable way back from 后台运行 in both dev and
            // packaged runs.
            {
                let show = MenuItemBuilder::with_id("menu-tray-show", "显示窗口").build(app)?;
                let check = MenuItemBuilder::with_id("menu-check-update", "检查更新…").build(app)?;
                let plugin = MenuItemBuilder::with_id("menu-install-plugin", "安装插件…").build(app)?;
                let settings_item = MenuItemBuilder::with_id("menu-settings", "设置…").build(app)?;
                let logs_item = MenuItemBuilder::with_id("menu-open-logs", "打开日志目录…").build(app)?;
                let quit = MenuItemBuilder::with_id("menu-quit", "退出 dsh desktop").build(app)?;
                let tray_menu = MenuBuilder::new(app)
                    .items(&[&show, &check, &plugin, &settings_item, &logs_item, &quit])
                    .build()?;
                let tray = TrayIconBuilder::with_id("main-tray")
                    .icon(tauri::image::Image::from_bytes(include_bytes!(
                        "../icons/tray-icon-32.png"
                    ))?)
                    // The tray icon is a white silhouette on transparency:
                    // template mode lets macOS render it monochrome and adapt
                    // it to the light/dark menu bar automatically.
                    .icon_as_template(true)
                    .tooltip("dsh desktop")
                    .menu(&tray_menu)
                    .show_menu_on_left_click(true)
                    .on_menu_event(|app, event| menu_action(app, event.id().as_ref()))
                    .build(app)?;
                // Keep the tray alive for the whole app lifetime.
                app.manage(tray);
            }

            // Reap a dsh child orphaned by a previous run (e.g. SIGKILLed app).
            dsh::reap_orphan(app.handle());

            // Clean exit on external SIGTERM/SIGINT: a plain libc handler sets
            // a flag (async-signal-safe) and a watcher thread performs the
            // child cleanup + exit. (ctrlc's handler proved unreliable inside
            // the macOS app: the signal never reached it.)
            let slot = shared.slot.clone();
            std::thread::spawn(move || loop {
                if dsh::term_flag() {
                    println!("[dsh] SIGTERM/SIGINT: stopping child, exiting");
                    dsh::stop(&slot);
                    std::process::exit(0);
                }
                std::thread::sleep(std::time::Duration::from_millis(150));
            });

            // No splash window on a fast start: the main window appears
            // directly. Only slow starts (>1.2s), first-run installs and
            // failures bring the shell window up.
            let shared_slow = shared.clone();
            let slow_app = app.handle().clone();
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(1200));
                let still_starting =
                    matches!(*shared_slow.phase.lock().unwrap(), dsh::Phase::Starting);
                if still_starting {
                    dsh::ensure_splash(&slow_app);
                }
            });
            let _ = dsh::start(app.handle(), &shared);

            // Auto-update scheduler (respects settings.autoUpdate).
            update::spawn_scheduler(app.handle().clone(), shared.clone());
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building dsh desktop")
        .run(|app, event| match event {
            RunEvent::ExitRequested { .. } => {
                let shared = app.state::<AppState>();
                dsh::stop_graceful(&shared.0.slot);
                dsh::remove_pidfile(app);
            }
            RunEvent::Exit => {
                let shared = app.state::<AppState>();
                dsh::stop_graceful(&shared.0.slot);
                dsh::remove_pidfile(app);
            }
            // Dock icon click while backgrounded: restore the main window.
            #[cfg(target_os = "macos")]
            RunEvent::Reopen { .. } => show_main_or_splash(app),
            _ => {}
        });
}
