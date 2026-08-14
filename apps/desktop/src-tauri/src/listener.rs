//! Non-invasive event listener (supplementary requirement #2).
//!
//! The shell attaches as an ADDITIONAL consumer to the dsh host's public
//! downlinks (`/api/events.mux` and `/api/events.host` over WebSocket) — the
//! same streams the web GUI reads. The host fan-outs every frame to ALL
//! connected consumers and replays pending entries on open, so observing from
//! here changes nothing for the GUI. No dsh internals are modified: we only
//! speak the documented wire protocol (downlink-only; the server closes the
//! socket if a client ever sends upstream traffic).
//!
//! Mapped events:
//! - `approval/requested` → permission-elevation request (badge + notification + focus)
//! - `question/requested` → ask-user questions            (badge + notification + focus)
//! - `approval/resolved` / `question/resolved`            (clear badge)
//! - `host/agent-error`  → agent error                    (badge + notification + focus)
//! - `stream/error`      → stream failure                 (notification)

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use serde::Serialize;
use serde_json::Value;
use tauri::{AppHandle, Emitter, Manager, WebviewUrl, WebviewWindowBuilder};

pub const MUX_PATH: &str = "/api/events.mux";
pub const HOST_PATH: &str = "/api/events.host";

#[derive(Debug, serde::Deserialize)]
struct Frame {
    #[serde(rename = "type")]
    kind: String,
    #[serde(flatten)]
    extra: Value,
}

#[derive(Debug, serde::Deserialize)]
struct Envelope {
    payload: Frame,
}

/// macOS system notifications via UNUserNotificationCenter (the modern
/// API — notification-center banners, non-blocking, web-notification-like).
/// The in-app toast banner is the guaranteed channel on every event; the
/// system banner is best-effort, attempted only when the live authorization
/// status allows it. Every decision is appended to `appdata/logs/notify.log`
/// so notification failures are diagnosable without a debugger.
#[cfg(target_os = "macos")]
pub mod sysnotify {
    use std::ptr::NonNull;

    use block2::RcBlock;
    use objc2::runtime::Bool;
    use objc2_foundation::{NSError, NSString};
    use objc2_user_notifications::{
        UNAuthorizationOptions, UNMutableNotificationContent, UNNotificationRequest,
        UNNotificationSettings, UNUserNotificationCenter,
    };

    /// UNUserNotificationCenter raises an uncaught NSException when the
    /// process has no app bundle (bundleProxyForCurrentProcess is nil) —
    /// i.e. for unbundled dev binaries. Only engage the system path inside
    /// a real .app bundle; the toast banner covers everything else.
    fn in_app_bundle() -> bool {
        let Ok(exe) = std::env::current_exe() else {
            return false;
        };
        let segs: Vec<String> = exe
            .components()
            .map(|c| c.as_os_str().to_string_lossy().to_string())
            .collect();
        segs.windows(2).any(|w| w[0] == "Contents" && w[1] == "MacOS")
    }

    /// UNAuthorizationStatus value → readable name (0 notDetermined, 1 denied,
    /// 2 authorized, 3 provisional, 4 ephemeral, -1 unbundled/unknown).
    pub fn status_name(s: i64) -> &'static str {
        match s {
            0 => "notDetermined",
            1 => "denied",
            2 => "authorized",
            3 => "provisional",
            4 => "ephemeral",
            _ => "unknown",
        }
    }

    /// Ask macOS for notification permission. The system prompt appears only
    /// when the status is still notDetermined; a previous deny/grant is
    /// answered silently and re-requests never re-prompt (that is macOS
    /// policy, not something code can override).
    pub fn request_permission(app: &tauri::AppHandle) {
        if !in_app_bundle() {
            super::diag(app, "sysnotify: unbundled dev binary, system notifications unavailable");
            return;
        }
        let center = UNUserNotificationCenter::currentNotificationCenter();
        let app = app.clone();
        let handler = RcBlock::new(move |granted: Bool, _err: *mut NSError| {
            super::diag(
                &app,
                &format!("sysnotify: authorization request answered: granted={}", granted.as_bool()),
            );
        });
        center.requestAuthorizationWithOptions_completionHandler(
            UNAuthorizationOptions::Alert | UNAuthorizationOptions::Sound | UNAuthorizationOptions::Badge,
            &handler,
        );
    }

    /// Read the live authorization status and pass its numeric value to `f`.
    /// Passes -1 for unbundled binaries (no bundle → no notification center).
    fn with_status(f: impl Fn(i64) + Send + 'static) {
        if !in_app_bundle() {
            f(-1);
            return;
        }
        let center = UNUserNotificationCenter::currentNotificationCenter();
        let block = RcBlock::new(move |settings: NonNull<UNNotificationSettings>| {
            let status = unsafe { settings.as_ptr().as_ref() }
                .map(|s| s.authorizationStatus().0 as i64)
                .unwrap_or(-1);
            f(status);
        });
        center.getNotificationSettingsWithCompletionHandler(&block);
    }

    /// Post the system banner; `on_error` receives the NSError (non-nil when
    /// the request was rejected, e.g. not authorized).
    pub fn send(title: &str, body: &str, on_error: RcBlock<dyn Fn(*mut NSError)>) {
        if !in_app_bundle() {
            return;
        }
        let center = UNUserNotificationCenter::currentNotificationCenter();
        let content = UNMutableNotificationContent::new();
        content.setTitle(&NSString::from_str(title));
        content.setBody(&NSString::from_str(body));
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let id = NSString::from_str(&format!("dsh-toast-{nanos}"));
        let request = UNNotificationRequest::requestWithIdentifier_content_trigger(&id, &content, None);
        center.addNotificationRequest_withCompletionHandler(&request, Some(&on_error));
    }

    /// Best-effort system banner keyed off the LIVE authorization status:
    /// authorized/provisional → send (delivery errors are logged);
    /// notDetermined → re-request permission (the startup request may have
    /// run before the app was fully active, which macOS can silently drop);
    /// denied/unknown → skip and log. The in-app toast already covered the
    /// event, so this channel is purely additive.
    pub fn notify_if_authorized(app: &tauri::AppHandle, title: &str, body: &str) {
        let app = app.clone();
        let title = title.to_string();
        let body = body.to_string();
        with_status(move |status| {
            super::diag(
                &app,
                &format!("sysnotify: status={status} ({})", status_name(status)),
            );
            match status {
                2 | 3 => {
                    let app_err = app.clone();
                    let on_error = RcBlock::new(move |err: *mut NSError| {
                        let msg = unsafe { err.as_ref() }
                            .map(|e| e.localizedDescription().to_string())
                            .unwrap_or_else(|| "unknown".to_string());
                        super::diag(&app_err, &format!("sysnotify: delivery error: {msg}"));
                    });
                    send(&title, &body, on_error);
                }
                0 => {
                    super::diag(&app, "sysnotify: notDetermined, re-requesting authorization");
                    request_permission(&app);
                }
                _ => {}
            }
        });
    }
}

/// Event entry point: the in-app toast banner always shows (guaranteed,
/// permission-free channel); on macOS a best-effort system banner is added
/// on top when the notification authorization allows it.
pub fn notify_event(app: &AppHandle, shared: &Arc<crate::dsh::Shared>, title: &str, body: &str) {
    diag(app, &format!("notify: {title} :: {body}"));
    toast(app, shared, title, body);
    #[cfg(target_os = "macos")]
    sysnotify::notify_if_authorized(app, title, body);
}

/// Best-effort persistent diagnostic line: `println!` plus an append to
/// `appdata/logs/notify.log` (the app's stdout is lost for GUI-launched
/// bundles, so the file is the readable record).
pub fn diag(app: &AppHandle, msg: &str) {
    println!("[dsh] {msg}");
    let Ok(dir) = app.path().app_data_dir() else {
        return;
    };
    let logs = dir.join("logs");
    let _ = std::fs::create_dir_all(&logs);
    let path = logs.join("notify.log");
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        use std::io::Write;
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let _ = writeln!(f, "[{secs}] {msg}");
    }
}

pub const TOAST_LABEL: &str = "toast";
static TOAST_SEQ: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Serialize)]
struct ToastPayload {
    title: String,
    body: String,
}

/// Non-blocking, web-notification-style banner in the top-right corner:
/// always-on-top but never steals focus, auto-hides after 6s, and focuses
/// the main window only when the user clicks it.
pub fn toast(app: &AppHandle, shared: &Arc<crate::dsh::Shared>, title: &str, body: &str) {
    println!("[dsh] alert: {title} :: {body}");
    diag(app, &format!("toast: showing banner ({title})"));
    // Store the payload first: a freshly created toast window loads the
    // page AFTER the emit, so its UI pulls the latest payload on load.
    if let Ok(mut slot) = shared.toast.lock() {
        *slot = Some((title.to_string(), body.to_string()));
    }
    ensure_toast(app);
    let seq = TOAST_SEQ.fetch_add(1, Ordering::SeqCst) + 1;
    let _ = app.emit_to(
        TOAST_LABEL,
        "dsh://toast",
        ToastPayload {
            title: title.to_string(),
            body: body.to_string(),
        },
    );
    // The toast window persists after its 6s auto-hide (or a click-dismiss)
    // but stays hidden forever — every event after the first would update
    // text in an invisible window. Re-show it on each event; doing it after
    // the emit means the fresh payload is what renders. A brand-new window
    // created by ensure_toast is visible by default and may not exist yet
    // when this line runs, so both branches are safe (show is idempotent
    // and never steals focus on macOS: it sets visibility, not key status).
    if let Some(w) = app.get_webview_window(TOAST_LABEL) {
        let _ = w.show();
    }
    let app = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(6));
        if TOAST_SEQ.load(Ordering::SeqCst) != seq {
            return; // superseded by a newer toast
        }
        if let Some(w) = app.get_webview_window(TOAST_LABEL) {
            let _ = w.hide();
        }
    });
}

fn ensure_toast(app: &AppHandle) {
    if app.get_webview_window(TOAST_LABEL).is_some() {
        return;
    }
    let app = app.clone();
    let _ = app.clone().run_on_main_thread(move || {
        let (tw, th) = (400.0f64, 128.0f64);
        let (mw, _mh) = app
            .primary_monitor()
            .ok()
            .flatten()
            .map(|m| (m.size().width as f64, m.size().height as f64))
            .unwrap_or((1920.0, 1080.0));
        let built = WebviewWindowBuilder::new(&app, TOAST_LABEL, WebviewUrl::App("toast.html".into()))
            .title("dsh desktop")
            .inner_size(tw, th)
            .position(mw - tw - 24.0, 24.0)
            .decorations(false)
            .resizable(false)
            .always_on_top(true)
            .skip_taskbar(true)
            .focused(false)
            .background_color(tauri::utils::config::Color(20, 22, 32, 255))
            .build();
        if let Err(e) = built {
            diag(&app, &format!("toast: create window failed: {e}"));
        } else {
            diag(&app, "toast: window created (visible by default)");
        }
    });
}

fn badge(app: &AppHandle, count: Option<i64>) {
    if let Some(w) = app.get_webview_window(crate::dsh::MAIN_LABEL) {
        let _ = w.set_badge_count(count);
    }
}

/// Map one downlink frame to user guidance. Also logs every frame type to the
/// ring so troubleshooting is possible without touching dsh.
pub fn handle_frame(app: &AppHandle, shared: &Arc<crate::dsh::Shared>, kind: &str, extra: &Value, stream: &str) {
    println!("[dsh] frame: {stream} {kind}");
    shared.push_line(format!("[listener] {stream}: {kind}"));
    match kind {
        "approval/requested" => {
            let tool = extra.get("toolName").and_then(|v| v.as_str()).unwrap_or("未知工具");
            let reason = extra
                .get("reason")
                .and_then(|v| v.as_str())
                .map(|r| format!("（{r}）"))
                .unwrap_or_default();
            badge(app, Some(1));
            notify_event(
                app,
                shared,
                "dsh 请求权限",
                &format!("dsh 想要执行「{tool}」{reason}，请在窗口中批准或拒绝。"),
            );
        }
        "question/requested" => {
            let n = extra
                .get("questions")
                .and_then(|v| v.as_array())
                .map(|a| a.len())
                .unwrap_or(1);
            badge(app, Some(1));
            notify_event(
                app,
                shared,
                "dsh 需要你的回答",
                &format!("dsh 向你提出了 {n} 个问题，请在窗口中回答。"),
            );
        }
        "approval/resolved" | "question/resolved" => {
            badge(app, None);
        }
        "host/agent-error" => {
            let msg = extra.get("message").and_then(|v| v.as_str()).unwrap_or("未知错误");
            badge(app, Some(1));
            notify_event(app, shared, "dsh 会话出错", msg);
        }
        "stream/error" => {
            let msg = extra
                .get("error")
                .and_then(|v| v.get("message"))
                .and_then(|v| v.as_str())
                .unwrap_or("未知错误");
            notify_event(app, shared, "dsh 事件流错误", msg);
        }
        _ => {}
    }
}

/// Start the two listener tasks for the child with generation `id`.
/// They stop themselves once a new generation replaces the child.
pub fn spawn(app: &AppHandle, shared: &Arc<crate::dsh::Shared>, id: u64, http_url: &str) {
    let ws_base = http_url.replace("http://", "ws://");
    for (name, path) in [("mux", MUX_PATH), ("host", HOST_PATH)] {
        let app = app.clone();
        let shared = shared.clone();
        let url = format!("{ws_base}{path}");
        tauri::async_runtime::spawn(async move {
            run_stream(&app, &shared, id, name, &url).await;
        });
    }
}

async fn run_stream(app: &AppHandle, shared: &Arc<crate::dsh::Shared>, id: u64, name: &str, url: &str) {
    loop {
        if shared.seq.load(Ordering::SeqCst) != id {
            return;
        }
        match tokio_tungstenite::connect_async(url).await {
            Ok((ws, _)) => {
                println!("[dsh] listener connected: {name}");
                let (_, mut read) = ws.split();
                while let Some(msg) = read.next().await {
                    if shared.seq.load(Ordering::SeqCst) != id {
                        return;
                    }
                    match msg {
                        Ok(tokio_tungstenite::tungstenite::Message::Text(text)) => {
                            if let Ok(env) = serde_json::from_str::<Envelope>(&text) {
                                handle_frame(app, shared, &env.payload.kind, &env.payload.extra, name);
                            }
                        }
                        Ok(tokio_tungstenite::tungstenite::Message::Close(_)) => break,
                        _ => {}
                    }
                }
            }
            Err(_) => {
                // Child gone or not yet listening; retry quietly.
            }
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}
