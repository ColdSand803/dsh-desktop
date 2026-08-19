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
        DwmSetWindowAttribute, DWMWA_BORDER_COLOR, DWMWA_CAPTION_COLOR, DWMWA_TEXT_COLOR,
        DWMWA_USE_IMMERSIVE_DARK_MODE,
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
  var eventBroken = false; // latched once we learn the Tauri bridge is unusable
  var lastMarked = null;   // theme currently encoded into document.title

  function markTitle(dark) {
    // Fallback channel: encode the theme into document.title. The host polls
    // it, applies the theme, then strips the marker back off. Only rewrite on
    // an actual change, otherwise we fight the host for the title every tick.
    if (lastMarked === dark) return;
    lastMarked = dark;
    try {
      var t = document.title.replace(/^\[dsh-(dark|light)\]/, '');
      document.title = (dark ? '[dsh-dark]' : '[dsh-light]') + (t || 'DSH Desktop');
    } catch (e) {}
  }

  function report() {
    var dark = getDark();
    if (!eventBroken) {
      try {
        // Primary channel: Tauri event. Only reaches the host if a capability
        // declares remote.urls for this origin (capabilities/remote-theme.json).
        var p = window.__TAURI__ && window.__TAURI__.event &&
          window.__TAURI__.event.emit('dsh-theme', { dark: dark });
        if (p) {
          // emit() is async, so a capability rejection lands in the promise --
          // not as a synchronous throw. Latch the fallback when that happens.
          if (p.catch) {
            p.catch(function () { eventBroken = true; markTitle(dark); });
          }
          return;
        }
        eventBroken = true; // no bridge injected on this page at all
      } catch (e) {
        eventBroken = true;
      }
    }
    markTitle(dark);
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
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_dialog::init())
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
            let window =
                WebviewWindowBuilder::new(&handle, "main", WebviewUrl::App("index.html".into()))
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
            std::thread::spawn(move || loop {
                let title = title_window.title().unwrap_or_default();
                let marker = if title.starts_with("[dsh-dark]") {
                    Some((true, "[dsh-dark]"))
                } else if title.starts_with("[dsh-light]") {
                    Some((false, "[dsh-light]"))
                } else {
                    None
                };
                if let Some((dark, prefix)) = marker {
                    #[cfg(windows)]
                    theme_titlebar(&title_window, dark);
                    // Strip the marker so the user never sees it in the title
                    // bar or taskbar. The injected script only re-marks on an
                    // actual theme change, so this settles instead of looping.
                    let _ = title_window.set_title(title.trim_start_matches(prefix));
                }
                std::thread::sleep(Duration::from_millis(800));
            });

            // Probing, spawning and waiting for the backend URL can take tens
            // of seconds, so none of it may happen here: `setup` runs to
            // completion *before* `run()` starts the event loop, so blocking
            // would leave the native window unable to pump messages -- Windows
            // greys it out as unresponsive, the splash page freezes and tray
            // clicks queue up unhandled. Hand it all to a worker thread.
            let boot = handle.clone();
            std::thread::spawn(move || {
                if let Err(e) = boot_backend(&boot) {
                    eprintln!("dsh backend failed: {e}");
                    report_boot_failure(&boot, &e);
                }
            });

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
                    // Recover from a poisoned lock rather than unwrapping: if
                    // any thread panicked while holding it, that must not stop
                    // us from reaping the backend. The Option<Child> behind it
                    // is still perfectly valid.
                    let mut guard = match state.0.lock() {
                        Ok(g) => g,
                        Err(poisoned) => poisoned.into_inner(),
                    };
                    if let Some(mut child) = guard.take() {
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
    let update = MenuItem::with_id(app, "update", "检查更新", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&show, &update, &quit])?;

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
            "update" => {
                let app = app.clone();
                tauri::async_runtime::spawn(async move { check_for_update(app).await });
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

/// Surface a boot failure on the splash page instead of quitting. The window
/// has not navigated yet, so the splash is still live and can render the reason
/// -- previously the app called exit(1) here and the window just vanished,
/// leaving the user nothing to act on. Quitting is left to the tray menu.
fn report_boot_failure(handle: &tauri::AppHandle, error: &str) {
    let log_hint = backend_log_path(handle)
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    let Some(window) = handle.get_webview_window("main") else {
        return;
    };
    // serde_json quotes and escapes, so the message cannot break out of the
    // call even though it carries paths and backslashes.
    let js = format!(
        "window.__dshBootError && window.__dshBootError({}, {})",
        serde_json::to_string(error).unwrap_or_else(|_| "\"\"".into()),
        serde_json::to_string(&log_hint).unwrap_or_else(|_| "\"\"".into()),
    );
    let _ = window.show();
    // A failure can land before the splash page has finished loading -- with
    // `dsh` missing from PATH, `cmd /C` exits in milliseconds. eval() cannot
    // report whether the handler existed yet, so just repeat for a couple of
    // seconds; __dshBootError is idempotent.
    for attempt in 0..6 {
        if attempt > 0 {
            std::thread::sleep(Duration::from_millis(350));
        }
        let _ = window.eval(js.as_str());
    }
}

/// Bring the backend up and point the window at it. Runs on a worker thread --
/// every step here can block for seconds, which must never happen on the thread
/// that owns the event loop. Returns Err only when the GUI never became
/// reachable, in which case the caller quits the app.
fn boot_backend(handle: &tauri::AppHandle) -> Result<(), String> {
    // If a dsh web is already serving on the probe port (e.g. the browser tab's
    // instance), reuse it instead of launching our own. This avoids the
    // task-board single-instance lock colliding with the browser instance. We
    // don't own that backend, so Backend(None): exiting the desktop app must
    // not kill the browser's dsh.
    if let Some(existing) = probe_existing_web() {
        handle.manage(Backend(Mutex::new(None)));
        let url = Url::parse(&existing).map_err(|e| format!("bad probe URL: {e}"))?;
        if let Some(window) = handle.get_webview_window("main") {
            let _ = window.navigate(url);
        }
        return Ok(());
    }

    let mut spawned = spawn_backend().map_err(|e| format!("failed to start `dsh web`: {e}"))?;
    let stderr = spawned.stderr.take();
    let log_file = backend_log_path(handle).map_err(|e| format!("log path: {e}"))?;
    let logger = spawn_logger(log_file.clone());
    let (url_tx, url_rx) = mpsc::channel::<String>();

    // Tie the child to a job object so the whole `dsh web` tree (node, and any
    // cloudflared helper it spawns) dies with us even on paths where the
    // RunEvent::Exit cleanup never runs: panic, Task Manager kill, logoff.
    #[cfg(windows)]
    confine_to_job(&spawned.child);

    handle.manage(Backend(Mutex::new(Some(spawned.child))));

    // Reader thread: keeps stdout + stderr pipes open for the backend's whole
    // lifetime (dropping them early could EPIPE-crash node), and forwards the
    // first `dsh web: http://...` line back here.
    std::thread::spawn(move || {
        let mut stdout = spawned.reader;
        let mut sent = false;

        // stderr on its own thread: a quiet pipe must never stall the other.
        if let Some(mut err) = stderr.map(BufReader::new) {
            let log = logger.clone();
            std::thread::spawn(move || {
                let mut line = String::new();
                loop {
                    line.clear();
                    match err.read_line(&mut line) {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {
                            let _ = log.send(format!("[err] {}", line.trim_end()));
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
                    // Log stdout as well. It used to be parsed for the URL and
                    // then dropped, so a boot that failed without touching
                    // stderr left an empty log -- the very log the README sends
                    // users to for the reason.
                    let _ = logger.send(format!("[out] {}", line.trim_end()));
                    if !sent {
                        if let Some(url) = extract_url(&line) {
                            sent = true;
                            let _ = url_tx.send(url);
                        }
                    }
                }
            }
        }
        // Backend exited: an empty string tells the waiter to give up / quit.
        let _ = url_tx.send(String::new());
    });

    // Wait (bounded) for the backend URL, then navigate the window.
    let url = match url_rx.recv_timeout(Duration::from_secs(90)) {
        Ok(url) if !url.is_empty() => url,
        _ => {
            return Err(format!(
                "dsh backend never became ready (see log: {})",
                log_file.display()
            ))
        }
    };
    let parsed = Url::parse(&url).map_err(|e| format!("bad backend URL {url:?}: {e}"))?;
    if let Some(window) = handle.get_webview_window("main") {
        let _ = window.navigate(parsed);
    }

    // When the backend later exits on its own, quit the app so the window does
    // not sit on a dead page.
    let _ = url_rx.recv();
    handle.exit(0);
    Ok(())
}

/// Put the child into a job object set to kill everything inside it once the
/// last handle to the job closes. We hold that handle, so however this process
/// dies -- clean exit, panic, Task Manager, logoff -- Windows reaps the whole
/// `dsh web` tree. The `RunEvent::Exit` taskkill stays as the graceful path;
/// this is the backstop for the paths where it never runs.
///
/// The job handle is deliberately leaked: it has to stay open for our entire
/// lifetime, and the OS closing it during process teardown is precisely the
/// trigger we want.
#[cfg(windows)]
fn confine_to_job(child: &Child) {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            return;
        }
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            std::ptr::addr_of!(info).cast(),
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        ) == 0
        {
            return;
        }
        AssignProcessToJobObject(job, child.as_raw_handle() as _);
    }
}

/// Tray-triggered update check. Manual rather than automatic on startup: an
/// update replaces the running binary and needs a relaunch, so it must not
/// happen behind the user's back mid-session.
async fn check_for_update<R: tauri::Runtime>(app: tauri::AppHandle<R>) {
    use tauri_plugin_dialog::{DialogExt, MessageDialogButtons};
    use tauri_plugin_updater::UpdaterExt;

    let result = match app.updater() {
        Ok(updater) => updater.check().await,
        Err(e) => {
            let _ = app.dialog().message(format!("更新器初始化失败：{e}"));
            return;
        }
    };

    match result {
        Ok(Some(update)) => {
            let version = update.version.clone();
            let handle = app.clone();
            app.dialog()
                .message(format!(
                    "发现新版本 {version}（当前 {}）。\n更新后应用会自动重启。",
                    update.current_version
                ))
                .title("DSH Desktop 更新")
                .buttons(MessageDialogButtons::OkCancelCustom(
                    "立即更新".into(),
                    "稍后".into(),
                ))
                .show(move |confirmed| {
                    if !confirmed {
                        return;
                    }
                    tauri::async_runtime::spawn(async move {
                        match update.download_and_install(|_, _| {}, || {}).await {
                            // Relaunch into the new binary; the RunEvent::Exit
                            // handler still runs, so the backend gets reaped.
                            Ok(()) => handle.restart(),
                            Err(e) => {
                                let _ = handle
                                    .dialog()
                                    .message(format!("更新失败：{e}"))
                                    .title("DSH Desktop 更新")
                                    .blocking_show();
                            }
                        }
                    });
                });
        }
        Ok(None) => {
            app.dialog()
                .message("已经是最新版本。")
                .title("DSH Desktop 更新")
                .show(|_| {});
        }
        Err(e) => {
            app.dialog()
                .message(format!("检查更新失败：{e}"))
                .title("DSH Desktop 更新")
                .show(|_| {});
        }
    }
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
    let mut stream =
        TcpStream::connect_timeout(&addr.parse().ok()?, Duration::from_millis(1200)).ok()?;
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
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| std::io::Error::other("failed to capture stdout"))?;
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
        std::path::PathBuf::from(std::env::var("USERPROFILE").unwrap_or_else(|_| ".".to_string()))
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

/// Size at which the backend log rolls over to `dsh-backend.log.1`.
const LOG_MAX_BYTES: u64 = 5 * 1024 * 1024;

/// Spawn the single thread that owns the log file. Both output pipes feed it
/// through the returned sender, so the readers never contend for the handle and
/// rotation has exactly one owner. Without this the log grew unbounded.
fn spawn_logger(path: PathBuf) -> mpsc::Sender<String> {
    use std::io::Write;

    let (tx, rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        let mut file = open_log(&path);
        let mut size = file
            .as_ref()
            .and_then(|f| f.metadata().ok())
            .map(|m| m.len())
            .unwrap_or(0);

        for line in rx {
            if size + line.len() as u64 > LOG_MAX_BYTES {
                // Close before renaming: Windows will not rename an open file.
                drop(file.take());
                let rotated = path.with_extension("log.1");
                let _ = std::fs::remove_file(&rotated);
                let _ = std::fs::rename(&path, &rotated);
                file = open_log(&path);
                size = 0;
            }
            if let Some(f) = file.as_mut() {
                if writeln!(f, "{line}").is_ok() {
                    let _ = f.flush();
                    size += line.len() as u64 + 1;
                }
            }
        }
    });
    tx
}

fn open_log(path: &std::path::Path) -> Option<std::fs::File> {
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .ok()
}

/// Pull the local GUI URL out of a line of `dsh web` output.
///
/// Deliberately strict: the backend prints other URLs too (docs links, a
/// cloudflared tunnel address, error prose), and navigating to the wrong one
/// parks the window on something that is not the GUI. We accept only an
/// `http://` URL that is loopback *and* carries an explicit port -- exactly
/// what `--port 0` reports -- and keep scanning the line when the first match
/// does not qualify.
fn extract_url(line: &str) -> Option<String> {
    let clean = strip_ansi(line);
    for (idx, _) in clean.match_indices("http://") {
        let rest = &clean[idx..];
        let end = rest
            .find(|c: char| c.is_whitespace() || c.is_control())
            .unwrap_or(rest.len());
        // Trailing punctuation belongs to the prose, not the URL.
        let candidate = rest[..end].trim_end_matches(|c| {
            matches!(
                c,
                '.' | ',' | ')' | ']' | '}' | '"' | '\'' | ';' | ':' | '!' | '?'
            )
        });
        let Ok(parsed) = Url::parse(candidate) else {
            continue;
        };
        let loopback = matches!(
            parsed.host_str(),
            Some("127.0.0.1" | "localhost" | "::1" | "0.0.0.0")
        );
        // Require an explicit :port. `Url::port()` reports None when the port
        // equals the scheme default (80), so inspect the authority text.
        let authority = candidate["http://".len()..]
            .split(['/', '?', '#'])
            .next()
            .unwrap_or_default();
        let has_port = authority
            .rsplit_once(':')
            .is_some_and(|(_, p)| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()));
        if parsed.scheme() == "http" && loopback && has_port {
            return Some(candidate.to_string());
        }
    }
    None
}

/// Strip ANSI CSI escapes (`ESC [ ... final-byte`). `dsh` is a Node CLI and
/// those routinely colourise URLs; without this the reset code gets glued onto
/// the end of the address we extract.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }
        if chars.next() != Some('[') {
            continue; // stray escape, not a CSI sequence
        }
        for c in chars.by_ref() {
            if ('\x40'..='\x7e').contains(&c) {
                break; // CSI final byte
            }
        }
    }
    out
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_loopback_with_port() {
        assert_eq!(
            extract_url("dsh web: http://127.0.0.1:52341\n").as_deref(),
            Some("http://127.0.0.1:52341")
        );
        assert_eq!(
            extract_url("  ready on http://localhost:3080/app?x=1 \r\n").as_deref(),
            Some("http://localhost:3080/app?x=1")
        );
    }

    #[test]
    fn requires_an_explicit_port() {
        // Without a port we cannot tell the GUI from an unrelated docs link,
        // and `--port 0` always reports one.
        assert_eq!(extract_url("see http://localhost/docs"), None);
    }

    #[test]
    fn rejects_non_loopback() {
        assert_eq!(extract_url("tunnel: http://example.com:8080"), None);
    }

    #[test]
    fn skips_unqualified_match_and_keeps_scanning() {
        // The real failure this guards: a docs link printed before the GUI URL
        // used to win because only the first `http://` was ever considered.
        assert_eq!(
            extract_url("docs http://example.com/x then http://127.0.0.1:9001").as_deref(),
            Some("http://127.0.0.1:9001")
        );
    }

    #[test]
    fn trims_trailing_prose_punctuation() {
        assert_eq!(
            extract_url("open (http://127.0.0.1:3080).").as_deref(),
            Some("http://127.0.0.1:3080")
        );
    }

    #[test]
    fn strips_ansi_colour_codes() {
        assert_eq!(
            extract_url("\x1b[32mhttp://127.0.0.1:3080\x1b[0m").as_deref(),
            Some("http://127.0.0.1:3080")
        );
    }

    #[test]
    fn ignores_lines_without_a_url() {
        assert_eq!(extract_url("Error: EADDRINUSE"), None);
        assert_eq!(extract_url(""), None);
        // https is never the local GUI; we only ever serve plain http.
        assert_eq!(extract_url("https://127.0.0.1:3080"), None);
    }
}
