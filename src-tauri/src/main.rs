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

/// Colours sampled from the dsh page, as reported by THEME_WATCH_JS.
#[derive(Clone, Copy, serde::Deserialize)]
struct Theme {
    /// Background actually painted at the top of the page, RGB.
    bg: [u8; 3],
    /// A foreground that stays readable on `bg`, RGB.
    fg: [u8; 3],
    dark: bool,
}

impl Default for Theme {
    fn default() -> Self {
        // dsh's dark sidebar, used until the page reports in. These are its own
        // token values: --dsw-specific-sidebar-fill resolves to
        // --dsw-static-neutral-bluish-900, and the text token to -00.
        //
        // Note rgb(15,17,21) -- which an earlier version used here -- is
        // neutral-bluish-1000, dsh's *text* colour, not a surface. Using it as
        // the background is what made the title bar look too dark.
        Self {
            bg: [27, 27, 28],
            fg: [255, 255, 255],
            dark: true,
        }
    }
}

impl Theme {
    fn css_bg(&self) -> String {
        format!("rgb({},{},{})", self.bg[0], self.bg[1], self.bg[2])
    }
    fn css_fg(&self) -> String {
        format!("rgb({},{},{})", self.fg[0], self.fg[1], self.fg[2])
    }
}

/// Push a sampled theme to the self-drawn title bar, and tint the native frame
/// where the OS supports it.
fn apply_theme(window: &tauri::WebviewWindow, theme: Theme) {
    let js = format!(
        "window.__dshApplyTheme && window.__dshApplyTheme({}, {})",
        serde_json::to_string(&theme.css_bg()).unwrap_or_else(|_| "\"\"".into()),
        serde_json::to_string(&theme.css_fg()).unwrap_or_else(|_| "\"\"".into()),
    );
    let _ = window.eval(js.as_str());

    #[cfg(windows)]
    theme_titlebar(window, theme);
}

/// Tint the window's border and dark-mode flag to match the page.
///
/// The window is created with `decorations: false`, so there is no native
/// caption to paint -- the visible title bar is drawn in `ui/index.html`. What
/// is still worth setting here is the border colour and the immersive dark-mode
/// flag, which affect the window frame itself.
///
/// Note the caption/text/border colour attributes need Windows 11 (build
/// 22000+); on Windows 10 they fail harmlessly and only the dark-mode flag
/// takes effect. That limitation is exactly why the title bar is self-drawn.
#[cfg(windows)]
fn theme_titlebar(window: &tauri::WebviewWindow, theme: Theme) {
    use windows_sys::Win32::Graphics::Dwm::{
        DwmSetWindowAttribute, DWMWA_BORDER_COLOR, DWMWA_CAPTION_COLOR, DWMWA_TEXT_COLOR,
        DWMWA_USE_IMMERSIVE_DARK_MODE,
    };

    let hwnd = match window.hwnd() {
        Ok(h) => h.0,
        Err(_) => return,
    };

    // COLORREF is 0x00BBGGRR -- byte order is reversed from hex RGB.
    let colorref = |c: [u8; 3]| (c[2] as u32) << 16 | (c[1] as u32) << 8 | c[0] as u32;
    let bg: u32 = colorref(theme.bg);
    let text: u32 = colorref(theme.fg);
    let border: u32 = bg;
    let dark_mode: u32 = u32::from(theme.dark);

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

/// Injected into every frame the webview loads, including the dsh GUI running
/// inside our shell's iframe. Reports dsh's sidebar colour to the Rust host via
/// the `dsh-theme` event so the self-drawn title bar sits flush with it.
///
/// Reads dsh's own design token, `--dsw-specific-sidebar-fill`, rather than
/// sampling pixels: the token is what dsh itself paints the sidebar with, so a
/// third-party theme that redefines it is picked up for free, and there is no
/// dependence on where elements happen to land. Pixel sampling is kept only as
/// a fallback for a theme that drops the token entirely.
const THEME_WATCH_JS: &str = r#"
(function () {
  // Only the dsh page reports, and it lives one level down in the shell's
  // iframe. Two frames run this script:
  //
  //   shell  (top-level, our index.html) -- has no dsh tokens to read, so its
  //          reading is meaningless; it is the thing being coloured.
  //   dsh    (first-level subframe)      -- the one we actually want.
  //
  // Skip anything deeper than one level too: dsh may frame things itself, and a
  // nested frame reporting would fight the real page over the title bar colour.
  //
  // This was `window.top !== window && !document.body`, which was inverted. The
  // script is injected at document-start, so `document.body` is null in *both*
  // frames; the shell fell through and reported (finding no token, and sampling
  // the iframe element's own background back), while dsh returned early and
  // never reported at all. The bar then sat on the hardcoded default forever --
  // which happens to match dsh's dark theme, so it looked correct until you
  // switched to light.
  if (window.top === window || window.parent !== window.top) return;

  function parseRgb(v) {
    if (!v) return null;
    var m = v.match(/[\d.]+/g);
    if (!m || m.length < 3) return null;
    // Fully transparent means "whatever is behind me", so it tells us nothing.
    if (m.length >= 4 && +m[3] === 0) return null;
    return [+m[0], +m[1], +m[2]];
  }

  // Resolve a CSS custom property to concrete RGB. getComputedStyle already
  // follows var() chains, so a token defined as var(--something-else) still
  // comes back as an rgb() triple.
  function readToken(name) {
    try {
      var v = getComputedStyle(document.body).getPropertyValue(name);
      return parseRgb(v && v.trim());
    } catch (e) {
      return null;
    }
  }

  // Fallback for a theme that removed the token: walk up from the left edge at
  // mid-height, which is where the sidebar sits, until something is opaque.
  function sampleSidebar() {
    try {
      var y = Math.floor(window.innerHeight / 2);
      var el = document.elementFromPoint(8, y);
      while (el) {
        var c = parseRgb(getComputedStyle(el).backgroundColor);
        if (c) return c;
        el = el.parentElement;
      }
    } catch (e) {}
    return null;
  }

  function readTheme() {
    // dsh paints its sidebar with this token, so matching it makes the title
    // bar continuous with the sidebar instead of merely "dark" or "light".
    var bg = readToken('--dsw-specific-sidebar-fill') ||
      sampleSidebar() ||
      parseRgb(getComputedStyle(document.body).backgroundColor) ||
      [27, 27, 28];
    var lum = 0.299 * bg[0] + 0.587 * bg[1] + 0.114 * bg[2];
    // Trust dsh's own attribute over luminance; fall back to luminance when a
    // third-party theme does not set it.
    var dark = document.body
      ? document.body.hasAttribute('data-ds-dark-theme') || lum < 130
      : lum < 130;
    // Derive the foreground from what we sampled instead of reading a token.
    // Verified in a real Chromium against dsh's CSS: the plausible-looking
    // tokens are traps. --dsw-alias-label-primary-foreground is *inverted*
    // (white in the light theme, near-black in the dark one -- it is the
    // foreground for a filled primary element, not body text), and the
    // sidebar-nav-item-* tokens are hover/active fills. Either one would paint
    // near-invisible text. Luminance cannot be inverted, and it keeps working
    // for a third-party theme whose token set we have never seen.
    var fg = lum < 130 ? [255, 255, 255] : [15, 17, 21];
    return { bg: bg, fg: fg, dark: dark };
  }
  var eventBroken = false; // latched once we learn the Tauri bridge is unusable
  var lastMarked = null;   // theme currently encoded into document.title

  function hex(c) {
    return ('00' + c.toString(16)).slice(-2);
  }

  function markTitle(t) {
    // Fallback channel for when the event bridge is unusable: encode the
    // colours into document.title as [dsh:RRGGBB:RRGGBB]. The host polls it,
    // applies them, then strips the marker. Only rewrite on an actual change,
    // otherwise we fight the host for the title every tick.
    var enc = hex(t.bg[0]) + hex(t.bg[1]) + hex(t.bg[2]) + ':' +
      hex(t.fg[0]) + hex(t.fg[1]) + hex(t.fg[2]);
    if (lastMarked === enc) return;
    lastMarked = enc;
    try {
      var s = document.title.replace(/^\[dsh:[0-9a-f]{6}:[0-9a-f]{6}\]/i, '');
      document.title = '[dsh:' + enc + ']' + (s || 'DSH Desktop');
    } catch (e) {}
  }

  function report() {
    var t = readTheme();
    if (!eventBroken) {
      try {
        // Primary channel: Tauri event. Only reaches the host if a capability
        // declares remote.urls for this origin (capabilities/remote-theme.json).
        var p = window.__TAURI__ && window.__TAURI__.event &&
          window.__TAURI__.event.emit('dsh-theme', t);
        if (p) {
          // emit() is async, so a capability rejection lands in the promise --
          // not as a synchronous throw. Latch the fallback when that happens.
          if (p.catch) {
            p.catch(function () { eventBroken = true; markTitle(t); });
          }
          return;
        }
        eventBroken = true; // no bridge injected on this page at all
      } catch (e) {
        eventBroken = true;
      }
    }
    markTitle(t);
  }
  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', report);
  } else {
    report();
  }
  setInterval(report, 800);
  try {
    var obs = new MutationObserver(function () { report(); });
    var opts = {
      attributes: true,
      attributeFilter: ['class', 'style', 'data-theme', 'data-ds-dark-theme'],
    };
    // dsh flips the theme by toggling data-ds-dark-theme on <body>, so watching
    // only <html> (as this did before) missed every theme change. Watch both:
    // <html> carries colorScheme, <body> carries the attribute that matters.
    obs.observe(document.documentElement, opts);
    if (document.body) obs.observe(document.body, opts);
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
        .invoke_handler(tauri::generate_handler![
            current_status,
            install_dsh,
            retry_boot,
            open_node_site
        ])
        .setup(|app| {
            let handle = app.handle().clone();
            setup_tray(&handle)?;

            // The window stays on our own shell page for its whole life: the
            // shell draws the title bar, hosts the boot/guidance views, and
            // later hosts the dsh GUI in an iframe. It is never navigated away,
            // which is what lets the title bar be any colour -- a native caption
            // cannot be tinted before Windows 11.
            //
            // decorations(false) removes the native caption. The replacement is
            // in ui/index.html, and the window control permissions it needs are
            // in capabilities/default.json.
            let window =
                WebviewWindowBuilder::new(&handle, "main", WebviewUrl::App("index.html".into()))
                    .title("DSH Desktop")
                    .inner_size(1280.0, 840.0)
                    .min_inner_size(800.0, 600.0)
                    .center()
                    .decorations(false)
                    // for_all_frames: the sampler has to run *inside* the
                    // iframe, since the shell cannot read across origins into
                    // the dsh page to find out what colour it is.
                    .initialization_script_for_all_frames(THEME_WATCH_JS)
                    .build()
                    .map_err(|e| format!("failed to create main window: {e}"))?;

            // Relay colours from the dsh page to the shell's title bar.
            let theme_handle = handle.clone();
            let _ = handle.listen("dsh-theme", move |event| {
                let theme = serde_json::from_str::<Theme>(event.payload()).unwrap_or_default();
                if let Some(w) = theme_handle.get_webview_window("main") {
                    apply_theme(&w, theme);
                }
            });

            // Until the page reports in, assume dsh's dark theme.
            apply_theme(&window, Theme::default());

            // Fallback channel: when the event bridge is unusable the injected
            // script encodes the colours into document.title as
            // [dsh:RRGGBB:RRGGBB]. Poll for that, apply it, then strip the
            // marker so it never shows in the taskbar. The script only re-marks
            // on an actual change, so this settles instead of looping.
            let title_window = window.clone();
            std::thread::spawn(move || loop {
                let title = title_window.title().unwrap_or_default();
                if let Some((theme, len)) = parse_title_marker(&title) {
                    apply_theme(&title_window, theme);
                    let _ = title_window.set_title(&title[len..]);
                }
                std::thread::sleep(Duration::from_millis(800));
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
            // The guard does the work: `take_backend` kills the child we own, and
            // answers None when there is nothing to kill (never started, or we
            // are reusing a backend that belongs to somebody else).
            RunEvent::Exit if take_backend(app_handle).is_some() => {
                // A backend that shut down cleanly released its own lock; one we
                // had to force did not. Clear it either way, so a dsh started
                // outside this app (browser tab) is not blocked by our leftovers.
                clear_stale_task_board_lock();
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
    NoDsh {
        has_npm: bool,
    },
}

fn preflight() -> Preflight {
    if has_command("dsh") {
        Preflight::Ready
    } else {
        Preflight::NoDsh {
            has_npm: has_command("npm"),
        }
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
        show_gui(app, &existing);
        return Ok(());
    }

    // Nothing to start if the command is missing. Report it on the page instead
    // of dying silently — previously this path just exited after a ~6s flash of
    // an empty window, with the shell's "command not found" going nowhere.
    match preflight() {
        Preflight::Ready => {}
        Preflight::NoDsh { has_npm } => {
            emit_status(
                app,
                if has_npm {
                    "missing-dsh"
                } else {
                    "missing-node"
                },
                Vec::new(),
            );
            return Ok(());
        }
    }

    // A backend we killed on a previous exit never got to release the task-board
    // ledger lock; drop it now or the plugin tree refuses to load. See
    // clear_stale_task_board_lock.
    clear_stale_task_board_lock();
    // A retry after a failed start may still be holding a dead (or dying) child.
    take_backend(app);

    let mut spawned = spawn_backend().map_err(|e| vec![format!("无法启动 `dsh web`：{e}")])?;
    let stderr = spawned.stderr.take();
    let log_file = backend_log_path(app).map_err(|e| vec![format!("无法打开后端日志：{e}")])?;
    let (url_tx, url_rx) = mpsc::channel::<String>();
    // One thread owns the log file; both pipes feed it through `log_tx`. The
    // receiver disconnects once it has written everything it will ever write, so
    // a failure path can summarize a complete log instead of racing it.
    let (log_tx, log_done) = spawn_logger(log_file.clone());

    // Tie the child to a job object so the whole `dsh web` tree (node, and any
    // cloudflared helper it spawns) dies with us even on paths where the
    // RunEvent::Exit cleanup never runs: panic, Task Manager kill, logoff.
    #[cfg(windows)]
    confine_to_job(&spawned.child);

    if let Some(state) = app.try_state::<Backend>() {
        *state.0.lock().unwrap() = Some(spawned.child);
    }

    // Reader thread: keeps stdout + stderr pipes open for the backend's whole
    // lifetime (dropping them early could EPIPE-crash node), logs both streams,
    // and forwards the first `dsh web: http://...` line to us.
    let err_log = log_tx.clone();
    std::thread::spawn(move || {
        let mut stdout = spawned.reader;
        let mut sent = false;

        if let Some(err) = stderr {
            std::thread::spawn(move || pump_log(err, err_log, "[err]"));
        } else {
            drop(err_log);
        }

        // Bytes rather than `read_line`, for the same reason as `pump_log`: one
        // non-UTF-8 byte from `cmd` would otherwise end the loop and we would
        // stop watching for the URL.
        let mut buf = Vec::new();
        loop {
            buf.clear();
            match stdout.read_until(b'\n', &mut buf) {
                Ok(0) | Err(_) => break, // backend exited
                Ok(_) => {
                    let text = String::from_utf8_lossy(&buf);
                    let text = text.trim_end();
                    if !text.is_empty() {
                        let _ = log_tx.send(format!("[out] {text}"));
                    }
                    if let Some(url) = extract_url(text) {
                        if !sent {
                            sent = true;
                            let _ = url_tx.send(url);
                        }
                    }
                }
            }
        }
        // Drop our logger handle so the logger can finish once stderr's is gone.
        drop(log_tx);
        // Backend exited: notify the waiter so it can act (navigate on success
        // was already handled; on failure report why).
        let _ = url_tx.send(String::new());
    });

    // Wait (bounded) for the backend URL, then hand it to the shell's iframe.
    match url_rx.recv_timeout(BACKEND_READY_TIMEOUT) {
        Ok(url) if !url.is_empty() => {
            show_gui(app, &url);
            // Watch for the backend dying under us. This used to call exit(0),
            // on the reasoning that the window must not sit on a dead page --
            // which was the only option back when the window navigated *to* the
            // backend: there was no page of ours left to report on. Now the
            // window never leaves the shell, so we can hide the iframe and show
            // the error view with its retry button instead of vanishing
            // mid-session, which is the worst thing a desktop app can do.
            let app = app.clone();
            let log_file = log_file.clone();
            std::thread::spawn(move || {
                let _ = url_rx.recv();
                // Let the logger finish before summarizing, or we read a file
                // the backend's dying words have not reached yet.
                let _ = log_done.recv_timeout(Duration::from_secs(2));
                let mut detail = vec!["后端 `dsh web` 意外退出了。".to_string()];
                detail.extend(backend_error_summary(&log_file));
                detail.push(format!("完整日志：{}", log_file.display()));
                emit_status(&app, "error", detail);
            });
            Ok(())
        }
        _ => {
            // Let the logger finish before reading the log, or we summarize a
            // file the backend's own error has not reached yet.
            let _ = log_done.recv_timeout(Duration::from_secs(2));
            eprintln!(
                "dsh backend never became ready (see log: {})",
                log_file.display()
            );
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

/// Point the shell's iframe at the backend GUI.
///
/// The window itself is never navigated: it has to stay on our page so the
/// self-drawn title bar survives. Retried because the backend can be ready
/// before the shell has finished loading and `eval` cannot report whether the
/// handler existed yet; `__dshSetFrame` is idempotent.
fn show_gui(app: &tauri::AppHandle, url: &str) {
    // Validate before handing it to the page: `probe_existing_web` builds its
    // URL from a port number, but the spawned backend's comes out of parsed
    // stdout, so this is the boundary where a malformed one should stop.
    let parsed = match Url::parse(url) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("bad backend URL {url:?}: {e}");
            return;
        }
    };
    let Some(window) = app.get_webview_window("main") else {
        return;
    };
    // Cache it as the current status too. The window no longer navigates, so
    // the shell page can be reloaded while the GUI is up; without this it would
    // ask for the status, be told "booting", and sit on the splash forever.
    emit_status(app, "ready", vec![parsed.as_str().to_string()]);

    let js = format!(
        "window.__dshSetFrame && window.__dshSetFrame({})",
        serde_json::to_string(parsed.as_str()).unwrap_or_else(|_| "\"\"".into()),
    );
    for attempt in 0..6 {
        if attempt > 0 {
            std::thread::sleep(Duration::from_millis(350));
        }
        let _ = window.eval(js.as_str());
    }
}

/// Parse the `[dsh:RRGGBB:RRGGBB]` prefix the injected script writes into
/// document.title when the event bridge is unusable. Returns the theme and the
/// byte length of the marker so the caller can strip it.
///
/// The marker is fixed-width by construction, so anything that does not match
/// exactly is treated as a normal title rather than partially parsed.
fn parse_title_marker(title: &str) -> Option<(Theme, usize)> {
    const LEN: usize = "[dsh:rrggbb:rrggbb]".len();
    let body = title.strip_prefix("[dsh:")?;
    let end = body.find(']')?;
    let (hexes, _) = body.split_at(end);
    let (bg, fg) = hexes.split_once(':')?;
    if bg.len() != 6 || fg.len() != 6 {
        return None;
    }
    let rgb = |s: &str| -> Option<[u8; 3]> {
        Some([
            u8::from_str_radix(&s[0..2], 16).ok()?,
            u8::from_str_radix(&s[2..4], 16).ok()?,
            u8::from_str_radix(&s[4..6], 16).ok()?,
        ])
    };
    let bg = rgb(bg)?;
    let fg = rgb(fg)?;
    let lum = 0.299 * bg[0] as f32 + 0.587 * bg[1] as f32 + 0.114 * bg[2] as f32;
    Some((
        Theme {
            bg,
            fg,
            dark: lum < 130.0,
        },
        LEN,
    ))
}

/// Forward one of the backend's output streams into the log channel, tagging
/// each line with `prefix` so `[out]` and `[err]` stay distinguishable after the
/// two are interleaved into one file.
///
/// Reads bytes rather than `read_line`: on Windows the failure text can come
/// from `cmd` itself in the console's OEM codepage, and a `String`-based read
/// errors out on the first such byte and abandons the rest of the stream — which
/// is exactly the case the log exists to explain. Lossy decoding keeps the line.
fn pump_log<R: std::io::Read>(stream: R, log: mpsc::Sender<String>, prefix: &str) {
    let mut reader = BufReader::new(stream);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf) {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                let text = String::from_utf8_lossy(&buf);
                let text = text.trim_end();
                if !text.is_empty() && log.send(format!("{prefix} {text}")).is_err() {
                    break; // logger is gone
                }
            }
        }
    }
}

/// Size at which the backend log rolls over to `dsh-backend.log.1`.
const LOG_MAX_BYTES: u64 = 5 * 1024 * 1024;

/// Spawn the single thread that owns the log file. Both output pipes feed it
/// through the returned sender, so the readers never contend for the handle and
/// rotation has exactly one owner.
///
/// The file is truncated on open, not appended to: the log documents the current
/// launch, and `backend_error_summary` reads the whole thing — carrying yesterday's
/// stack traces in would attribute them to this boot. Rotation then bounds growth
/// within a long-running session, where a chatty backend could otherwise fill the
/// disk.
///
/// The returned receiver disconnects when the thread has written everything it
/// will ever write, which is how a failure path knows the log is complete.
fn spawn_logger(path: PathBuf) -> (mpsc::Sender<String>, mpsc::Receiver<()>) {
    use std::io::Write;

    let (tx, rx) = mpsc::channel::<String>();
    let (done_tx, done_rx) = mpsc::channel::<()>();
    std::thread::spawn(move || {
        // Held only so dropping it on thread exit wakes the waiter.
        let _done = done_tx;
        let mut file = open_log(&path, true);
        let mut size = 0u64;

        for line in rx {
            if size + line.len() as u64 > LOG_MAX_BYTES {
                // Close before renaming: Windows will not rename an open file.
                drop(file.take());
                let rotated = path.with_extension("log.1");
                let _ = std::fs::remove_file(&rotated);
                let _ = std::fs::rename(&path, &rotated);
                file = open_log(&path, true);
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
    (tx, done_rx)
}

fn open_log(path: &std::path::Path, truncate: bool) -> Option<std::fs::File> {
    std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(truncate)
        .append(!truncate)
        .open(path)
        .ok()
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
        text.strip_prefix('"')
            .and_then(|rest| rest.split('"').next())
    } else {
        Some(text.trim()).filter(|t| !t.is_empty())
    };

    image.is_some_and(pred)
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
        // Both streams share the file, each line tagged; strip either tag.
        let text = line
            .trim_start_matches("[out] ")
            .trim_start_matches("[err] ")
            .trim();
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
    fn parses_a_title_colour_marker() {
        let (theme, len) = parse_title_marker("[dsh:0f1115:e2e8f0]DeepSeek Harness").unwrap();
        assert_eq!(theme.bg, [0x0f, 0x11, 0x15]);
        assert_eq!(theme.fg, [0xe2, 0xe8, 0xf0]);
        assert!(theme.dark, "0f1115 is dark");
        // The caller strips by this length, so it must land exactly after ']'.
        assert_eq!(
            &"[dsh:0f1115:e2e8f0]DeepSeek Harness"[len..],
            "DeepSeek Harness"
        );
    }

    #[test]
    fn derives_dark_from_the_sampled_colour() {
        // A light theme, and an arbitrary third-party one, both classified by
        // luminance rather than a hardcoded list.
        assert!(!parse_title_marker("[dsh:f9fafb:1f2937]x").unwrap().0.dark);
        assert!(parse_title_marker("[dsh:2d1b4e:ffffff]x").unwrap().0.dark);
    }

    #[test]
    fn rejects_malformed_title_markers() {
        assert!(parse_title_marker("DeepSeek Harness").is_none());
        assert!(parse_title_marker("[dsh:0f1115]x").is_none()); // one colour
        assert!(parse_title_marker("[dsh:0f111:e2e8f0]x").is_none()); // short hex
        assert!(parse_title_marker("[dsh:zzzzzz:e2e8f0]x").is_none()); // not hex
        assert!(parse_title_marker("[dsh:0f1115:e2e8f0").is_none()); // unterminated
    }

    #[test]
    fn ignores_lines_without_a_url() {
        assert_eq!(extract_url("Error: EADDRINUSE"), None);
        assert_eq!(extract_url(""), None);
        // https is never the local GUI; we only ever serve plain http.
        assert_eq!(extract_url("https://127.0.0.1:3080"), None);
    }
}
