// Prevents an extra console window on Windows in release builds.
// DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::io::{BufRead, BufReader};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::sync::Mutex;
use std::time::Duration;

use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::webview::WebviewWindowBuilder;
use tauri::{Listener, Manager, RunEvent, Url, WebviewUrl, WindowEvent};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

/// CREATE_NO_WINDOW: spawn the console child without flashing a black window.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Holds the spawned `dsh web` backend so we can kill it on exit.
struct Backend(Mutex<Option<Child>>);

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

            // If a dsh web is already serving on the probe port (e.g. the
            // browser tab's instance), reuse it instead of launching our own.
            // This avoids the task-board single-instance lock colliding with
            // the browser instance. We don't own that backend, so Backend(None):
            // exiting the desktop app must not kill the browser's dsh.
            if let Some(existing) = probe_existing_web() {
                app.manage(Backend(Mutex::new(None)));
                if let Ok(parsed) = Url::parse(&existing) {
                    let _ = window.navigate(parsed);
                }
                return Ok(());
            }

            let mut spawned = spawn_backend()
                .map_err(|e| format!("failed to start `dsh web`: {e}"))?;
            let stderr = spawned.stderr.take();
            let log_file = backend_log_path(&handle)?;
            let (url_tx, url_rx) = mpsc::channel::<String>();

            app.manage(Backend(Mutex::new(Some(spawned.child))));

            // Reader thread: keeps stdout + stderr pipes open for the backend's
            // whole lifetime (dropping them early could EPIPE-crash node), and
            // forwards the first `dsh web: http://...` line to the main thread.
            let navigator = handle.clone();
            let log_path = log_file.clone();
            std::thread::spawn(move || {
                let mut stdout = spawned.reader;
                let stderr = stderr.map(BufReader::new);
                let mut sent = false;

                // stderr -> log file logger
                if let Some(err) = stderr {
                    std::thread::spawn(move || {
                        use std::io::Write;
                        let file = std::fs::OpenOptions::new()
                            .create(true)
                            .append(true)
                            .open(&log_path);
                        if let Ok(mut file) = file {
                            let mut line = String::new();
                            let mut r = err;
                            loop {
                                line.clear();
                                match r.read_line(&mut line) {
                                    Ok(0) | Err(_) => break,
                                    Ok(_) => {
                                        let _ = writeln!(file, "[dsh] {}", line.trim_end());
                                        let _ = file.flush();
                                    }
                                }
                            }
                        }
                    });
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
                // Backend exited: notify the main thread so it can act
                // (navigate on success was already handled; on failure quit).
                let _ = url_tx.send(String::new());
            });

            // Wait (bounded) for the backend URL, then navigate the window.
            match url_rx.recv_timeout(Duration::from_secs(90)) {
                Ok(url) if !url.is_empty() => {
                    if let Some(window) = navigator.get_webview_window("main") {
                        match Url::parse(&url) {
                            Ok(parsed) => {
                                let _ = window.navigate(parsed);
                            }
                            Err(e) => eprintln!("bad backend URL {url:?}: {e}"),
                        }
                    }
                    // When the backend later exits on its own, quit the app so the
                    // window does not sit on a dead page.
                    std::thread::spawn(move || {
                        let _ = url_rx.recv();
                        let _ = navigator.exit(0);
                    });
                }
                _ => {
                    eprintln!("dsh backend never became ready (see log: {})", log_file.display());
                    let _ = navigator.exit(1);
                }
            }

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
                if let Some(state) = app_handle.try_state::<Backend>() {
                    if let Some(mut child) = state.0.lock().unwrap().take() {
                        kill_process_tree(child.id());
                        let _ = child.kill();
                        let _ = child.wait();
                    }
                }
            }
            _ => {}
        });
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

/// Kill the backend and its whole child tree (dsh may spawn helpers such as
/// `cloudflared` tunnels), so nothing is left running after the app exits.
fn kill_process_tree(pid: u32) {
    if cfg!(windows) {
        let mut cmd = Command::new("taskkill");
        cmd.args(["/PID", &pid.to_string(), "/T", "/F"]);
        // Do not flash a console window for taskkill either.
        #[cfg(windows)]
        {
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        let _ = cmd.status();
    } else {
        let _ = Command::new("kill").args(["-9", &pid.to_string()]).status();
    }
}