mod dsh;
mod listener;
mod runtime;
mod update;

use std::sync::Arc;

use dsh::Phase;
use tauri::menu::{MenuBuilder, MenuItemBuilder, SubmenuBuilder};
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

/// Run `dsh plugin --profile web <args…>` (e.g. ["add", "pkg"]) for the
/// managed runtime, streaming output lines to the shell UI.
#[tauri::command]
fn dsh_plugin(app: AppHandle, args: Vec<String>) -> Result<(), String> {
    let bin = match runtime::resolve_active_bin(&app) {
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
    std::thread::spawn(move || {
        let code = child.wait().map(|s| s.code().unwrap_or(-1)).unwrap_or(-1);
        let _ = app.emit("dsh://plugin-done", code);
    });
    Ok(())
}

/// The panel the shell UI should show on load, consumed once.
#[tauri::command]
fn ui_intent(state: tauri::State<'_, AppState>) -> Option<String> {
    state.0.ui_intent.lock().ok()?.take()
}

/// Open the shell window on a specific panel ("plugin" | "update").
fn open_shell_panel(app: &AppHandle, panel: &str) {
    if let Ok(mut intent) = app.state::<AppState>().0.ui_intent.lock() {
        *intent = Some(panel.to_string());
    }
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
    let bytes: &'static [u8] = include_bytes!("../icons/icon.png");
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
    match id {
        "menu-check-update" => open_shell_panel(app, "update"),
        "menu-install-plugin" => open_shell_panel(app, "plugin"),
        "menu-settings" => open_shell_panel(app, "settings"),
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
            close_choice
        ])
        .setup(|app| {
            let shared = app.state::<AppState>().0.clone();

            // Replace the default SIGTERM/SIGINT disposition so an external
            // kill still cleans up the dsh child.
            dsh::install_term_handler();

            // Branded Dock icon even for the unbundled dev binary.
            #[cfg(target_os = "macos")]
            apply_dock_icon();

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
                let quit = MenuItemBuilder::with_id("menu-quit", "退出 dsh desktop")
                    .accelerator("CmdOrCtrl+Q")
                    .build(app)?;
                let submenu = SubmenuBuilder::new(app, "dsh desktop")
                    .items(&[&check, &plugin, &restart, &settings, &quit])
                    .build()?;
                let menu = MenuBuilder::new(app).items(&[&submenu]).build()?;
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
                let quit = MenuItemBuilder::with_id("menu-quit", "退出 dsh desktop").build(app)?;
                let tray_menu = MenuBuilder::new(app)
                    .items(&[&show, &check, &plugin, &settings_item, &quit])
                    .build()?;
                let tray = TrayIconBuilder::with_id("main-tray")
                    .icon(tauri::image::Image::from_bytes(include_bytes!(
                        "../icons/32x32.png"
                    ))?)
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

            // Show the shell UI first so it can render the "starting" state.
            dsh::ensure_splash(app.handle());
            let _ = dsh::start(app.handle(), &shared);
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
