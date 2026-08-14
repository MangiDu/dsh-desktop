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
pub fn toast(app: &AppHandle, title: &str, body: &str) {
    println!("[dsh] alert: {title} :: {body}");
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
            eprintln!("[dsh] create toast window failed: {e}");
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
            toast(
                app,
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
            toast(
                app,
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
            toast(app, "dsh 会话出错", msg);
        }
        "stream/error" => {
            let msg = extra
                .get("error")
                .and_then(|v| v.get("message"))
                .and_then(|v| v.as_str())
                .unwrap_or("未知错误");
            toast(app, "dsh 事件流错误", msg);
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
