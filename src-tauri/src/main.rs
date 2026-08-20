// Prevents an extra console window on Windows in release builds.
// DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::io::{BufRead, BufReader};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::webview::WebviewWindowBuilder;
use tauri::{Emitter, Listener, Manager, RunEvent, Url, WebviewUrl, WindowEvent};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

/// CREATE_NO_WINDOW: spawn the console child without flashing a black window.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Holds the spawned `dsh web` backend so we can kill it on exit. Empty when we
/// have no backend of our own — either none started yet, or we are reusing one
/// somebody else owns (a `dsh web` already serving in a browser tab).
struct Backend(Mutex<Option<Child>>);

/// Guards against two boot sequences running at once (double-clicked retry, or a
/// retry landing while the install-triggered boot is still going).
struct BootLock(AtomicBool);

/// Last status pushed to the boot page, so a page that loads (or reloads) after
/// the event fired can still ask for it. Without this the window could sit on
/// the splash forever having missed the one event that mattered.
struct Status(Mutex<serde_json::Value>);

/// The package that provides the `dsh` command, and the site to send users to
/// when they have no Node.js at all. Both are constants: `install_dsh` and
/// `open_node_site` take no arguments from the page, so it cannot talk us into
/// installing an arbitrary package or opening an arbitrary URL.
const DSH_PACKAGE: &str = "@deepseek-ai/dsh";
const NODE_SITE: &str = "https://nodejs.org/";

/// How long we wait for `dsh web` to print its URL before calling it a failure.
const BACKEND_READY_TIMEOUT: Duration = Duration::from_secs(90);

/// Paint the native title bar to match dsh's theme: dark -> #0f1115 with light
/// text, light -> #f9fafb with dark text. Called on startup and again whenever
/// the web page flips its theme (see THEME_WATCH_JS).
#[cfg(windows)]
fn theme_titlebar(window: &tauri::WebviewWindow, dark: bool) {
    use windows_sys::Win32::Graphics::Dwm::{
        DwmSetWindowAttribute, DWMWA_BORDER_COLOR, DWMWA_CAPTION_COLOR,
        DWMWA_TEXT_COLOR, DWMWA_USE_IMMERSIVE_DARK_MODE,
    };

    let hwnd = match window.hwnd() {
        Ok(h) => h.0,
        Err(_) => return,
    };

    // COLORREF (0x00BBGGRR)
    let bg: u32 = if dark { 0x0015_110F } else { 0x00FB_FAF9 }; // #0f1115 / #f9fafb
    let text: u32 = if dark { 0x00F0_E8E2 } else { 0x0037_291F }; // #e2e8f0 / #1f2937
    let border: u32 = bg;
    let dark_mode: u32 = if dark { 1 } else { 0 };

    unsafe {
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_USE_IMMERSIVE_DARK_MODE as u32,
            std::ptr::addr_of!(dark_mode).cast(),
            std::mem::size_of::<u32>() as u32,
        );
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_CAPTION_COLOR as u32,
            std::ptr::addr_of!(bg).cast(),
            std::mem::size_of::<u32>() as u32,
        );
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_TEXT_COLOR as u32,
            std::ptr::addr_of!(text).cast(),
            std::mem::size_of::<u32>() as u32,
        );
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_BORDER_COLOR as u32,
            std::ptr::addr_of!(border).cast(),
            std::mem::size_of::<u32>() as u32,
        );
    }
}

/// Injected into every page the webview loads (including the navigated dsh
/// GUI): samples the page background to detect dark/light theme and reports it
/// to the Rust host via the `dsh-theme` event. Polls cheaply every 800ms and
/// also reacts to class/style mutations on <html>.
const THEME_WATCH_JS: &str = r#"
(function () {
  function hexToRgb(c) {
    var n = parseInt(c.slice(1), 16);
    return [(n >> 16) & 255, (n >> 8) & 255, n & 255];
  }
  function getDark() {
    try {
      var bg = getComputedStyle(document.body).backgroundColor;
      if (bg && bg.indexOf('rgb') === 0) {
        var m = bg.match(/[\d.]+/g);
        if (m && m.length >= 3) {
          var r = +m[0], g = +m[1], b = +m[2];
          var lum = 0.299 * r + 0.587 * g + 0.114 * b;
          return lum < 130;
        }
      }
    } catch (e) {}
    return true;
  }
  function report() {
    var dark = getDark();
    try {
      // Primary channel: Tauri event (works when __TAURI__ is injected).
      if (window.__TAURI__ && window.__TAURI__.event) {
        window.__TAURI__.event.emit('dsh-theme', { dark: dark });
        return;
      }
    } catch (e) {}
    try {
      // Fallback channel: encode the theme into the title, the host polls it.
      var prefix = dark ? '[dsh-dark]' : '[dsh-light]';
      var t = document.title;
      if (t.indexOf('[dsh-') !== 0) {
        document.title = prefix + (t || 'DSH Desktop');
      }
    } catch (e) {}
  }
  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', report);
  } else {
    report();
  }
  setInterval(report, 800);
  try {
    new MutationObserver(function () { report(); }).observe(
      document.documentElement, { attributes: true, attributeFilter: ['class', 'style', 'data-theme'] }
    );
  } catch (e) {}
})();
"#;

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            // Second launch: focus the existing window (show it first in case
            // it was hidden to the tray).
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.show();
                let _ = window.unminimize();
                let _ = window.set_focus();
            }
        }))
        .invoke_handler(tauri::generate_handler![
            current_status,
            install_dsh,
            retry_boot,
            open_node_site
        ])
        .setup(|app| {
            let handle = app.handle().clone();
            setup_tray(&handle)?;

            // Create the main window at runtime so we can inject the theme
            // watcher script; it starts on the local splash page and gets
            // navigated to the backend URL below once it reports ready.
            let window = WebviewWindowBuilder::new(
                &handle,
                "main",
                WebviewUrl::App("index.html".into()),
            )
            .title("DSH Desktop")
            .inner_size(1280.0, 840.0)
            .min_inner_size(800.0, 600.0)
            .center()
            .initialization_script(THEME_WATCH_JS)
            .build()
            .map_err(|e| format!("failed to create main window: {e}"))?;

            // React to theme changes coming from the web page (dark/light).
            let theme_handle = handle.clone();
            let _ = handle.listen("dsh-theme", move |event| {
                let dark = serde_json::from_str::<serde_json::Value>(event.payload())
                    .ok()
                    .and_then(|v| v.get("dark").and_then(|d| d.as_bool()))
                    .unwrap_or(true);
                #[cfg(windows)]
                if let Some(w) = theme_handle.get_webview_window("main") {
                    theme_titlebar(&w, dark);
                }
            });

            // Initial chrome: dsh defaults to the dark theme.
            #[cfg(windows)]
            theme_titlebar(&window, true);

            // Fallback channel for theme changes: the injected script encodes
            // "[dsh-dark]"/"[dsh-light]" into document.title when the Tauri
            // event bridge is unavailable on the navigated page. Poll it so the
            // title bar follows the page theme on all platforms.
            let title_window = window.clone();
            std::thread::spawn(move || {
                let mut last: Option<bool> = None;
                loop {
                    let current = title_window.title().ok().and_then(|t| {
                        if t.starts_with("[dsh-dark]") {
                            Some(true)
                        } else if t.starts_with("[dsh-light]") {
                            Some(false)
                        } else {
                            None
                        }
                    });
                    if current.is_some() && current != last {
                        last = current;
                        #[cfg(windows)]
                        theme_titlebar(&title_window, current.unwrap_or(true));
                    }
                    std::thread::sleep(Duration::from_millis(800));
                }
            });

            // The backend slot starts empty and is filled by boot_sequence once
            // we actually own a child. It stays empty when we reuse a dsh we did
            // not start (see try_boot): exiting must not kill the browser's dsh.
            app.manage(Backend(Mutex::new(None)));
            app.manage(BootLock(AtomicBool::new(false)));
            app.manage(Status(Mutex::new(status_payload("booting", Vec::new()))));

            // Boot in the background. This used to block `setup` for up to 90s
            // waiting on the backend URL, which froze the webview — and the boot
            // page now has to stay interactive to offer the install button.
            std::thread::spawn(move || boot_sequence(handle));

            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app_handle, event| match event {
            // Closing the window hides to the tray instead of quitting;
            // use the tray menu's "退出" (Quit) to fully exit.
            RunEvent::WindowEvent {
                label,
                event: WindowEvent::CloseRequested { api, .. },
                ..
            } if label == "main" => {
                api.prevent_close();
                if let Some(window) = app_handle.get_webview_window("main") {
                    let _ = window.hide();
                }
            }
            RunEvent::Exit => {
                if take_backend(app_handle).is_some() {
                    // A backend that shut down cleanly released its own lock;
                    // one we had to force did not. Clear it either way, so a dsh
                    // started outside this app (browser tab) is not blocked by
                    // our leftovers.
                    clear_stale_task_board_lock();
                }
            }
            _ => {}
        });
}

/// What the boot page is currently showing. Mirrors the `state` strings the
/// page switches on; see `ui/index.html`.
fn status_payload(state: &str, detail: Vec<String>) -> serde_json::Value {
    serde_json::json!({ "state": state, "detail": detail })
}

/// Push a status to the boot page and remember it for `current_status`.
fn emit_status(app: &tauri::AppHandle, state: &str, detail: Vec<String>) {
    eprintln!("boot status: {state}");
    let payload = status_payload(state, detail);
    if let Some(store) = app.try_state::<Status>() {
        *store.0.lock().unwrap() = payload.clone();
    }
    let _ = app.emit("dsh-status", payload);
}

/// The boot page asks for this on load, in case it missed the event.
#[tauri::command]
fn current_status(app: tauri::AppHandle) -> serde_json::Value {
    app.try_state::<Status>()
        .map(|s| s.0.lock().unwrap().clone())
        .unwrap_or_else(|| status_payload("booting", Vec::new()))
}

/// Why we cannot start a backend right now, if we cannot.
enum Preflight {
    Ready,
    /// No `dsh` on PATH. `has_npm` decides whether we can offer to install it.
    NoDsh { has_npm: bool },
}

fn preflight() -> Preflight {
    if has_command("dsh") {
        Preflight::Ready
    } else {
        Preflight::NoDsh { has_npm: has_command("npm") }
    }
}

/// Whether `name` resolves on PATH. On Windows this has to go through `where`
/// rather than a plain existence check: `dsh` and `npm` are `.cmd` shims, so
/// resolution depends on PATHEXT, which `where` applies and we would not.
fn has_command(name: &str) -> bool {
    let output = if cfg!(windows) {
        let mut cmd = Command::new("where");
        cmd.arg(name);
        #[cfg(windows)]
        {
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        cmd.stdout(Stdio::null()).stderr(Stdio::null()).status()
    } else {
        // `command -v` as an argument, never interpolated into the script text.
        Command::new("sh")
            .args(["-c", r#"command -v "$1" >/dev/null 2>&1"#, "sh", name])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
    };
    output.is_ok_and(|s| s.success())
}

/// Bring the app to a usable state: reuse or start a backend and navigate the
/// window to it, or leave the boot page showing what the user needs to do.
///
/// Runs on its own thread and may run again later (after an install, or a retry
/// from the page), so it takes the boot lock rather than assuming it is alone.
fn boot_sequence(app: tauri::AppHandle) {
    let Some(lock) = app.try_state::<BootLock>() else {
        return;
    };
    if lock.0.swap(true, Ordering::SeqCst) {
        return; // a boot is already in flight
    }

    if let Err(detail) = try_boot(&app) {
        emit_status(&app, "error", detail);
    }

    if let Some(lock) = app.try_state::<BootLock>() {
        lock.0.store(false, Ordering::SeqCst);
    }
}

/// The boot sequence proper. `Err` carries lines to show the user verbatim.
fn try_boot(app: &tauri::AppHandle) -> Result<(), Vec<String>> {
    // If a dsh web is already serving on the probe port (e.g. the browser tab's
    // instance), reuse it instead of launching our own. This avoids the
    // task-board single-instance lock colliding with the browser instance. We
    // leave Backend empty: we do not own that backend, and exiting the desktop
    // app must not kill it.
    if let Some(existing) = probe_existing_web() {
        navigate_to(app, &existing);
        return Ok(());
    }

    // Nothing to start if the command is missing. Report it on the page instead
    // of dying silently — previously this path just exited after a ~6s flash of
    // an empty window, with the shell's "command not found" going nowhere.
    match preflight() {
        Preflight::Ready => {}
        Preflight::NoDsh { has_npm } => {
            emit_status(app, if has_npm { "missing-dsh" } else { "missing-node" }, Vec::new());
            return Ok(());
        }
    }

    // A backend we killed on a previous exit never got to release the task-board
    // ledger lock; drop it now or the plugin tree refuses to load. See
    // clear_stale_task_board_lock.
    clear_stale_task_board_lock();
    // A retry after a failed start may still be holding a dead (or dying) child.
    take_backend(app);

    let mut spawned = spawn_backend()
        .map_err(|e| vec![format!("无法启动 `dsh web`：{e}")])?;
    let stderr = spawned.stderr.take();
    let log_file = backend_log_path(app)
        .map_err(|e| vec![format!("无法打开后端日志：{e}")])?;
    let (url_tx, url_rx) = mpsc::channel::<String>();
    // Closed once the stderr logger has flushed everything it will ever write,
    // so a failure path can read a complete log instead of racing it.
    let (logged_tx, logged_rx) = mpsc::channel::<()>();

    if let Some(state) = app.try_state::<Backend>() {
        *state.0.lock().unwrap() = Some(spawned.child);
    }

    // Reader thread: keeps stdout + stderr pipes open for the backend's whole
    // lifetime (dropping them early could EPIPE-crash node), and forwards the
    // first `dsh web: http://...` line to us.
    let log_path = log_file.clone();
    std::thread::spawn(move || {
        let mut stdout = spawned.reader;
        let mut sent = false;

        if let Some(err) = stderr {
            std::thread::spawn(move || {
                log_backend_stderr(err, &log_path);
                drop(logged_tx);
            });
        } else {
            drop(logged_tx);
        }

        let mut line = String::new();
        loop {
            line.clear();
            match stdout.read_line(&mut line) {
                Ok(0) | Err(_) => break, // backend exited
                Ok(_) => {
                    if let Some(url) = extract_url(&line) {
                        if !sent {
                            sent = true;
                            let _ = url_tx.send(url);
                        }
                    }
                }
            }
        }
        // Backend exited: notify the waiter so it can act (navigate on success
        // was already handled; on failure report why).
        let _ = url_tx.send(String::new());
    });

    // Wait (bounded) for the backend URL, then navigate the window.
    match url_rx.recv_timeout(BACKEND_READY_TIMEOUT) {
        Ok(url) if !url.is_empty() => {
            navigate_to(app, &url);
            // When the backend later exits on its own, quit the app so the
            // window does not sit on a dead page.
            let app = app.clone();
            std::thread::spawn(move || {
                let _ = url_rx.recv();
                let _ = app.exit(0);
            });
            Ok(())
        }
        _ => {
            // Let the logger finish before reading the log, or we summarize a
            // file the backend's own error has not reached yet.
            let _ = logged_rx.recv_timeout(Duration::from_secs(2));
            eprintln!("dsh backend never became ready (see log: {})", log_file.display());
            let summary = backend_error_summary(&log_file);
            for line in &summary {
                eprintln!("  {line}");
            }
            let mut detail = summary;
            detail.push(format!("完整日志：{}", log_file.display()));
            Err(detail)
        }
    }
}

fn navigate_to(app: &tauri::AppHandle, url: &str) {
    let Some(window) = app.get_webview_window("main") else {
        return;
    };
    match Url::parse(url) {
        Ok(parsed) => {
            let _ = window.navigate(parsed);
        }
        Err(e) => eprintln!("bad backend URL {url:?}: {e}"),
    }
}

/// Copy the backend's stderr into the log file, one prefixed line at a time.
///
/// Reads bytes rather than `read_line`: on Windows the failure text can come
/// from `cmd` itself in the console's OEM codepage, and a `String`-based read
/// errors out on the first such byte and abandons the rest of the log — which is
/// exactly the case the log exists to explain. Lossy decoding keeps the line.
fn log_backend_stderr(stderr: std::process::ChildStderr, log_path: &std::path::Path) {
    use std::io::Write;

    // Truncate per run: the log diagnoses the current launch, and appending
    // forever would grow unbounded and mix stale errors into the tail we print.
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(log_path);
    let Ok(mut file) = file else {
        return;
    };

    let mut reader = BufReader::new(stderr);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf) {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                let text = String::from_utf8_lossy(&buf);
                let _ = writeln!(file, "[dsh] {}", text.trim_end());
                let _ = file.flush();
            }
        }
    }
}

/// Take our backend child out of the state, if we have one.
fn take_backend(app: &tauri::AppHandle) -> Option<Child> {
    let child = app.try_state::<Backend>()?.0.lock().unwrap().take();
    let mut child = child?;
    kill_process_tree(child.id());
    let _ = child.kill();
    let _ = child.wait();
    Some(child)
}

/// Install the `dsh` command with npm, streaming npm's output to the page.
///
/// Returns as soon as the install is under way: the work runs on its own thread
/// so the IPC call does not block, and progress arrives as `dsh-install-log`
/// events. On success this boots straight into dsh — npm's global prefix is
/// already on PATH, so the shim it just wrote is visible without a restart.
#[tauri::command]
fn install_dsh(app: tauri::AppHandle) {
    std::thread::spawn(move || {
        emit_status(&app, "installing", Vec::new());
        match run_install(&app) {
            Ok(()) if has_command("dsh") => {
                emit_status(&app, "booting", vec!["安装完成，正在启动…".into()]);
                boot_sequence(app);
            }
            Ok(()) => emit_status(
                &app,
                "error",
                vec![format!("{DSH_PACKAGE} 安装完成，但 PATH 中仍找不到 `dsh` 命令。请重开一次应用；若仍不行，检查 npm 全局目录（npm prefix）是否在 PATH 中。")],
            ),
            Err(detail) => emit_status(&app, "error", vec![detail]),
        }
    });
}

/// Run `npm i -g <DSH_PACKAGE>`, forwarding every output line to the page.
fn run_install(app: &tauri::AppHandle) -> Result<(), String> {
    let mut cmd = if cfg!(windows) {
        // npm is a .cmd shim, so it needs a shell to run at all.
        let mut c = Command::new("cmd");
        c.args(["/C", "npm", "install", "-g", DSH_PACKAGE]);
        c
    } else {
        let mut c = Command::new("npm");
        c.args(["install", "-g", DSH_PACKAGE]);
        c
    };
    #[cfg(windows)]
    {
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        // npm writes its progress to stderr; merging both keeps the page's log
        // in the order the user would have seen in a terminal.
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("无法执行 npm：{e}"))?;

    let mut pumps = Vec::new();
    if let Some(out) = child.stdout.take() {
        pumps.push(pump_lines(app.clone(), out));
    }
    if let Some(err) = child.stderr.take() {
        pumps.push(pump_lines(app.clone(), err));
    }

    let status = child.wait().map_err(|e| format!("npm 执行失败：{e}"))?;
    for p in pumps {
        let _ = p.join();
    }

    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "npm install -g {DSH_PACKAGE} 失败（退出码 {}）。上面的日志里通常有原因，常见的是网络或权限问题。",
            status.code().map_or_else(|| "未知".to_string(), |c| c.to_string())
        ))
    }
}

/// Forward one output stream to the page as `dsh-install-log` events. Bytes, not
/// lines, for the same OEM-codepage reason as `log_backend_stderr`.
fn pump_lines<R: std::io::Read + Send + 'static>(
    app: tauri::AppHandle,
    stream: R,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stream);
        let mut buf = Vec::new();
        loop {
            buf.clear();
            match reader.read_until(b'\n', &mut buf) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    let text = String::from_utf8_lossy(&buf);
                    let text = text.trim_end();
                    if !text.is_empty() {
                        let _ = app.emit("dsh-install-log", text);
                    }
                }
            }
        }
    })
}

/// Try booting again after a failure, from the page's retry button.
#[tauri::command]
fn retry_boot(app: tauri::AppHandle) {
    std::thread::spawn(move || {
        emit_status(&app, "booting", Vec::new());
        boot_sequence(app);
    });
}

/// Open nodejs.org in the user's browser, for the no-Node case.
#[tauri::command]
fn open_node_site() {
    let mut cmd = if cfg!(windows) {
        let mut c = Command::new("cmd");
        // The empty "" is `start`'s title argument; without it a quoted URL
        // would be taken as the window title and nothing would open.
        c.args(["/C", "start", "", NODE_SITE]);
        c
    } else if cfg!(target_os = "macos") {
        let mut c = Command::new("open");
        c.arg(NODE_SITE);
        c
    } else {
        let mut c = Command::new("xdg-open");
        c.arg(NODE_SITE);
        c
    };
    #[cfg(windows)]
    {
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let _ = cmd.stdout(Stdio::null()).stderr(Stdio::null()).status();
}

/// Tray icon (DeepSeek whale) with a menu: show window / quit.
fn setup_tray<R: tauri::Runtime>(app: &tauri::AppHandle<R>) -> tauri::Result<()> {
    let show = MenuItem::with_id(app, "show", "显示 DSH", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&show, &quit])?;

    let icon = tauri::image::Image::from_bytes(include_bytes!("../icons/tray.png"))?;

    TrayIconBuilder::with_id("main")
        .icon(icon)
        .tooltip("DeepSeek Harness")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "show" => {
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.show();
                    let _ = window.unminimize();
                    let _ = window.set_focus();
                }
            }
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            // Left click shows the window.
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                let app = tray.app_handle();
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.show();
                    let _ = window.unminimize();
                    let _ = window.set_focus();
                }
            }
        })
        .build(app)?;
    Ok(())
}

struct Spawned {
    child: Child,
    reader: BufReader<std::process::ChildStdout>,
    stderr: Option<std::process::ChildStderr>,
}

/// Port to probe for an already-running `dsh web`. Overridable via the
/// `DSH_DESKTOP_PROBE_PORT` environment variable.
fn probe_port() -> u16 {
    std::env::var("DSH_DESKTOP_PROBE_PORT")
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(3080)
}

/// Check whether a dsh web GUI is already serving on the probe port. If so,
/// return its URL to reuse; otherwise None (we will launch our own backend).
fn probe_existing_web() -> Option<String> {
    use std::io::{Read, Write};

    let port = probe_port();
    let addr = format!("127.0.0.1:{port}");
    let mut stream = TcpStream::connect_timeout(&addr.parse().ok()?, Duration::from_millis(1200))
        .ok()?;
    let _ = stream.set_read_timeout(Some(Duration::from_millis(1500)));
    let _ = write!(
        stream,
        "GET / HTTP/1.0\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n"
    );
    let mut buf = String::new();
    let _ = stream.read_to_string(&mut buf);
    // A dsh web page titles itself "DeepSeek Harness".
    if buf.contains("<title>DeepSeek Harness") || buf.contains("window.__DSH_BOOT__") {
        Some(format!("http://127.0.0.1:{port}"))
    } else {
        None
    }
}

/// Launch `dsh web --port 0` (OS picks a free port) from the user's home
/// directory, with piped stdout/stderr (stderr goes to a log file).
fn spawn_backend() -> std::io::Result<Spawned> {
    let cwd = working_dir();
    let mut cmd = if cfg!(windows) {
        let mut c = Command::new("cmd");
        c.args(["/C", "dsh", "web", "--port", "0"]);
        c
    } else {
        let mut c = Command::new("sh");
        c.args(["-c", "dsh web --port 0"]);
        c
    };
    // No black console window for the spawned cmd.exe on Windows.
    #[cfg(windows)]
    {
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd.current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn()?;
    let stdout = child.stdout.take().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::Other, "failed to capture stdout")
    })?;
    let stderr = child.stderr.take();
    Ok(Spawned {
        child,
        reader: BufReader::new(stdout),
        stderr,
    })
}

fn working_dir() -> std::path::PathBuf {
    if let Ok(dir) = std::env::var("DSH_DESKTOP_WORKDIR") {
        if !dir.is_empty() {
            return std::path::PathBuf::from(dir);
        }
    }
    // dsh treats the current directory as the default workspace root.
    dirs_home()
}

fn dirs_home() -> std::path::PathBuf {
    #[cfg(windows)]
    {
        std::path::PathBuf::from(
            std::env::var("USERPROFILE").unwrap_or_else(|_| ".".to_string()),
        )
    }
    #[cfg(not(windows))]
    {
        std::path::PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".to_string()))
    }
}

fn backend_log_path(app: &tauri::AppHandle) -> tauri::Result<PathBuf> {
    let dir = app.path().app_log_dir()?;
    std::fs::create_dir_all(&dir)?;
    Ok(dir.join("dsh-backend.log"))
}

/// Resolve dsh's home directory the same way dsh does: `$DSH_HOME` wins
/// (with a leading `~` expanded), otherwise `~/.dsh`.
fn dsh_home() -> PathBuf {
    if let Ok(raw) = std::env::var("DSH_HOME") {
        let raw = raw.trim();
        if !raw.is_empty() {
            let expanded = if raw == "~" {
                dirs_home()
            } else if let Some(rest) = raw.strip_prefix("~/").or_else(|| raw.strip_prefix("~\\")) {
                dirs_home().join(rest)
            } else {
                PathBuf::from(raw)
            };
            return expanded;
        }
    }
    dirs_home().join(".dsh")
}

/// Drop a stale `task-board` ledger lock left behind by a backend that died
/// without releasing it — a crash, or a shutdown we had to force past the grace
/// period in `kill_process_tree`, either of which skips node's cleanup hook.
///
/// The plugin guards the lock with a bare liveness check on the recorded PID,
/// which the OS is free to reassign to an unrelated process — once it does, the
/// lock looks permanently held and `dsh web` refuses to boot. So we only clear
/// the lock when the recorded PID is *not* a live node process; a genuinely
/// running backend (e.g. the browser tab's dsh) keeps its lock untouched.
fn clear_stale_task_board_lock() {
    let lock = dsh_home().join("task-board").join("ledger-v2.lock");
    let Ok(raw) = std::fs::read_to_string(&lock) else {
        return; // no lock, nothing to do
    };

    let pid = serde_json::from_str::<serde_json::Value>(&raw)
        .ok()
        .and_then(|v| v.get("pid").and_then(|p| p.as_u64()));

    // Unreadable/PID-less lock is junk by definition; a PID that is not a live
    // node process cannot be the owning backend.
    match pid {
        Some(pid) if node_process_alive(pid) => return,
        _ => {}
    }

    if std::fs::remove_file(&lock).is_ok() {
        eprintln!("cleared stale task-board lock: {}", lock.display());
    }
}

/// Whether `pid` is alive *and* is a node process — the identity check the
/// ledger lock itself skips, which is what makes PID reuse fatal there.
fn node_process_alive(pid: u64) -> bool {
    process_image_matches(pid, |image| image.contains("node"))
}

/// Whether any process with `pid` currently exists.
fn process_alive(pid: u32) -> bool {
    process_image_matches(pid as u64, |_| true)
}

/// Look up `pid`'s image name and test it with `pred`. When the lookup itself
/// fails we answer `true`: callers use this to decide whether to keep waiting
/// or to destroy something, and both are safer erring on "still there".
fn process_image_matches(pid: u64, pred: impl Fn(&str) -> bool) -> bool {
    let output = if cfg!(windows) {
        let mut cmd = Command::new("tasklist");
        cmd.args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"]);
        #[cfg(windows)]
        {
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        cmd.output()
    } else {
        Command::new("ps")
            .args(["-p", &pid.to_string(), "-o", "comm="])
            .output()
    };

    let Ok(out) = output else {
        return true; // cannot tell
    };
    // Image names are ASCII, so this survives the console's OEM codepage.
    let text = String::from_utf8_lossy(&out.stdout).to_lowercase();

    let image = if cfg!(windows) {
        // A match is a CSV row starting with the quoted image name; a miss is a
        // localized "no tasks" notice, which never starts with a quote.
        text.strip_prefix('"').and_then(|rest| rest.split('"').next())
    } else {
        Some(text.trim()).filter(|t| !t.is_empty())
    };

    image.is_some_and(|image| pred(image))
}

/// Pull the interesting lines out of the backend log for a startup failure, so
/// the terminal shows *why* it failed instead of only a path to go read.
fn backend_error_summary(log: &std::path::Path) -> Vec<String> {
    let Ok(raw) = std::fs::read_to_string(log) else {
        return Vec::new();
    };
    // dsh nests its boot failures: the outermost lines only say "plugin tree
    // failed to load" / "loader entries failed to apply", while the lines that
    // name the actual broken package sit at the bottom of the cause chain. Rank
    // by specificity so the terminal shows the latter, not the former.
    let mut specific = Vec::new();
    let mut generic = Vec::new();
    for line in raw.lines() {
        let text = line.trim_start_matches("[dsh] ").trim();
        // Error headlines only: stack frames repeat the same cause many times.
        if text.starts_with("at ") || !text.contains("Error") {
            continue;
        }
        let text = text.trim_start_matches("[cause]: ").to_string();
        let bucket = if text.contains("Cannot find") || text.contains("already owned by") {
            &mut specific
        } else {
            &mut generic
        };
        if !bucket.contains(&text) {
            bucket.push(text);
        }
    }
    specific.append(&mut generic);
    specific.truncate(4);
    specific
}

fn extract_url(line: &str) -> Option<String> {
    let start = line.find("http://")?;
    let rest = line[start..].trim();
    let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
    if end == 0 {
        None
    } else {
        Some(rest[..end].to_string())
    }
}

/// How long a backend that can actually honor a polite shutdown gets before we
/// force it. Only reachable off Windows — see `kill_process_tree`.
const KILL_GRACE: Duration = Duration::from_secs(8);

/// Shut down the backend and its whole child tree (dsh may spawn helpers such
/// as `cloudflared` tunnels), so nothing is left running after the app exits.
///
/// Why the two paths differ, measured rather than assumed:
///
/// On Unix, SIGTERM reaches the tree and lets it run its own cleanup, so we ask
/// first and only escalate to SIGKILL if the grace period runs out. That matters
/// because dsh shells out to pnpm, which swaps a package by staging a copy into
/// `<pkg>_tmp_<pid>_<n>` and renaming it over the target; a hard kill landing
/// between "empty the target" and "rename" leaves a bare `src/` with no
/// `package.json`, and the profile then fails to boot with ERR_MODULE_NOT_FOUND.
///
/// On Windows that ask is not available. The backend is a windowless `cmd`
/// wrapping node, and `taskkill /T` without `/F` refuses every process in the
/// tree with "只能强制终止此任务(带 /F 选项)" — a console process with no window
/// has nothing to send a close message to. So the polite call cannot ever
/// succeed, and waiting on it only spends the user's exit: the app holds the
/// single-instance lock until it is gone, so a slow quit silently turns an
/// immediate relaunch into a no-op. We force immediately and rely on
/// startup-side recovery (clear_stale_task_board_lock) for the leftovers.
fn kill_process_tree(pid: u32) {
    if cfg!(windows) {
        run_kill(pid, true);
        return;
    }

    // Polite first, so the tree can run its own cleanup.
    run_kill(pid, false);

    // Poll instead of sleeping a flat interval: a backend that is already gone,
    // or that honors the signal, costs us nothing.
    let start = Instant::now();
    while start.elapsed() < KILL_GRACE {
        if !process_alive(pid) {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    run_kill(pid, true);
}

/// Issue one terminate against `pid`'s tree, forced or not.
fn run_kill(pid: u32, force: bool) {
    let mut cmd = if cfg!(windows) {
        let mut c = Command::new("taskkill");
        c.args(["/PID", &pid.to_string(), "/T"]);
        if force {
            c.arg("/F");
        }
        // Do not flash a console window for taskkill either.
        #[cfg(windows)]
        {
            c.creation_flags(CREATE_NO_WINDOW);
        }
        c
    } else {
        let mut c = Command::new("kill");
        c.args([if force { "-9" } else { "-TERM" }, &pid.to_string()]);
        c
    };
    // The wrapper is often gone already (the tree exits on its own), and the
    // "process not found" complaint is localized console output that lands as
    // mojibake in the parent terminal. Swallow both streams.
    let _ = cmd.stdout(Stdio::null()).stderr(Stdio::null()).status();
}