mod dsh;
mod listener;
mod runtime;

use std::sync::Arc;

use dsh::Phase;
use tauri::{AppHandle, Manager, RunEvent};

/// Managed state: everything the watcher threads and commands share.
struct AppState(Arc<dsh::Shared>);

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
            runtime_bootstrap
        ])
        .setup(|app| {
            let shared = app.state::<AppState>().0.clone();

            // Replace the default SIGTERM/SIGINT disposition so an external
            // kill still cleans up the dsh child.
            dsh::install_term_handler();

            // Persist default settings on first run.
            let settings = runtime::load_settings(app.handle());
            let _ = runtime::save_settings(app.handle(), &settings);

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
            RunEvent::Reopen { .. } => {
                if let Some(w) = app.get_webview_window(dsh::MAIN_LABEL) {
                    let _ = w.show();
                    let _ = w.unminimize();
                    let _ = w.set_focus();
                } else {
                    dsh::ensure_splash(app);
                }
            }
            _ => {}
        });
}
