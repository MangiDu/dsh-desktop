//! dsh child-process supervisor (M1).
//!
//! Contract with the dsh CLI (verified 2026-08-13, see
//! `.intermediate/2026-08/13/001-tauri_wrapper/05-风险与待验证项.md`):
//! - spawn `node <dsh>/lib/bin.js web --port 0` (OS-assigned port, no conflicts)
//! - stdout handshake line: `dsh web: http://127.0.0.1:<port>`
//!   (take the FIRST URL; a `(LAN: …)` suffix, if it ever appears, is ignored)
//! - SIGTERM exits the process tree cleanly

use std::collections::VecDeque;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, Url, WebviewUrl, WebviewWindowBuilder};


/// Async-signal-safe SIGTERM/SIGINT handler: only sets a flag.
static TERM_FLAG: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

extern "C" fn on_term_signal(_sig: libc::c_int) {
    TERM_FLAG.store(true, std::sync::atomic::Ordering::SeqCst);
}

/// Install the libc signal handlers (called once from setup).
pub fn install_term_handler() {
    unsafe {
        libc::signal(libc::SIGTERM, on_term_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGINT, on_term_signal as *const () as libc::sighandler_t);
    }
}

/// Whether a termination signal was received.
pub fn term_flag() -> bool {
    TERM_FLAG.load(std::sync::atomic::Ordering::SeqCst)
}

pub const STATE_EVENT: &str = "dsh://state";
pub const SPLASH_LABEL: &str = "splash";
pub const MAIN_LABEL: &str = "main";
const HANDSHAKE_PREFIX: &str = "dsh web: http://127.0.0.1:";
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);
const RING_CAP: usize = 400;

/// Passive page-side logger for the "second click" diagnosis: capture-phase
/// mousedown/mouseup/click with a compact target descriptor, printed to the
/// Web Inspector console. Read-only: it never touches page state or events.
const CLICK_LOG_SCRIPT: &str = r##"
(function () {
  function desc(t) {
    try {
      if (!t) return "?";
      var el = t;
      var s = (el.tagName || "?").toLowerCase();
      if (el.id) s += "#" + el.id;
      var c = String(el.className && el.className.baseVal !== undefined ? el.className.baseVal : el.className || "").trim();
      if (c) s += "." + c.split(/\s+/).slice(0, 3).join(".");
      return s;
    } catch (e) { return "?"; }
  }
  function log(kind, e) {
    try {
      console.log("[dsh-shell-click]", kind,
        "target=" + desc(e.target),
        "down=" + desc(window.__dshDownTarget),
        "detail=" + e.detail,
        "x=" + e.clientX, "y=" + e.clientY,
        "prev=" + (e.defaultPrevented ? "1" : "0"));
    } catch (err) {}
  }
  window.__dshScrolled = 0;
  document.addEventListener("scroll", function () { window.__dshScrolled++; }, true);
  ["mousedown", "click", "dblclick", "pointerdown", "pointercancel", "contextmenu"].forEach(function (k) {
    document.addEventListener(k, function (e) { log(k, e); }, true);
  });
  document.addEventListener("mousedown", function (e) {
    window.__dshDownNode = e.target;
    window.__dshDownTarget = e.target;
    window.__dshScrolled = 0;
    log("mousedown", e);
  }, true);
  document.addEventListener("pointerup", function (e) { log("pointerup", e); }, true);
  document.addEventListener("mouseup", function (e) {
    log("mouseup", e);
    try {
      console.log("[dsh-shell-click]", "same-node",
        "same=" + (e.target === window.__dshDownNode ? "1" : "0"),
        "connected=" + (window.__dshDownNode && window.__dshDownNode.isConnected ? "1" : "0"),
        "scrolled=" + window.__dshScrolled);
    } catch (err) {}
  }, true);
})();
"##;

/// Compensates for clicks lost to mid-gesture scrolls: the page scrolls
/// between mousedown and mouseup (the dsh sidebar scrolls when an item
/// gains focus/selection), the node under the cursor changes and WebKit
/// drops the click (proven by the same-node probe: same=0 scrolled=1).
/// If no click arrives within 90ms of the press, the cursor barely moved
/// and the pressed node is still attached, re-dispatch a click on it —
/// tightly gated so real drags, right/middle clicks and double-clicks
/// are untouched.
const CLICK_COMPENSATOR_SCRIPT: &str = r##"
(function () {
  var lastX = 0, lastY = 0;
  document.addEventListener("mousemove", function (e) {
    lastX = e.clientX; lastY = e.clientY;
  }, true);
  document.addEventListener("mousedown", function (e) {
    if (e.button !== 0) return;
    var node = e.target;
    var x = e.clientX, y = e.clientY;
    var gotClick = false;
    function mark() { gotClick = true; }
    document.addEventListener("click", mark, true);
    setTimeout(function () {
      document.removeEventListener("click", mark, true);
      if (gotClick) return;
      if (!node || !node.isConnected) return;
      if (Math.abs(lastX - x) >= 6 || Math.abs(lastY - y) >= 6) return;
      try {
        node.dispatchEvent(new MouseEvent("click", {
          bubbles: true, cancelable: true, view: window,
          detail: 1, button: 0, clientX: lastX, clientY: lastY
        }));
      } catch (err) {}
    }, 90);
  }, true);
})();
"##;

/// State broadcast to the shell UI (event `dsh://state` and `dsh_status`).
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "phase", rename_all = "camelCase")]
pub enum Phase {
    Starting,
    Bootstrapping { line: String },
    Ready { url: String },
    StartFailed { reason: String, log_tail: String, can_reset: bool },
    Exited { code: Option<i32>, log_tail: String, can_reset: bool },
}

impl Default for Phase {
    fn default() -> Self {
        Phase::Starting
    }
}

/// One supervised dsh child. Owned by [`Shared::slot`]; the waiter thread
/// reaps it, the kill paths send signals through it.
pub struct Supervisor {
    pub child: Child,
    pub pid: u32,
    pub id: u64,
}

/// Cross-thread shared state between commands, watcher threads and signals.
#[derive(Default)]
pub struct Shared {
    pub slot: Arc<Mutex<Option<Supervisor>>>,
    pub phase: Mutex<Phase>,
    pub ring: Mutex<VecDeque<String>>,
    pub seq: AtomicU64,
    /// Panel the shell UI should open on next load ("plugin", …).
    pub ui_intent: Mutex<Option<String>>,
    /// A runtime install/update is in progress (scheduler mutex).
    pub install_busy: AtomicBool,
    /// Consecutive start failures in this run (>=3 enables 重置运行时).
    pub fail_streak: AtomicU32,
    /// Latest toast payload (title, body) for the pull-on-load model.
    pub toast: Mutex<Option<(String, String)>>,
}

impl Shared {
    /// Append a line to the in-memory ring only (no log file context).
    pub fn push_line(&self, line: String) {
        if let Ok(mut ring) = self.ring.lock() {
            ring.push_back(line);
            while ring.len() > RING_CAP {
                ring.pop_front();
            }
        }
    }

    pub fn log_tail(&self) -> String {
        let ring = self.ring.lock().unwrap();
        let mut lines: Vec<&String> = ring.iter().filter(|l| !l.trim().is_empty()).collect();
        if lines.len() > 12 {
            lines = lines.split_off(lines.len() - 12);
        }
        lines
            .into_iter()
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn log_line(&self, file: &Arc<Mutex<Option<File>>>, line: String) {
        if let Ok(mut ring) = self.ring.lock() {
            ring.push_back(line.clone());
            while ring.len() > RING_CAP {
                ring.pop_front();
            }
        }
        if let Ok(mut f) = file.lock() {
            if let Some(f) = f.as_mut() {
                let _ = writeln!(f, "{line}");
                let _ = f.flush();
            }
        }
    }
}

fn app_data_dir(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .app_data_dir()
        .map_err(|e| format!("无法解析应用数据目录: {e}"))
}

fn pid_file_path(app: &AppHandle) -> Result<PathBuf, String> {
    Ok(app_data_dir(app)?.join("dsh-child.pid"))
}

/// Remove the pid file on clean exit (the orphan reaper tolerates stale files,
/// but we prefer not to leave them behind).
pub fn remove_pidfile(app: &AppHandle) {
    if let Ok(path) = pid_file_path(app) {
        let _ = fs::remove_file(path);
    }
}

fn check_node(app: &AppHandle) -> Result<String, String> {
    let (program, source) = crate::nodejs::node_program(app);
    let out = Command::new(&program)
        .arg("--version")
        .output()
        .map_err(|e| format!("找不到 node 运行时（捆绑缺失且 PATH 中无 node）: {e}"))?;
    if !out.status.success() {
        return Err("node --version 执行失败".to_string());
    }
    let version = String::from_utf8_lossy(&out.stdout).trim().to_string();
    match source {
        Some(dir) => println!("[dsh] node toolchain: bundled ({dir})"),
        None => println!("[dsh] node toolchain: system PATH"),
    }
    Ok(version)
}

fn open_log(app: &AppHandle) -> Result<(PathBuf, Arc<Mutex<Option<File>>>), String> {
    let logs = app_data_dir(app)?.join("logs");
    fs::create_dir_all(&logs).map_err(|e| format!("创建日志目录失败: {e}"))?;
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let path = logs.join(format!("dsh-{secs}.log"));
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .ok();
    Ok((path, Arc::new(Mutex::new(file))))
}

fn emit_state(app: &AppHandle, phase: Phase) {
    if let Err(e) = app.emit(STATE_EVENT, &phase) {
        eprintln!("[dsh] emit state failed: {e}");
    }
}

pub fn ensure_splash(app: &AppHandle) {
    println!(
        "[dsh] ensure_splash: exists={}",
        app.get_webview_window(SPLASH_LABEL).is_some()
    );
    // The shell window lives hidden for the whole app lifetime (ready
    // transition and red X both hide it); reopening is just show+focus —
    // instant, no page reload, no white flash.
    if let Some(window) = app.get_webview_window(SPLASH_LABEL) {
        let _ = window.show();
        let _ = window.set_focus();
        return;
    }
    let app = app.clone();
    if let Err(e) = app.clone().run_on_main_thread(move || {
        match WebviewWindowBuilder::new(&app, SPLASH_LABEL, WebviewUrl::App("index.html".into()))
            .title("dsh desktop")
            .title_bar_style(tauri::TitleBarStyle::Overlay)
            .hidden_title(true)
            .inner_size(520.0, 440.0)
            .min_inner_size(420.0, 380.0)
            .resizable(true)
            .center()
            .accept_first_mouse(true)
            .background_color(tauri::utils::config::Color(16, 17, 26, 255))
            .build()
        {
            Ok(window) => {
                // Red X hides instead of destroying (rebuilding would flash
                // white and reload on the next open).
                let handle = window.clone();
                window.on_window_event(move |event| {
                    if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                        api.prevent_close();
                        let _ = handle.hide();
                    }
                    if let tauri::WindowEvent::Focused(f) = event {
                        crate::listener::diag(&handle.app_handle(), &format!("focus: splash={f}"));
                    }
                });
                // Newly created windows can land behind the focused main
                // window; bring the shell window to the front explicitly.
                let _ = window.show();
                let _ = window.set_focus();
            }
            Err(e) => {
                eprintln!("[dsh] create splash window failed: {e}");
            }
        }
    }) {
        eprintln!("[dsh] run_on_main_thread failed: {e}");
    }
}

fn close_main(app: &AppHandle) {
    if let Some(w) = app.get_webview_window(MAIN_LABEL) {
        let _ = w.close();
    }
}

/// Start (or restart) the dsh child. Idempotent: kills any previous child.
pub fn start(app: &AppHandle, shared: &Arc<Shared>) -> Result<(), String> {
    let id = shared.seq.fetch_add(1, Ordering::SeqCst) + 1;

    // A previous child is always tied to a URL the main window no longer
    // serves after this start, so close the window first.
    close_main(app);
    stop_graceful(&shared.slot);

    *shared.phase.lock().unwrap() = Phase::Starting;
    println!("[dsh] start id={id}");
    emit_state(app, Phase::Starting);

    let bin = match crate::runtime::resolve_active_bin(app) {
        crate::runtime::Resolve::Ready(b) => b,
        crate::runtime::Resolve::NeedBootstrap => {
            // A first-run install takes minutes and needs its progress bar
            // visible from the start.
            ensure_splash(app);
            let settings = crate::runtime::load_settings(app);
            bootstrap_in_background(app, shared, settings);
            return Ok(());
        }
        crate::runtime::Resolve::Failed(reason) => {
            fail_start(app, shared, reason);
            return Err("runtime resolve failed".to_string());
        }
    };
    let node_version = match check_node(app) {
        Ok(v) => v,
        Err(reason) => {
            fail_start(app, shared, reason);
            return Err("node check failed".to_string());
        }
    };

    let (log_path, file) = match open_log(app) {
        Ok(v) => v,
        Err(reason) => {
            fail_start(app, shared, reason);
            return Err("open log failed".to_string());
        }
    };
    let _ = log_line_string(&file, &shared.ring, format!("[dsh] node {node_version}, bin {bin:?}, log {log_path:?}"));

    // The dsh child's cwd partitions its session history (~/.dsh/sessions).
    // Default to a dedicated, stable workspace so the desktop never opens
    // sessions another dsh instance (e.g. a terminal one) is writing; a
    // reading instance validating a live session log can otherwise report
    // "corrupt session log: seq gap" — an artifact of concurrent access.
    // Default anchor: ~/dsh-desktop-workspace (DSH_CWD overrides). Workspace
    // switching is dsh's own feature — its GUI groups sessions by directory
    // and ships a native directory picker, so the desktop stays out of it.
    let cwd = match std::env::var("DSH_CWD") {
        Ok(v) if !v.trim().is_empty() => std::path::PathBuf::from(v.trim()),
        _ => {
            let home = app
                .path()
                .home_dir()
                .unwrap_or_else(|_| std::path::PathBuf::from("."));
            let dir = home.join("dsh-desktop-workspace");
            let _ = std::fs::create_dir_all(&dir);
            dir
        }
    };

    let (node_program, _) = crate::nodejs::node_program(app);
    let mut cmd = Command::new(&node_program);
    cmd.arg(&bin).arg("web").arg("--port").arg("0").current_dir(&cwd);
    println!("[dsh] child cwd: {}", cwd.display());
    cmd.env("DSH_DESKTOP_CHILD", "1");
    if let Some(path) = crate::nodejs::enriched_path(app) {
        cmd.env("PATH", path);
    }
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            let reason = format!("spawn dsh 失败: {e}");
            let _ = log_line_string(&file, &shared.ring, format!("[dsh] {reason}"));
            fail_start(app, shared, reason);
            return Err("spawn failed".to_string());
        }
    };
    let pid = child.id();

    let app_data = match app_data_dir(app) {
        Ok(d) => d,
        Err(reason) => {
            let _ = child.kill();
            fail_start(app, shared, reason);
            return Err("app data dir failed".to_string());
        }
    };
    if let Err(e) = fs::create_dir_all(&app_data) {
        let _ = child.kill();
        fail_start(app, shared, format!("创建应用数据目录失败: {e}"));
        return Err("create app data dir failed".to_string());
    }
    let pid_path = match pid_file_path(app) {
        Ok(p) => p,
        Err(reason) => {
            let _ = child.kill();
            fail_start(app, shared, reason);
            return Err("pid file path failed".to_string());
        }
    };
    // pid file is best-effort (orphan reaper degrades gracefully without it)
    let _ = fs::write(&pid_path, format!("{pid}\n{id}\n"));
    let _ = log_line_string(&file, &shared.ring, format!("[dsh] spawned pid={pid} id={id}"));

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    *shared.slot.lock().unwrap() = Some(Supervisor { child, pid, id });

    let started = Instant::now();
    if let Some(out) = stdout {
        let app = app.clone();
        let shared = shared.clone();
        let file = file.clone();
        thread::spawn(move || {
            for line in BufReader::new(out).lines() {
                match line {
                    Ok(line) => {
                        shared.log_line(&file, line.clone());
                        if let Some(port) = parse_handshake(&line) {
                            on_ready(&app, &shared, id, port);
                        }
                    }
                    Err(_) => break,
                }
            }
        });
    }
    if let Some(err) = stderr {
        let shared = shared.clone();
        let file = file.clone();
        thread::spawn(move || {
            for line in BufReader::new(err).lines() {
                if let Ok(line) = line {
                    shared.log_line(&file, format!("[stderr] {line}"));
                }
            }
        });
    }
    let app = app.clone();
    let shared = shared.clone();
    thread::spawn(move || waiter(&app, &shared, id, started));

    Ok(())
}

fn log_line_string(file: &Arc<Mutex<Option<File>>>, ring: &Mutex<VecDeque<String>>, line: String) -> std::io::Result<()> {
    if let Ok(mut r) = ring.lock() {
        r.push_back(line.clone());
        while r.len() > RING_CAP {
            r.pop_front();
        }
    }
    if let Ok(mut f) = file.lock() {
        if let Some(f) = f.as_mut() {
            writeln!(f, "{line}")?;
            f.flush()?;
        }
    }
    Ok(())
}

/// Parse the handshake line; takes the first URL, ignores a `(LAN: …)` suffix.
fn parse_handshake(line: &str) -> Option<u16> {
    let rest = line.strip_prefix(HANDSHAKE_PREFIX)?;
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    digits.parse().ok()
}

fn on_ready(app: &AppHandle, shared: &Arc<Shared>, id: u64, port: u16) {
    if shared.seq.load(Ordering::SeqCst) != id {
        return;
    }
    let url = format!("http://127.0.0.1:{port}");
    *shared.phase.lock().unwrap() = Phase::Ready { url: url.clone() };
    println!("[dsh] ready: {url}");

    shared.fail_streak.store(0, Ordering::SeqCst);

    // A managed runtime that reached ready is the new last-known-good;
    // prune version dirs beyond current + lkg + two newest.
    if let Some(version) = crate::runtime::current_version(app) {
        crate::runtime::set_last_known_good(app, &version);
        crate::runtime::prune_versions(app);
    }

    let app2 = app.clone();
    let shared2 = shared.clone();
    let url2 = url.clone();
    if let Err(e) = app.run_on_main_thread(move || {
        let parsed = match Url::parse(&url2) {
            Ok(u) => u,
            Err(e) => {
                fail_start(&app2, &shared2, format!("握手行 URL 解析失败: {e}"));
                return;
            }
        };
        let shared3 = shared2.clone();
        match WebviewWindowBuilder::new(&app2, MAIN_LABEL, WebviewUrl::External(parsed))
            .title("dsh desktop")
            .inner_size(1280.0, 800.0)
            .min_inner_size(800.0, 600.0)
            .center()
            // First click delivers immediately even when the window is not
            // key (browser-like); without this the first click only focuses.
            .accept_first_mouse(true)
            // Diagnostics for the "second click" issue: make the page
            // inspectable from Safari's Develop menu, passively log
            // mousedown/mouseup/click with target descriptors to the
            // console, and compensate clicks lost to mid-gesture scrolls.
            .devtools(true)
            .initialization_script(CLICK_LOG_SCRIPT)
            .initialization_script(CLICK_COMPENSATOR_SCRIPT)
            .build()
        {
            Ok(window) => {
                // macOS: disable Force Touch pressure handling on the
                // WKWebView. A firm press on a word triggers the system
                // Look-Up gesture, which consumes the click (the mouseup
                // arrives with click count 0 and no click event follows):
                // the GUI then needs a second, lighter click. Browsers do
                // not implement Look-Up, which is why the web version
                // never shows this. Setting the pressure configuration to
                // nil disables all pressure behaviours (Apple docs).
                #[cfg(target_os = "macos")]
                {
                    let _ = window.as_ref().with_webview(|wv| {
                        let wk = wv.inner() as *mut objc2_web_kit::WKWebView;
                        unsafe {
                            (*wk).setPressureConfiguration(None);
                        }
                    });
                }
                // Close-button confirmation: fully quit, or keep running in
                // the background (hidden; restored via the Dock icon).
                let shared4 = shared3.clone();
                let window2 = window.clone();
                window.on_window_event(move |event| {
                    if let tauri::WindowEvent::Focused(f) = event {
                        crate::listener::diag(
                            &window2.app_handle(),
                            &format!("focus: main={f}"),
                        );
                    }
                    if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                        // Only guard the close while dsh is running; when dsh
                        // has exited, closing is just dismissing the window.
                        let ready =
                            matches!(*shared4.phase.lock().unwrap(), Phase::Ready { .. });
                        if !ready {
                            return;
                        }
                        api.prevent_close();
                        let app = window2.app_handle().clone();
                        // Remembered close behaviour wins; otherwise ask in
                        // the shell window (checkbox + buttons panel).
                        match crate::runtime::load_settings(&app).close_action.as_str() {
                            "quit" => app.exit(0),
                            "background" => {
                                let _ = window2.hide();
                            }
                            _ => {
                                if let Ok(mut intent) =
                                    app.state::<crate::AppState>().0.ui_intent.lock()
                                {
                                    *intent = Some("close-confirm".to_string());
                                }
                                let _ = app.emit_to(SPLASH_LABEL, "dsh://ui-intent", "close-confirm");
                                crate::dsh::ensure_splash(&app);
                            }
                        }
                    }
                });
                // Hide (not destroy) the shell window: menu panels reopen it
                // instantly instead of reloading the page.
                if let Some(s) = app2.get_webview_window(SPLASH_LABEL) {
                    let _ = s.hide();
                }
            }
            Err(e) => {
                eprintln!("[dsh] create main window failed: {e}");
                fail_start(&app2, &shared2, format!("创建主窗口失败: {e}"));
            }
        }
    }) {
        eprintln!("[dsh] run_on_main_thread failed: {e}");
    }
    emit_state(app, Phase::Ready { url: url.clone() });

    // Attach the non-invasive event listeners (permission requests, ask-user
    // questions, agent errors) to this child's public downlinks.
    crate::listener::spawn(app, shared, id, &url);
}

fn waiter(app: &AppHandle, shared: &Arc<Shared>, id: u64, started: Instant) {
    loop {
        let status = {
            let mut g = shared.slot.lock().unwrap();
            match g.as_mut() {
                Some(sup) if sup.id == id => match sup.child.try_wait() {
                    Ok(Some(st)) => Some(st.code()),
                    Ok(None) => None,
                    Err(e) => {
                        eprintln!("[dsh] try_wait failed: {e}");
                        Some(None)
                    }
                },
                _ => return, // replaced or reaped elsewhere — stale watcher
            }
        };
        if let Some(code) = status {
            on_exit(app, shared, id, code);
            return;
        }
        if started.elapsed() > HANDSHAKE_TIMEOUT {
            let still_starting = matches!(*shared.phase.lock().unwrap(), Phase::Starting);
            if still_starting {
                stop_graceful(&shared.slot);
                fail_start(
                    app,
                    shared,
                    format!("dsh 在 {}s 内未打印就绪握手行", HANDSHAKE_TIMEOUT.as_secs()),
                );
                return;
            }
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn on_exit(app: &AppHandle, shared: &Arc<Shared>, id: u64, code: Option<i32>) {
    if shared.seq.load(Ordering::SeqCst) != id {
        return;
    }
    // Auto-rollback: a managed runtime that died BEFORE the ready handshake
    // (e.g. right after an update) falls back to the last version that did
    // reach ready, then restarts with it.
    let was_starting = matches!(*shared.phase.lock().unwrap(), Phase::Starting);
    if was_starting {
        if let Some(current) = crate::runtime::current_version(app) {
            if let Some(good) = crate::runtime::last_known_good(app) {
                if current != good && crate::runtime::version_complete(app, &good) {
                    if crate::runtime::switch_version(app, &good).is_ok() {
                        println!("[dsh] auto-rollback: {current} -> {good}");
                        crate::update::record_update(
                            app,
                            &current,
                            &good,
                            "rolled-back",
                            Some(format!("新版本启动失败（退出码 {code:?}），已自动回退")),
                        );
                        if let Ok(mut g) = shared.slot.lock() {
                            *g = None;
                        }
                        let _ = start(app, shared);
                        return;
                    }
                }
            }
        }
    }
    if let Ok(mut g) = shared.slot.lock() {
        if let Some(sup) = g.as_ref() {
            if sup.id == id {
                if let Ok(path) = pid_file_path(app) {
                    let _ = fs::remove_file(path);
                }
            }
        }
        *g = None;
    }
    let streak = if was_starting {
        shared.fail_streak.fetch_add(1, Ordering::SeqCst) + 1
    } else {
        shared.fail_streak.load(Ordering::SeqCst)
    };
    println!("[dsh] exited: code={code:?} (streak {streak})");
    let phase = Phase::Exited {
        code,
        log_tail: shared.log_tail(),
        can_reset: streak >= 3,
    };
    *shared.phase.lock().unwrap() = phase.clone();
    close_main(app);
    let app2 = app.clone();
    let phase2 = phase.clone();
    if let Err(e) = app.run_on_main_thread(move || {
        ensure_splash(&app2);
        emit_state(&app2, phase2);
    }) {
        eprintln!("[dsh] run_on_main_thread failed: {e}");
    }
}

/// Kick off a runtime install on a background thread, streaming progress to
/// the shell UI, then start the child with the fresh runtime.
fn bootstrap_in_background(app: &AppHandle, shared: &Arc<Shared>, settings: crate::runtime::Settings) {
    let initial = Phase::Bootstrapping {
        line: "正在安装 dsh 运行时（首次约 1 分钟）…".to_string(),
    };
    *shared.phase.lock().unwrap() = initial.clone();
    emit_state(app, initial);

    let app2 = app.clone();
    let shared2 = shared.clone();
    shared2.install_busy.store(true, Ordering::SeqCst);
    thread::spawn(move || {
        let app3 = app2.clone();
        let shared3 = shared2.clone();
        let on_line = move |line: &str| {
            println!("[dsh] bootstrap: {line}");
            shared3.push_line(line.to_string());
            let phase = Phase::Bootstrapping { line: line.to_string() };
            *shared3.phase.lock().unwrap() = phase.clone();
            emit_state(&app3, phase);
        };
        let spec = settings.channel.clone();
        match crate::runtime::bootstrap(&app2, &settings, &spec, &on_line) {
            Ok(version) => {
                println!("[dsh] bootstrap done: {version}");
                if let Err(e) = start(&app2, &shared2) {
                    eprintln!("[dsh] post-bootstrap start failed: {e}");
                }
            }
            Err(reason) => {
                fail_start(&app2, &shared2, format!("dsh 运行时安装失败: {reason}"));
            }
        }
        shared2.install_busy.store(false, Ordering::SeqCst);
    });
}

/// Command entry: retry a failed bootstrap.
pub fn bootstrap_now(app: &AppHandle, shared: &Arc<Shared>) -> Result<(), String> {
    ensure_splash(app);
    let settings = crate::runtime::load_settings(app);
    bootstrap_in_background(app, shared, settings);
    Ok(())
}

fn fail_start(app: &AppHandle, shared: &Arc<Shared>, reason: String) {
    let streak = shared.fail_streak.fetch_add(1, Ordering::SeqCst) + 1;
    eprintln!("[dsh] start failed (streak {streak}): {reason}");
    let phase = Phase::StartFailed {
        reason: reason.clone(),
        log_tail: shared.log_tail(),
        can_reset: streak >= 3,
    };
    *shared.phase.lock().unwrap() = phase.clone();
    let app2 = app.clone();
    let phase2 = phase.clone();
    if let Err(e) = app.run_on_main_thread(move || {
        ensure_splash(&app2);
        emit_state(&app2, phase2);
    }) {
        eprintln!("[dsh] run_on_main_thread failed: {e}");
    }
}

/// SIGTERM, poll up to 3s, then SIGKILL + reap. Safe to call repeatedly.
pub fn stop_graceful(slot: &Arc<Mutex<Option<Supervisor>>>) {
    let pid = slot
        .lock()
        .ok()
        .and_then(|g| g.as_ref().map(|s| s.pid));
    if let Some(pid) = pid {
        unsafe {
            libc::kill(pid as i32, libc::SIGTERM);
        }
        for _ in 0..60 {
            let done = slot
                .lock()
                .ok()
                .and_then(|mut g| g.as_mut().map(|s| s.child.try_wait().ok().flatten().is_some()))
                .unwrap_or(true);
            if done {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
    }
    stop(slot);
}

/// Hard kill + reap.
pub fn stop(slot: &Arc<Mutex<Option<Supervisor>>>) {
    if let Ok(mut g) = slot.lock() {
        if let Some(sup) = g.as_mut() {
            let _ = sup.child.kill();
            let _ = sup.child.wait();
        }
        *g = None;
    }
}

/// Reap a leftover dsh child from a previous app run (e.g. the app was
/// SIGKILLed). Uses the pid file plus `ps -E` marker check to avoid killing
/// an unrelated process that reused the pid.
pub fn reap_orphan(app: &AppHandle) {
    let Ok(pid_path) = pid_file_path(app) else {
        return;
    };
    let Ok(content) = fs::read_to_string(&pid_path) else {
        return;
    };
    let pid: u32 = match content.lines().next().and_then(|l| l.trim().parse().ok()) {
        Some(p) if p > 0 => p,
        _ => {
            let _ = fs::remove_file(&pid_path);
            return;
        }
    };
    if is_our_dsh(pid) {
        println!("[dsh] reaping orphan pid={pid}");
        unsafe {
            libc::kill(pid as i32, libc::SIGTERM);
        }
        for _ in 0..60 {
            if !is_our_dsh(pid) {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        unsafe {
            libc::kill(pid as i32, libc::SIGKILL);
        }
    }
    let _ = fs::remove_file(&pid_path);
}

#[cfg(target_os = "macos")]
fn is_our_dsh(pid: u32) -> bool {
    let Ok(out) = Command::new("ps")
        .args(["-p", &pid.to_string(), "-E", "-o", "command="])
        .output()
    else {
        return false;
    };
    let s = String::from_utf8_lossy(&out.stdout);
    s.contains("bin.js") && s.contains("DSH_DESKTOP_CHILD=1")
}

#[cfg(not(target_os = "macos"))]
fn is_our_dsh(pid: u32) -> bool {
    // Best effort on other platforms (Linux /proc check comes with M4/M5).
    let Ok(out) = Command::new("ps").args(["-p", &pid.to_string(), "-o", "command="]).output() else {
        return false;
    };
    let s = String::from_utf8_lossy(&out.stdout);
    s.contains("bin.js") && s.contains("--port")
}
