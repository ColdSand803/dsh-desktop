// Prevents an extra console window on Windows in release builds.
// DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::io::{BufRead, BufReader};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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
///
/// `generation` counts how many backends we have retired, and exists to tell
/// "the backend died on its own" apart from "we killed it on purpose". Both look
/// identical from the reader thread — stdout hits EOF either way — so without
/// this, every deliberate replacement (retry button, upgrade, exit) reported
/// itself to the user as an unexpected crash. `take_backend` bumps it; each
/// watcher remembers the generation it was born into and stays quiet once it is
/// no longer the current one. See the watcher in `try_boot`.
struct Backend {
    child: Mutex<Option<Child>>,
    generation: AtomicU64,
}

impl Default for Backend {
    fn default() -> Self {
        Self {
            child: Mutex::new(None),
            generation: AtomicU64::new(0),
        }
    }
}

/// Guards against two boot sequences running at once (double-clicked retry, or a
/// retry landing while the install-triggered boot is still going).
struct BootLock(AtomicBool);

/// Last status pushed to the boot page, so a page that loads (or reloads) after
/// the event fired can still ask for it. Without this the window could sit on
/// the splash forever having missed the one event that mattered.
struct Status(Mutex<serde_json::Value>);

/// Last dsh version comparison, cached for the same reason `Status` is: the
/// check runs on its own thread well after the page has loaded, so a page that
/// reloads afterwards would otherwise never learn an update is waiting.
///
/// `auto_checked` keeps the automatic check to once per session — `boot_sequence`
/// can run several times (install, retry button) and each pass would otherwise
/// spend another registry round trip to learn the same thing.
struct DshVersion {
    payload: Mutex<serde_json::Value>,
    auto_checked: AtomicBool,
}

impl Default for DshVersion {
    fn default() -> Self {
        Self {
            payload: Mutex::new(version_payload(
                "unknown",
                DEFAULT_CHANNEL,
                None,
                None,
                None,
            )),
            auto_checked: AtomicBool::new(false),
        }
    }
}

/// Same idea as `DshVersion`, for this binary's own update check. No
/// `auto_checked`: the shell check never runs on its own, only when asked.
struct ShellVersion {
    payload: Mutex<serde_json::Value>,
}

impl Default for ShellVersion {
    fn default() -> Self {
        Self {
            payload: Mutex::new(version_payload(
                "unknown",
                DEFAULT_CHANNEL,
                None,
                None,
                None,
            )),
        }
    }
}

/// The package that provides the `dsh` command, and the site to send users to
/// when they have no Node.js at all. Both are constants: `install_dsh` and
/// `open_node_site` take no arguments from the page, so it cannot talk us into
/// installing an arbitrary package or opening an arbitrary URL.
const DSH_PACKAGE: &str = "@deepseek-ai/dsh";
const NODE_SITE: &str = "https://nodejs.org/";

/// Release channels the update panel offers, for both the shell and dsh. For dsh
/// these are npm dist-tags; for the shell they select an updater manifest (see
/// `SHELL_MANIFESTS`).
const CHANNELS: [&str; 2] = ["latest", "alpha"];
const DEFAULT_CHANNEL: &str = "latest";

/// Map whatever the page sent to one of `CHANNELS`, falling back to the default.
///
/// A fixed allowlist rather than a free string because the dsh side interpolates
/// this into an `npm view` command line. Returning `&'static str` is what makes
/// that safe by construction: nothing downstream can see a value that did not
/// come from the table above.
fn normalize_channel(raw: &str) -> &'static str {
    let raw = raw.trim();
    CHANNELS
        .iter()
        .copied()
        .find(|c| c.eq_ignore_ascii_case(raw))
        .unwrap_or(DEFAULT_CHANNEL)
}

/// The channel selected for each side, and the file it is remembered in.
///
/// Kept host-side rather than in the page's localStorage because the startup dsh
/// check — the one that lights up the title bar pill — runs before the page can
/// say anything, and it has to check the channel the user actually follows. The
/// tray's check reads it for the same reason.
struct Channels {
    dsh: Mutex<&'static str>,
    shell: Mutex<&'static str>,
}

impl Default for Channels {
    fn default() -> Self {
        Self {
            dsh: Mutex::new(DEFAULT_CHANNEL),
            shell: Mutex::new(DEFAULT_CHANNEL),
        }
    }
}

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
            current_dsh_version,
            current_shell_version,
            current_channels,
            set_channel,
            check_dsh_update,
            check_shell_update,
            update_shell,
            install_dsh,
            upgrade_dsh,
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
            app.manage(Backend::default());
            app.manage(BootLock(AtomicBool::new(false)));
            app.manage(Status(Mutex::new(status_payload("booting", Vec::new()))));
            app.manage(DshVersion::default());
            app.manage(ShellVersion::default());
            // Before the boot thread starts: its automatic dsh check has to look
            // at the channel the user actually follows, not the default.
            app.manage(Channels::default());
            load_channels(&handle);

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

/// Whether the backend now serving is one we started, as opposed to one we found
/// already running on the probe port and reused.
///
/// Decides two things that both hinge on it: whether we may replace the files it
/// is executing from (`upgrade_dsh_now`), and whether stopping ours costs the user
/// anything (the page's update confirmation). `try_boot` stores the child before
/// it waits for the URL, so this is already true by the time the GUI is shown.
fn backend_is_ours(app: &tauri::AppHandle) -> bool {
    app.try_state::<Backend>()
        .is_some_and(|s| s.child.lock().unwrap().is_some())
}

/// Whether the window is currently showing the dsh GUI, read off the last status
/// we pushed — which is by definition what the page is displaying.
///
/// Used to decide whether an error may take over the content area. Replacing a
/// working GUI (possibly with a session in it) with an error page is a heavy
/// answer to "the update check failed"; when the GUI is up, the update panel
/// reports the failure itself and the iframe is left alone.
fn gui_is_up(app: &tauri::AppHandle) -> bool {
    app.try_state::<Status>()
        .map(|s| s.0.lock().unwrap()["state"] == "ready")
        .unwrap_or(false)
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

    // Once the app is usable, find out whether the dsh it just started is
    // current. Deliberately after the boot rather than before it: the check
    // costs a registry round trip, and a slow or unreachable registry must not
    // delay the thing the user actually asked for. Nothing is installed
    // automatically — see `auto_check_dsh_version`.
    auto_check_dsh_version(&app);
}

/// Where the selected channels are remembered between sessions.
fn channels_file(app: &tauri::AppHandle) -> Option<PathBuf> {
    let dir = app.path().app_config_dir().ok()?;
    Some(dir.join("channels.json"))
}

/// Load the remembered channels into the managed state. Every value read back is
/// pushed through `normalize_channel`, so a hand-edited or corrupt file degrades
/// to the default instead of reaching a command line.
fn load_channels(app: &tauri::AppHandle) {
    let Some(state) = app.try_state::<Channels>() else {
        return;
    };
    let Some(text) = channels_file(app).and_then(|p| std::fs::read_to_string(p).ok()) else {
        return;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return;
    };
    if let Some(c) = value.get("dsh").and_then(|v| v.as_str()) {
        *state.dsh.lock().unwrap() = normalize_channel(c);
    }
    if let Some(c) = value.get("shell").and_then(|v| v.as_str()) {
        *state.shell.lock().unwrap() = normalize_channel(c);
    }
}

fn save_channels(app: &tauri::AppHandle) {
    let Some(state) = app.try_state::<Channels>() else {
        return;
    };
    let payload = serde_json::json!({
        "dsh": *state.dsh.lock().unwrap(),
        "shell": *state.shell.lock().unwrap(),
    });
    let Some(path) = channels_file(app) else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // Best effort: losing the preference costs one wrong default next launch,
    // which is not worth interrupting the user over.
    let _ = std::fs::write(path, payload.to_string());
}

fn dsh_channel(app: &tauri::AppHandle) -> &'static str {
    app.try_state::<Channels>()
        .map(|s| *s.dsh.lock().unwrap())
        .unwrap_or(DEFAULT_CHANNEL)
}

fn shell_channel(app: &tauri::AppHandle) -> &'static str {
    app.try_state::<Channels>()
        .map(|s| *s.shell.lock().unwrap())
        .unwrap_or(DEFAULT_CHANNEL)
}

/// Remember the channel the panel switched to. `which` picks the side; anything
/// else is ignored rather than erroring, since the page is the only caller.
#[tauri::command]
fn set_channel(app: tauri::AppHandle, which: String, channel: String) {
    let channel = normalize_channel(&channel);
    let Some(state) = app.try_state::<Channels>() else {
        return;
    };
    match which.as_str() {
        "dsh" => *state.dsh.lock().unwrap() = channel,
        "shell" => *state.shell.lock().unwrap() = channel,
        _ => return,
    }
    save_channels(&app);
}

/// The panel asks for this on open, so its two selects start on the right values.
#[tauri::command]
fn current_channels(app: tauri::AppHandle) -> serde_json::Value {
    serde_json::json!({
        "dsh": dsh_channel(&app),
        "shell": shell_channel(&app),
        "available": CHANNELS,
    })
}

/// The startup version check: run at most once per session, report the result to
/// the page, and never act on it. An update that replaces `dsh` under a running
/// backend has to be the user's decision, so this only lights up the title bar's
/// pill; the upgrade itself goes through `upgrade_dsh`.
fn auto_check_dsh_version(app: &tauri::AppHandle) {
    let Some(state) = app.try_state::<DshVersion>() else {
        return;
    };
    // Nothing to compare against until dsh exists. Checked before the flag is
    // consumed, so the pass that follows a first-time install still gets its
    // chance -- otherwise the "missing dsh" boot would burn the one check.
    if !has_command("dsh") {
        return;
    }
    if state.auto_checked.swap(true, Ordering::SeqCst) {
        return;
    }
    let channel = dsh_channel(app);
    emit_dsh_version(app, "checking", channel, None, None, None);
    let result = check_dsh_version(channel);
    report_dsh_version(app, &result);
}

/// Where a channel's version sits relative to what is installed.
///
/// `Older` exists because switching channels is a supported move: going from an
/// alpha back to the stable line is a downgrade, and the panel has to be able to
/// offer it rather than reporting "already current" and refusing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum VersionRelation {
    Same,
    Newer,
    Older,
}

impl VersionRelation {
    /// The `state` string the page switches on.
    fn state(self) -> &'static str {
        match self {
            VersionRelation::Same => "current",
            VersionRelation::Newer => "outdated",
            VersionRelation::Older => "rollback",
        }
    }
}

/// Compare a channel's version against the installed one.
fn relate(target: &str, installed: &str) -> VersionRelation {
    match compare_versions(target, installed) {
        std::cmp::Ordering::Greater => VersionRelation::Newer,
        std::cmp::Ordering::Less => VersionRelation::Older,
        std::cmp::Ordering::Equal => VersionRelation::Same,
    }
}

/// What the version check found. Only `Compared { relation: Newer }` lights up
/// the title bar pill; everything else is for the panel to render.
enum VersionCheck {
    /// Both versions are known and were compared.
    Compared {
        installed: String,
        channel: &'static str,
        target: String,
        relation: VersionRelation,
    },
    /// The registry answered, but this channel has nothing published under it.
    ChannelEmpty {
        installed: String,
        channel: &'static str,
    },
    /// dsh is not installed, so there is nothing to compare.
    NotInstalled,
    /// The check itself failed (no npm, no network, unparseable output).
    Failed {
        installed: Option<String>,
        channel: &'static str,
        reason: String,
    },
}

/// Ask the local dsh for its version and the registry for what `channel` points
/// at, and compare them.
///
/// Blocking, and slow enough to matter (two process spawns, one of them network
/// bound), so every caller runs it off the UI thread.
fn check_dsh_version(channel: &'static str) -> VersionCheck {
    if !has_command("dsh") {
        return VersionCheck::NotInstalled;
    }
    let installed = match installed_dsh_version() {
        Some(v) => v,
        None => {
            return VersionCheck::Failed {
                installed: None,
                channel,
                reason: "无法确定本机 dsh 的版本（`dsh --version` 没有输出可识别的版本号）。"
                    .into(),
            }
        }
    };
    if !has_command("npm") {
        return VersionCheck::Failed {
            installed: Some(installed),
            channel,
            reason: "PATH 中没有 npm，无法查询最新版本。".into(),
        };
    }
    let tags = match dsh_dist_tags() {
        Some(t) => t,
        None => {
            return VersionCheck::Failed {
                installed: Some(installed),
                channel,
                reason: format!(
                    "无法从 npm 查询 {DSH_PACKAGE} 的版本信息，通常是网络或注册表配置问题。"
                ),
            }
        }
    };
    match channel_dsh_version(&tags, channel) {
        ChannelLookup::Version(target) => {
            let relation = relate(&target, &installed);
            VersionCheck::Compared {
                installed,
                channel,
                target,
                relation,
            }
        }
        ChannelLookup::Missing => VersionCheck::ChannelEmpty { installed, channel },
        ChannelLookup::Failed => VersionCheck::Failed {
            installed: Some(installed),
            channel,
            reason: format!("{DSH_PACKAGE} 的 {channel} 通道返回了无法识别的版本号。"),
        },
    }
}

/// Cache a check result and push it to the page.
fn report_dsh_version(app: &tauri::AppHandle, result: &VersionCheck) {
    match result {
        VersionCheck::Compared {
            installed,
            channel,
            target,
            relation,
        } => emit_dsh_version(
            app,
            relation.state(),
            channel,
            Some(installed),
            Some(target),
            None,
        ),
        VersionCheck::ChannelEmpty { installed, channel } => {
            emit_dsh_version(app, "channel-empty", channel, Some(installed), None, None)
        }
        VersionCheck::NotInstalled => {
            emit_dsh_version(app, "not-installed", dsh_channel(app), None, None, None)
        }
        VersionCheck::Failed {
            installed,
            channel,
            reason,
        } => emit_dsh_version(
            app,
            "error",
            channel,
            installed.as_deref(),
            None,
            Some(reason),
        ),
    }
}

/// Shape of the `dsh-version` and `shell-version` events; `state` is what the
/// page switches on. `target` is the version the selected channel points at,
/// which may be older than `installed` when the channel was just switched.
fn version_payload(
    state: &str,
    channel: &str,
    installed: Option<&str>,
    target: Option<&str>,
    reason: Option<&str>,
) -> serde_json::Value {
    serde_json::json!({
        "state": state,
        "channel": channel,
        "installed": installed,
        "target": target,
        "reason": reason,
    })
}

/// `(installed, target)` from the last dsh report, for statuses emitted before a
/// fresh check has run. Both are `None` before the first check.
fn cached_dsh_versions(app: &tauri::AppHandle) -> (Option<String>, Option<String>) {
    let field = |p: &serde_json::Value, k: &str| p[k].as_str().map(str::to_string);
    app.try_state::<DshVersion>()
        .map(|s| {
            let p = s.payload.lock().unwrap();
            (field(&p, "installed"), field(&p, "target"))
        })
        .unwrap_or((None, None))
}

fn emit_dsh_version(
    app: &tauri::AppHandle,
    state: &str,
    channel: &str,
    installed: Option<&str>,
    target: Option<&str>,
    reason: Option<&str>,
) {
    eprintln!("dsh version: {state} on {channel} (installed {installed:?}, target {target:?})");
    let payload = version_payload(state, channel, installed, target, reason);
    if let Some(store) = app.try_state::<DshVersion>() {
        *store.payload.lock().unwrap() = payload.clone();
    }
    let _ = app.emit("dsh-version", payload);
}

/// The channel target from the last shell report. The installed version needs no
/// cache — it is `package_info()`.
fn cached_shell_target(app: &tauri::AppHandle) -> Option<String> {
    app.try_state::<ShellVersion>().and_then(|s| {
        s.payload.lock().unwrap()["target"]
            .as_str()
            .map(str::to_string)
    })
}

fn emit_shell_version(
    app: &tauri::AppHandle,
    state: &str,
    channel: &str,
    installed: Option<&str>,
    target: Option<&str>,
    reason: Option<&str>,
) {
    eprintln!("shell version: {state} on {channel} (installed {installed:?}, target {target:?})");
    let payload = version_payload(state, channel, installed, target, reason);
    if let Some(store) = app.try_state::<ShellVersion>() {
        *store.payload.lock().unwrap() = payload.clone();
    }
    let _ = app.emit("shell-version", payload);
}

/// The page asks for these on load, in case it missed the events.
#[tauri::command]
fn current_dsh_version(app: tauri::AppHandle) -> serde_json::Value {
    app.try_state::<DshVersion>()
        .map(|s| s.payload.lock().unwrap().clone())
        .unwrap_or_else(|| version_payload("unknown", DEFAULT_CHANNEL, None, None, None))
}

#[tauri::command]
fn current_shell_version(app: tauri::AppHandle) -> serde_json::Value {
    app.try_state::<ShellVersion>()
        .map(|s| s.payload.lock().unwrap().clone())
        .unwrap_or_else(|| version_payload("unknown", DEFAULT_CHANNEL, None, None, None))
}

/// Run a dsh check for `channel` off the UI thread and report it.
///
/// A plain thread rather than the async runtime: the check shells out twice and
/// blocks, which would stall other tasks on that runtime.
#[tauri::command]
fn check_dsh_update(app: tauri::AppHandle, channel: String) {
    let channel = normalize_channel(&channel);
    std::thread::spawn(move || {
        emit_dsh_version(&app, "checking", channel, None, None, None);
        let result = check_dsh_version(channel);
        report_dsh_version(&app, &result);
    });
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

    // Past the checks, so there is now something to start. The page opens on
    // "正在检查运行环境…" because up to this line that is all we were doing:
    // claiming to start DeepSeek Harness while the very next thing we might do
    // is report that dsh is not installed reads as a lie in the one case where
    // the wording matters. Say "starting" only once it is true.
    emit_status(app, "booting", vec!["正在启动 DeepSeek Harness…".into()]);

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

    // Read the generation while we store the child: `take_backend` above already
    // retired whatever came before, so this is the number our own watcher has to
    // match. Zero when the state is missing, which only happens in tests.
    let mut generation = 0;
    if let Some(state) = app.try_state::<Backend>() {
        *state.child.lock().unwrap() = Some(spawned.child);
        generation = state.generation.load(Ordering::SeqCst);
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
                if !wait_for_backend_exit(&url_rx) {
                    return;
                }
                // EOF also happens when *we* kill the backend -- retry button,
                // dsh upgrade, app exit. Those are not crashes and must not
                // raise an error view: on the upgrade path the replacement is
                // usually already serving by now, so this would paint "启动失败"
                // over a working GUI. `take_backend` bumped the generation on
                // its way out, so a stale watcher can tell and leave quietly.
                if let Some(state) = app.try_state::<Backend>() {
                    if state.generation.load(Ordering::SeqCst) != generation {
                        return;
                    }
                }
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

/// Block until the reader thread says the backend's stdout reached EOF.
///
/// Two kinds of message travel this channel and only one of them means "gone":
/// a URL (the reader matched a `dsh web: http://...` line) and the empty string
/// (EOF, so the process is finished writing). Only the first URL is forwarded
/// today, but reading *any* message as an exit would turn a second address line
/// into a phantom crash, so skip everything non-empty.
///
/// `false` means the sender was dropped without an EOF ever arriving, which is
/// an absence of news rather than a death — say nothing.
fn wait_for_backend_exit(rx: &mpsc::Receiver<String>) -> bool {
    loop {
        match rx.recv() {
            Ok(line) if !line.is_empty() => continue,
            Ok(_) => return true,
            Err(_) => return false,
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
    //
    // The second element says whether this backend is ours. The update panel needs
    // it to be accurate about what an update costs: stopping ours interrupts
    // whatever it was doing, while a reused one in a browser tab is untouched by a
    // shell restart and refuses a dsh upgrade outright.
    emit_status(
        app,
        "ready",
        vec![
            parsed.as_str().to_string(),
            if backend_is_ours(app) {
                "owned".into()
            } else {
                "foreign".into()
            },
        ],
    );

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

/// Take our backend child out of the state, if we have one, and kill it.
///
/// Bumps `generation` before killing so the watcher this child's death is about
/// to wake can tell it was retired on purpose and keep quiet. The bump happens
/// even when there is no child to take: a caller that finds the slot empty may
/// still be about to replace a backend we do not own, and a stale watcher must
/// not outlive that either.
fn take_backend(app: &tauri::AppHandle) -> Option<Child> {
    let state = app.try_state::<Backend>()?;
    state.generation.fetch_add(1, Ordering::SeqCst);
    let mut child = state.child.lock().unwrap().take()?;
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
        // The bare package name: a first install just follows the default tag.
        match run_install(&app, DSH_PACKAGE) {
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

/// Flags for the install, all of them about what the page gets to show.
///
/// `--loglevel=http` is the one that matters: npm draws its progress bar only
/// when stderr is a TTY, and ours is a pipe, so at the default level the install
/// is silent for the several minutes it spends fetching a few hundred MB — the
/// log box just sits empty. At `http` npm prints one newline-terminated line per
/// tarball instead, which streams. A real percentage is not on offer: npm never
/// reports a total, so per-package lines are as close as we get.
///
/// The other two only remove noise we would otherwise have to scroll past: an
/// audit report and a funding plug, neither of which says anything about whether
/// the install worked.
const NPM_INSTALL_FLAGS: [&str; 3] = ["--loglevel=http", "--no-audit", "--no-fund"];

/// Run `npm i -g <spec>`, forwarding every output line to the page.
///
/// `spec` is either the bare package name (first install: follow the default tag)
/// or `pkg@version` with a version the caller already resolved and showed the
/// user. It is never a `pkg@tag`: a tag can move between the check and the
/// install, and the panel just promised a specific number.
fn run_install(app: &tauri::AppHandle, spec: &str) -> Result<(), String> {
    let mut cmd = if cfg!(windows) {
        // npm is a .cmd shim, so it needs a shell to run at all.
        let mut c = Command::new("cmd");
        c.args(["/C", "npm", "install", "-g"]);
        c.args(NPM_INSTALL_FLAGS);
        c.arg(spec);
        c
    } else {
        let mut c = Command::new("npm");
        c.args(["install", "-g"]);
        c.args(NPM_INSTALL_FLAGS);
        c.arg(spec);
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
            "npm install -g {spec} 失败（退出码 {}）。上面的日志里通常有原因，常见的是网络或权限问题。",
            status.code().map_or_else(|| "未知".to_string(), |c| c.to_string())
        ))
    }
}

/// Forward one output stream to the page as `dsh-install-log` events. Reads
/// bytes and decodes lossily rather than going through lines, because on Windows
/// npm's output arrives in the console's OEM codepage and is not always UTF-8.
///
/// Breaks on `\r` as well as `\n`: anything that redraws a line in place (a
/// progress bar, a spinner) returns the carriage without ever sending a newline,
/// so a `\n`-only split would hold those bytes in the buffer indefinitely and
/// release them in one lump at the end. Splitting on both turns each redraw into
/// its own line — repetitive in the log, but it arrives while it still means
/// something. `\r\n` flushes on the `\r` and leaves the `\n` looking at an empty
/// buffer, which `flush` drops, so DOS line endings do not double up.
fn pump_lines<R: std::io::Read + Send + 'static>(
    app: tauri::AppHandle,
    stream: R,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stream);
        let mut buf = Vec::new();

        let flush = |buf: &mut Vec<u8>| {
            let text = String::from_utf8_lossy(buf);
            let text = text.trim_end();
            if !text.is_empty() {
                let _ = app.emit("dsh-install-log", text);
            }
            buf.clear();
        };

        loop {
            // fill_buf/consume rather than a read_until per delimiter: it lets us
            // scan for either terminator in one pass over the buffered bytes.
            let (consumed, hit) = match reader.fill_buf() {
                Ok([]) | Err(_) => break, // EOF, or a stream we can no longer read
                Ok(available) => match available.iter().position(|&b| b == b'\n' || b == b'\r') {
                    Some(at) => {
                        buf.extend_from_slice(&available[..at]);
                        (at + 1, true) // +1 drops the terminator itself
                    }
                    None => {
                        buf.extend_from_slice(available);
                        (available.len(), false)
                    }
                },
            };
            reader.consume(consumed);
            if hit {
                flush(&mut buf);
            }
        }
        // A final line with no terminator still deserves to be shown.
        flush(&mut buf);
    })
}

/// Ask the installed dsh what version it is.
fn installed_dsh_version() -> Option<String> {
    let out = capture(&["dsh", "--version"])?;
    parse_version(&out)
}

/// What a channel resolved to.
enum ChannelLookup {
    Version(String),
    /// The package has no such dist-tag — nothing has been published there.
    Missing,
    /// The tag exists but its value is not a version we can read.
    Failed,
}

/// Ask npm for the dsh package's dist-tags: every channel in one round trip.
///
/// Deliberately npm rather than an HTTP request to registry.npmjs.org: npm
/// applies the user's own `.npmrc` — a mirror, a corporate proxy, auth — so a
/// machine that can install dsh at all can also check it. Talking to the public
/// registry directly would report "unreachable" on exactly those setups, and
/// would add an HTTP stack to a binary that currently needs none.
///
/// `dist-tags` rather than a `version` per channel because the panel shows both
/// channels at once: one spawn instead of one per channel, and it also tells a
/// tag that does not exist apart from a lookup that failed.
fn dsh_dist_tags() -> Option<serde_json::Map<String, serde_json::Value>> {
    let out = capture(&["npm", "view", DSH_PACKAGE, "dist-tags", "--json"])?;
    match serde_json::from_str::<serde_json::Value>(&out) {
        Ok(serde_json::Value::Object(map)) => Some(map),
        _ => None,
    }
}

/// Resolve one channel out of the dist-tags map.
///
/// The version is taken through `parse_version`, which both validates it and
/// bounds it to `[0-9A-Za-z.+-]`. That is what makes it safe to interpolate into
/// the `pkg@version` spec `run_install` puts on a command line.
fn channel_dsh_version(
    tags: &serde_json::Map<String, serde_json::Value>,
    channel: &str,
) -> ChannelLookup {
    match tags.get(channel).and_then(|v| v.as_str()) {
        Some(raw) => match parse_version(raw) {
            Some(v) => ChannelLookup::Version(v),
            None => ChannelLookup::Failed,
        },
        None => ChannelLookup::Missing,
    }
}

/// Run a command and return its stdout, or None if it could not run or failed.
///
/// Goes through `cmd /C` on Windows because both commands we use it for are
/// `.cmd` shims, which `CreateProcess` cannot execute directly. Output is
/// decoded lossily: npm's can arrive in the console's OEM codepage, and a
/// version number is ASCII either way.
fn capture(argv: &[&str]) -> Option<String> {
    let mut cmd = if cfg!(windows) {
        let mut c = Command::new("cmd");
        c.arg("/C");
        c.args(argv);
        c
    } else {
        let mut c = Command::new(argv[0]);
        c.args(&argv[1..]);
        c
    };
    #[cfg(windows)]
    {
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let out = cmd.stdin(Stdio::null()).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Pull the first semver-looking token out of a command's output.
///
/// Lenient about what surrounds it because the two producers disagree: `npm view`
/// prints a bare `1.2.3`, while a CLI's `--version` may print `dsh/1.2.3`, a
/// banner line, or the version among other words. Anchored on a digit run that
/// is not itself mid-number, so a token is matched whole rather than from its
/// tail.
fn parse_version(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    for (i, c) in text.char_indices() {
        if !c.is_ascii_digit() {
            continue;
        }
        // Only consider the start of a run, so "1.2.3" is not also tried at "2".
        if i > 0 && matches!(bytes[i - 1], b'0'..=b'9' | b'.') {
            continue;
        }
        let rest = &text[i..];
        let end = rest
            .find(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+')))
            .unwrap_or(rest.len());
        // A trailing '.' belongs to the sentence, not the version.
        let candidate = rest[..end].trim_end_matches('.');
        // Require at least major.minor, so a lone "8" from unrelated prose (an
        // exit code, a port) is not mistaken for a version.
        if candidate
            .split('.')
            .take(2)
            .filter(|p| !p.is_empty())
            .count()
            == 2
            && candidate.starts_with(|c: char| c.is_ascii_digit())
        {
            return Some(candidate.to_string());
        }
    }
    None
}

/// Order two versions by semver precedence.
///
/// Hand-rolled rather than pulling in the `semver` crate: this decides which way
/// the update panel's button points, and the rules that matter here are short.
/// Build metadata (`+...`) is ignored, as the spec requires; a prerelease sorts
/// below the release it precedes — which is what puts `0.1.1-rc.2` below `0.1.1`
/// and above `0.1.1-rc.1`.
fn compare_versions(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;

    // Build metadata never affects precedence.
    let strip_build = |v: &str| v.split('+').next().unwrap_or(v).to_string();
    let a = strip_build(a);
    let b = strip_build(b);

    let split = |v: &str| -> (Vec<u64>, String) {
        let (core, pre) = v.split_once('-').unwrap_or((v, ""));
        let nums = core
            .split('.')
            .map(|p| p.parse::<u64>().unwrap_or(0))
            .collect();
        (nums, pre.to_string())
    };
    let (a_nums, a_pre) = split(&a);
    let (b_nums, b_pre) = split(&b);

    // Missing components are zero: 1.2 and 1.2.0 are the same release.
    for i in 0..a_nums.len().max(b_nums.len()) {
        let ord = a_nums
            .get(i)
            .copied()
            .unwrap_or(0)
            .cmp(&b_nums.get(i).copied().unwrap_or(0));
        if ord != Ordering::Equal {
            return ord;
        }
    }

    match (a_pre.is_empty(), b_pre.is_empty()) {
        (true, true) => Ordering::Equal,
        // A release outranks any prerelease of the same core version.
        (true, false) => Ordering::Greater,
        (false, true) => Ordering::Less,
        (false, false) => compare_prerelease(&a_pre, &b_pre),
    }
}

/// Compare two prerelease strings dot-part by dot-part: numeric parts compare
/// numerically, anything else lexically, and a numeric part sorts below a
/// non-numeric one. A shorter prerelease sorts below an otherwise equal longer
/// one (`1.0.0-rc` < `1.0.0-rc.1`).
fn compare_prerelease(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;

    let mut a_parts = a.split('.');
    let mut b_parts = b.split('.');
    loop {
        match (a_parts.next(), b_parts.next()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) => {
                let ord = match (x.parse::<u64>(), y.parse::<u64>()) {
                    (Ok(nx), Ok(ny)) => nx.cmp(&ny),
                    (Ok(_), Err(_)) => Ordering::Less,
                    (Err(_), Ok(_)) => Ordering::Greater,
                    (Err(_), Err(_)) => x.cmp(y),
                };
                if ord != Ordering::Equal {
                    return ord;
                }
            }
        }
    }
}

/// Move the installed dsh to whatever the selected channel points at, from the
/// panel's action button. May be a downgrade — switching off alpha is the case
/// this exists for.
#[tauri::command]
fn upgrade_dsh(app: tauri::AppHandle, channel: Option<String>) {
    let channel = channel
        .as_deref()
        .map(normalize_channel)
        .unwrap_or_else(|| dsh_channel(&app));
    std::thread::spawn(move || upgrade_dsh_to(&app, channel));
}

/// Resolve the channel, then hand off to `upgrade_dsh_now`.
///
/// Resolved here rather than trusting a version the page sent: the panel's number
/// may be minutes old, and this is what ends up on a command line.
fn upgrade_dsh_to(app: &tauri::AppHandle, channel: &'static str) {
    // Say we are working before doing any of it. The resolve below spawns
    // `dsh --version` and a network-bound `npm view`, which together take
    // seconds; until this existed the panel sat unchanged for all of them and the
    // click read as having done nothing. Carries the numbers the panel is already
    // showing rather than None, so the row does not blank out and refill.
    let (installed, target) = cached_dsh_versions(app);
    emit_dsh_version(
        app,
        "resolving",
        channel,
        installed.as_deref(),
        target.as_deref(),
        None,
    );

    match check_dsh_version(channel) {
        VersionCheck::Compared {
            installed,
            target,
            relation,
            ..
        } => {
            report_dsh_version(
                app,
                &VersionCheck::Compared {
                    installed: installed.clone(),
                    channel,
                    target: target.clone(),
                    relation,
                },
            );
            if relation == VersionRelation::Same {
                return; // nothing to do; the panel's button is disabled anyway
            }
            upgrade_dsh_now(app, &installed, &target, relation);
        }
        other => {
            // Nothing installable: say why on the error view rather than running
            // an npm command that cannot do what the user asked.
            let detail = match &other {
                VersionCheck::ChannelEmpty { channel, .. } => {
                    format!("{DSH_PACKAGE} 的 {channel} 通道还没有发布过版本，没有可安装的目标。")
                }
                VersionCheck::NotInstalled => "本机还没有安装 dsh。".to_string(),
                VersionCheck::Failed { reason, .. } => reason.clone(),
                VersionCheck::Compared { .. } => unreachable!(),
            };
            report_dsh_version(app, &other);
            // The panel's own row now carries this, so only take over the content
            // area when there is no GUI to take it over from. Killing a running
            // session's iframe to announce that a version lookup failed is the
            // wrong trade.
            if !gui_is_up(app) {
                emit_status(app, "error", vec![detail]);
            }
        }
    }
}

/// The upgrade proper: stop our backend, reinstall the package, boot again.
///
/// Stopping first is not optional. `npm install -g` replaces the files the
/// running `dsh web` is executing out of, and node has them open — on Windows
/// that fails outright (EBUSY/EPERM), and on any platform a half-swapped package
/// is a backend that no longer boots. So the sequence is: kill what we own,
/// install, then let `boot_sequence` bring up the new version.
fn upgrade_dsh_now(
    app: &tauri::AppHandle,
    installed: &str,
    target: &str,
    relation: VersionRelation,
) {
    // A backend we do not own (a `dsh web` already serving in a browser tab) is
    // still holding the old files open, and we have no business killing somebody
    // else's process. Refuse rather than corrupting their install.
    let ours = backend_is_ours(app);
    if !ours && probe_existing_web().is_some() {
        emit_status(
            app,
            "error",
            vec![format!(
                "另一个 `dsh web` 正在运行（端口 {}），它占用着要被替换的文件。请先关掉它，再回来更新。",
                probe_port()
            )],
        );
        return;
    }

    let down = relation == VersionRelation::Older;
    emit_status(
        app,
        "installing",
        vec![
            if down {
                format!("正在把 dsh 回退到 {target}")
            } else {
                format!("正在把 dsh 更新到 {target}")
            },
            "装完会自动重新启动，稍等一下~".into(),
        ],
    );
    // Our own backend has to let go of the files before npm touches them.
    take_backend(app);
    // The backend we just forced never released the ledger lock; the boot below
    // would refuse to start otherwise.
    clear_stale_task_board_lock();

    // A downgrade is the one direction that can meet state it does not
    // understand: `~/.dsh` holds versioned files (task-board/ledger-v2.json,
    // storages/, settings.yaml) that the newer dsh may have migrated forward, and
    // npm does not roll those back. Copy them aside first, and refuse the
    // downgrade if that fails -- having promised a backup and then downgraded
    // without one is worse than not downgrading.
    if down {
        match backup_dsh_home(app, installed) {
            Ok(Some(path)) => {
                let _ = app.emit(
                    "dsh-install-log",
                    format!("已备份 {} → {}", dsh_home().display(), path.display()),
                );
            }
            // Nothing to back up: a machine that never ran dsh has no state.
            Ok(None) => {}
            Err(e) => {
                emit_status(
                    app,
                    "error",
                    vec![format!(
                        "回退前备份 {} 失败，已中止：{e}\n手动备份后再试，或直接执行 npm i -g {DSH_PACKAGE}@{target}。",
                        dsh_home().display()
                    )],
                );
                return;
            }
        }
    }

    match run_install(app, &format!("{DSH_PACKAGE}@{target}")) {
        Ok(()) => {
            // Report the version we ended up on before booting, so the pill
            // clears even if the boot then fails for an unrelated reason.
            report_dsh_version(app, &check_dsh_version(dsh_channel(app)));
            emit_status(
                app,
                "booting",
                vec![if down {
                    "回退完成，正在启动…".into()
                } else {
                    "更新完成，正在启动…".to_string()
                }],
            );
            boot_sequence(app.clone());
        }
        // The retry button on the error view boots whatever version is installed,
        // which is the right move whether the install landed or not.
        Err(detail) => emit_status(app, "error", vec![detail]),
    }
}

/// Copy `~/.dsh` aside before a downgrade. `Ok(None)` means there was nothing
/// there. The destination carries the version being left behind, so several
/// backups can coexist and each says what it came from.
fn backup_dsh_home(app: &tauri::AppHandle, installed: &str) -> std::io::Result<Option<PathBuf>> {
    let home = dsh_home();
    if !home.is_dir() {
        return Ok(None);
    }
    // Seconds since the epoch rather than a formatted date: unique and ordered,
    // with no date-formatting dependency. `installed` is a version we parsed, so
    // it cannot carry a path separator.
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let dest = home.with_file_name(format!(".dsh.bak-{installed}-{stamp}"));
    let _ = app.emit(
        "dsh-install-log",
        format!("回退前先备份 {} …", home.display()),
    );
    copy_tree(&home, &dest)?;
    Ok(Some(dest))
}

/// Recursively copy a directory. Hand-rolled to avoid a dependency for one call.
///
/// Symlinks are followed rather than recreated: `~/.dsh` is config and state, and
/// a backup that points back at the files being replaced would not be a backup.
fn copy_tree(from: &std::path::Path, to: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let src = entry.path();
        let dst = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_tree(&src, &dst)?;
        } else {
            // A lock file another process holds open is not worth failing the
            // whole backup over -- it carries no state we could restore anyway.
            match std::fs::copy(&src, &dst) {
                Ok(_) => {}
                Err(e) if src.extension().is_some_and(|x| x == "lock") => {
                    eprintln!("skipping lock file {}: {e}", src.display());
                }
                Err(e) => return Err(e),
            }
        }
    }
    Ok(())
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

/// Tray icon (DeepSeek whale) with a menu: show window / check for updates / quit.
///
/// Two separate update entries, because they update different things: the shell
/// (this binary, via the Tauri updater) and dsh itself (the npm package that does
/// the actual work). Conflating them would leave no way to update a months-old
/// dsh under a current shell, which is the common case.
///
/// Not generic over the runtime: the menu handlers reach the commands, which take
/// a concrete `AppHandle`. The generic only ever resolved to Wry anyway.
fn setup_tray(app: &tauri::AppHandle) -> tauri::Result<()> {
    let show = MenuItem::with_id(app, "show", "显示 DSH", true, None::<&str>)?;
    let update = MenuItem::with_id(app, "update", "检查桌面端更新", true, None::<&str>)?;
    let update_dsh = MenuItem::with_id(app, "update-dsh", "检查 dsh 更新", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&show, &update, &update_dsh, &quit])?;

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
            "update-dsh" => {
                // On a thread, not the async runtime: the check shells out twice
                // and blocks, which would stall other tasks on that runtime.
                let app = app.clone();
                std::thread::spawn(move || check_dsh_update_interactive(&app));
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

/// Updater manifest per channel.
///
/// `latest` is GitHub's own alias, which resolves to the newest release that is
/// neither a draft nor a prerelease — so stable users never see an alpha. The
/// alpha channel cannot use that alias for exactly that reason, so it gets a
/// fixed prerelease tag whose `latest.json` CI overwrites on every prerelease
/// (see `.github/workflows/release.yml`). Until the first one is published that
/// URL 404s, which surfaces as `channel-empty` rather than an error.
const SHELL_MANIFESTS: [(&str, &str); 2] = [
    (
        "latest",
        "https://github.com/ColdSand803/dsh-desktop/releases/latest/download/latest.json",
    ),
    (
        "alpha",
        "https://github.com/ColdSand803/dsh-desktop/releases/download/alpha/latest.json",
    ),
];

fn shell_manifest(channel: &str) -> Option<&'static str> {
    SHELL_MANIFESTS
        .iter()
        .find(|(c, _)| *c == channel)
        .map(|(_, url)| *url)
}

/// Look up what `channel` offers for this binary, without installing anything.
///
/// Returns the release's version alongside how it relates to the running one.
/// The version comparator is deliberately "always yes": the default only accepts
/// a strictly higher version, which would make a channel switch back to stable
/// invisible. The direction is worked out afterwards with `relate`, so the panel
/// can label the button 更新 or 回退.
async fn resolve_shell_update(
    app: &tauri::AppHandle,
    channel: &'static str,
) -> Result<Option<(tauri_plugin_updater::Update, VersionRelation)>, String> {
    use tauri_plugin_updater::UpdaterExt;

    let url = shell_manifest(channel).ok_or_else(|| format!("未知的通道 {channel}。"))?;
    let endpoint = Url::parse(url).map_err(|e| format!("更新清单地址无效：{e}"))?;

    let updater = app
        .updater_builder()
        .endpoints(vec![endpoint])
        .map_err(|e| format!("更新器初始化失败：{e}"))?
        .version_comparator(|_current, _release| true)
        .build()
        .map_err(|e| format!("更新器初始化失败：{e}"))?;

    match updater.check().await {
        Ok(Some(update)) => {
            let relation = relate(&update.version, &update.current_version);
            Ok(Some((update, relation)))
        }
        // The comparator above always accepts, so `None` only happens on a 204.
        Ok(None) => Ok(None),
        // No manifest at that URL: the channel has published nothing yet. This is
        // the alpha case before CI's first prerelease, and is not a failure.
        Err(tauri_plugin_updater::Error::ReleaseNotFound) => Ok(None),
        Err(e) => Err(format!("{e}")),
    }
}

/// Run a shell check for `channel` and report it to the page.
#[tauri::command]
async fn check_shell_update(app: tauri::AppHandle, channel: String) {
    let channel = normalize_channel(&channel);
    let installed = app.package_info().version.to_string();
    emit_shell_version(&app, "checking", channel, Some(&installed), None, None);

    match resolve_shell_update(&app, channel).await {
        Ok(Some((update, relation))) => emit_shell_version(
            &app,
            relation.state(),
            channel,
            Some(&installed),
            Some(&update.version),
            None,
        ),
        Ok(None) => {
            emit_shell_version(&app, "channel-empty", channel, Some(&installed), None, None)
        }
        Err(reason) => emit_shell_version(
            &app,
            "error",
            channel,
            Some(&installed),
            None,
            Some(&reason),
        ),
    }
}

/// Install what `channel` offers and restart into it.
///
/// Re-resolves rather than holding an `Update` from the earlier check: the object
/// is not something to park across IPC calls, and one extra request is cheap next
/// to replacing the running binary.
#[tauri::command]
async fn update_shell(app: tauri::AppHandle, channel: String) {
    let channel = normalize_channel(&channel);
    let installed = app.package_info().version.to_string();

    // Fetching the manifest is a network round trip, so claim the row before it
    // rather than after: same reason `upgrade_dsh_to` opens with this.
    emit_shell_version(
        &app,
        "resolving",
        channel,
        Some(&installed),
        cached_shell_target(&app).as_deref(),
        None,
    );

    let resolved = match resolve_shell_update(&app, channel).await {
        Ok(Some(pair)) => pair,
        Ok(None) => {
            emit_shell_version(&app, "channel-empty", channel, Some(&installed), None, None);
            return;
        }
        Err(reason) => {
            emit_shell_version(
                &app,
                "error",
                channel,
                Some(&installed),
                None,
                Some(&reason),
            );
            return;
        }
    };
    let (update, relation) = resolved;
    if relation == VersionRelation::Same {
        emit_shell_version(
            &app,
            "current",
            channel,
            Some(&installed),
            Some(&update.version),
            None,
        );
        return;
    }

    let target = update.version.clone();
    emit_shell_version(
        &app,
        "installing",
        channel,
        Some(&installed),
        Some(&target),
        None,
    );
    match update.download_and_install(|_, _| {}, || {}).await {
        // Relaunch into the new binary; the RunEvent::Exit handler still runs,
        // so the backend gets reaped.
        Ok(()) => app.restart(),
        Err(e) => emit_shell_version(
            &app,
            "error",
            channel,
            Some(&installed),
            Some(&target),
            Some(&format!("更新失败：{e}")),
        ),
    }
}

/// Tray-triggered shell update check, kept as the path that works when the
/// webview does not: native dialogs need no page. The panel in the title bar is
/// the richer entry point; this one follows the same selected channel so the two
/// never disagree.
///
/// Manual rather than automatic on startup: an update replaces the running binary
/// and needs a relaunch, so it must not happen behind the user's back mid-session.
async fn check_for_update(app: tauri::AppHandle) {
    use tauri_plugin_dialog::{DialogExt, MessageDialogButtons};

    const TITLE: &str = "DSH Desktop 更新";
    let channel = shell_channel(&app);
    let installed = app.package_info().version.to_string();
    emit_shell_version(&app, "checking", channel, Some(&installed), None, None);

    match resolve_shell_update(&app, channel).await {
        Ok(Some((update, relation))) => {
            emit_shell_version(
                &app,
                relation.state(),
                channel,
                Some(&installed),
                Some(&update.version),
                None,
            );
            if relation == VersionRelation::Same {
                app.dialog()
                    .message(format!("已经是最新版本（{installed}，{channel} 通道）。"))
                    .title(TITLE)
                    .show(|_| {});
                return;
            }
            let down = relation == VersionRelation::Older;
            let version = update.version.clone();
            let handle = app.clone();
            app.dialog()
                .message(if down {
                    format!(
                        "{channel} 通道当前是 {version}，比本机的 {installed} 旧。\n继续会回退到 {version}，之后应用自动重启。"
                    )
                } else {
                    format!("发现新版本 {version}（当前 {installed}）。\n更新后应用会自动重启。")
                })
                .title(TITLE)
                .buttons(MessageDialogButtons::OkCancelCustom(
                    if down { "回退".into() } else { "立即更新".into() },
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
                                    .title(TITLE)
                                    .blocking_show();
                            }
                        }
                    });
                });
        }
        Ok(None) => {
            emit_shell_version(&app, "channel-empty", channel, Some(&installed), None, None);
            app.dialog()
                .message(format!(
                    "{channel} 通道还没有发布过版本，没有可安装的目标。本机是 {installed}。"
                ))
                .title(TITLE)
                .show(|_| {});
        }
        Err(reason) => {
            emit_shell_version(
                &app,
                "error",
                channel,
                Some(&installed),
                None,
                Some(&reason),
            );
            app.dialog()
                .message(format!("检查更新失败：{reason}"))
                .title(TITLE)
                .show(|_| {});
        }
    }
}

/// Tray-triggered dsh version check, the counterpart to `check_for_update`.
/// Unlike the startup check this always says something — the user asked, so
/// "already current" and "could not tell" are both answers worth a dialog.
fn check_dsh_update_interactive(app: &tauri::AppHandle) {
    use tauri_plugin_dialog::{DialogExt, MessageDialogButtons};

    const TITLE: &str = "dsh 更新";

    let channel = dsh_channel(app);
    emit_dsh_version(app, "checking", channel, None, None, None);
    let result = check_dsh_version(channel);
    report_dsh_version(app, &result);

    match result {
        VersionCheck::Compared {
            installed,
            target,
            relation,
            ..
        } => {
            if relation == VersionRelation::Same {
                app.dialog()
                    .message(format!(
                        "dsh 已经是 {channel} 通道的最新版本（{installed}）。"
                    ))
                    .title(TITLE)
                    .show(|_| {});
                return;
            }
            let down = relation == VersionRelation::Older;
            let handle = app.clone();
            app.dialog()
                .message(if down {
                    format!(
                        "{channel} 通道当前是 dsh {target}，比本机的 {installed} 旧。\n继续会回退到 {target}；回退前会先把 ~/.dsh 备份一份，但里面的状态不会被迁移回旧格式。"
                    )
                } else {
                    format!(
                        "发现新版本 dsh {target}（当前 {installed}）。\n更新期间后端会短暂停止，完成后自动重新启动。"
                    )
                })
                .title(TITLE)
                .buttons(MessageDialogButtons::OkCancelCustom(
                    if down { "回退".into() } else { "立即更新".into() },
                    "稍后".into(),
                ))
                .show(move |confirmed| {
                    if !confirmed {
                        return;
                    }
                    // Bring the window up: the upgrade takes over the content area
                    // to stream npm's log, which is no use behind the tray.
                    if let Some(w) = handle.get_webview_window("main") {
                        let _ = w.show();
                        let _ = w.unminimize();
                        let _ = w.set_focus();
                    }
                    std::thread::spawn(move || upgrade_dsh_to(&handle, channel));
                });
        }
        VersionCheck::ChannelEmpty { installed, .. } => {
            app.dialog()
                .message(format!(
                    "{DSH_PACKAGE} 的 {channel} 通道还没有发布过版本。本机是 {installed}。"
                ))
                .title(TITLE)
                .show(|_| {});
        }
        VersionCheck::NotInstalled => {
            app.dialog()
                .message("本机还没有安装 dsh，没有可比较的版本。")
                .title(TITLE)
                .show(|_| {});
        }
        VersionCheck::Failed { reason, .. } => {
            app.dialog()
                .message(format!("检查 dsh 更新失败：{reason}"))
                .title(TITLE)
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
    // `--no-open`: this window *is* the UI. Left to itself `dsh web` opens the
    // default browser on every boot, so the same backend ends up on screen twice
    // — once here, once in a stray tab.
    let mut cmd = if cfg!(windows) {
        let mut c = Command::new("cmd");
        c.args(["/C", "dsh", "web", "--port", "0", "--no-open"]);
        c
    } else {
        let mut c = Command::new("sh");
        c.args(["-c", "dsh web --port 0 --no-open"]);
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
        // `does not provide an export named` is the missing-export shape, and it is
        // the most useful line dsh emits: it names both the plugin that failed to
        // import and the export it wanted. It earned its place here the hard way --
        // twice a prerelease dropped an API that installed plugins still import,
        // and both times this summary led with "plugin tree failed to load" while
        // the line that explained it sat further down, so the log had to be read by
        // hand. ESM resolves exports at load time, so one such plugin takes the
        // whole tree with it and the wrapper lines are all identical.
        let bucket = if text.contains("Cannot find")
            || text.contains("already owned by")
            || text.contains("does not provide an export named")
        {
            &mut specific
        } else {
            &mut generic
        };
        if !bucket.contains(&text) {
            bucket.push(text);
        }
    }
    // Checked before the buckets merge, so a log full of generic wrappers cannot
    // push the evidence for this out of view.
    let missing_export = specific
        .iter()
        .any(|l| l.contains("does not provide an export named"));

    specific.append(&mut generic);
    specific.truncate(4);

    // The line above names the plugin and the export, which still leaves the reader
    // to work out that this is a version disagreement rather than a broken file --
    // and the two look identical from the outside. Say which it is, and what
    // actually clears it. Added after the truncation so it cannot be cut.
    if missing_export {
        specific.push(
            "这是 dsh 内核与已装插件的接口不一致（常见于装了预发布版本），插件文件本身没坏。\
             可在标题栏「检查更新」里把 dsh 换回 latest 通道的版本，或更新/卸载报错的那个插件。"
                .into(),
        );
    }
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

    /// Write `body` to a uniquely named file under the temp dir and hand back the
    /// path. No tempfile dependency for one test; the caller removes it.
    fn temp_log(name: &str, body: &str) -> PathBuf {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let path = std::env::temp_dir().join(format!("dsh-desktop-test-{name}-{stamp}.log"));
        std::fs::write(&path, body).unwrap();
        path
    }

    #[test]
    fn leads_with_the_missing_export_not_the_wrapper() {
        // The real shape of a boot failure after a prerelease dropped an API that
        // installed plugins still import. Everything above the last [cause] is the
        // same text for any plugin-tree failure, so the summary has to reach past it.
        let log = temp_log(
            "missing-export",
            "[err] throw new Error(`${binName}: ${stage}: ${detail}${stack}`, { cause });\n\
             [err] Error: dsh: plugin tree failed to load: failed to apply loader entry include (cordis:include): loader entries failed to apply\n\
             [err]     at loadAll (file:///C:/x/dist/loader.js:12:9)\n\
             [err] [cause]: AggregateError: loader entries failed to apply\n\
             [err]     at applyEntries (file:///C:/x/dist/loader.js:44:11)\n\
             [err] [cause]: Error: failed to import loader entry web-ui-settings (@linxin666/dsh-client-ui-web-ui-settings): The requested module '@deepseek-ai/dsh-settings' does not provide an export named 'settingsNamespace'\n",
        );
        let summary = backend_error_summary(&log);
        let _ = std::fs::remove_file(&log);

        assert!(
            summary[0].contains("does not provide an export named"),
            "the line naming the plugin and the export has to come first, got {summary:#?}"
        );
        assert!(
            summary[0].contains("@linxin666/dsh-client-ui-web-ui-settings"),
            "and it has to still name the plugin: {:?}",
            summary[0]
        );
        // Stack frames are noise; the wrapper lines are kept but demoted.
        assert!(
            !summary.iter().any(|l| l.trim_start().starts_with("at ")),
            "no stack frames: {summary:#?}"
        );
        // The diagnosis is not readable off the raw line, so it is appended.
        assert!(
            summary.last().unwrap().contains("接口不一致"),
            "expected the version-mismatch hint last, got {summary:#?}"
        );
    }

    #[test]
    fn adds_no_hint_when_nothing_is_missing_an_export() {
        let log = temp_log(
            "ledger-lock",
            "[err] Error: dsh: task board is already owned by pid 4242\n",
        );
        let summary = backend_error_summary(&log);
        let _ = std::fs::remove_file(&log);

        assert_eq!(
            summary.len(),
            1,
            "no hint for an unrelated failure: {summary:#?}"
        );
        assert!(summary[0].contains("already owned by"));
    }

    #[test]
    fn reads_the_empty_line_as_the_backend_exiting() {
        let (tx, rx) = mpsc::channel::<String>();
        tx.send(String::new()).unwrap();
        assert!(wait_for_backend_exit(&rx));
    }

    #[test]
    fn keeps_waiting_through_a_second_url() {
        let (tx, rx) = mpsc::channel::<String>();
        tx.send("http://127.0.0.1:52341".into()).unwrap();
        tx.send("http://127.0.0.1:52341".into()).unwrap();
        tx.send(String::new()).unwrap();
        assert!(wait_for_backend_exit(&rx));
    }

    #[test]
    fn reports_no_exit_when_the_sender_just_disappears() {
        let (tx, rx) = mpsc::channel::<String>();
        drop(tx);
        assert!(!wait_for_backend_exit(&rx));
    }

    #[test]
    fn retiring_a_backend_moves_the_generation_on() {
        // What the watcher actually compares. The empty-slot case matters too:
        // `take_backend` returns None there but must still invalidate any
        // watcher left over from a backend we no longer track.
        let state = Backend::default();
        let mine = state.generation.load(Ordering::SeqCst);
        assert_eq!(state.generation.fetch_add(1, Ordering::SeqCst), mine);
        assert_ne!(state.generation.load(Ordering::SeqCst), mine);
    }

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
    fn parses_a_bare_version() {
        // What `npm view <pkg> version` prints.
        assert_eq!(parse_version("1.2.3\n").as_deref(), Some("1.2.3"));
    }

    #[test]
    fn parses_a_version_out_of_surrounding_text() {
        // A CLI's --version is not obliged to print only the number.
        assert_eq!(
            parse_version("dsh/2.0.1 win32-x64").as_deref(),
            Some("2.0.1")
        );
        assert_eq!(
            parse_version("dsh version 0.9.12").as_deref(),
            Some("0.9.12")
        );
        // The whole token is taken, not its tail: anchoring on a digit run start
        // is what stops "1.2.3" from also matching at the "2".
        assert_eq!(parse_version("v1.2.3").as_deref(), Some("1.2.3"));
    }

    #[test]
    fn keeps_prerelease_and_drops_trailing_prose() {
        assert_eq!(parse_version("3.0.0-rc.2").as_deref(), Some("3.0.0-rc.2"));
        assert_eq!(parse_version("installed 1.4.0.").as_deref(), Some("1.4.0"));
    }

    #[test]
    fn ignores_output_with_no_version() {
        assert_eq!(parse_version(""), None);
        assert_eq!(parse_version("command not found"), None);
        // A lone integer is an exit code or a port, not a version.
        assert_eq!(parse_version("exited with 8"), None);
    }

    /// The old `is_newer`, kept as a test helper so these cases stay readable.
    fn is_newer(a: &str, b: &str) -> bool {
        relate(a, b) == VersionRelation::Newer
    }

    #[test]
    fn orders_releases_by_component() {
        assert!(is_newer("1.2.4", "1.2.3"));
        assert!(is_newer("1.3.0", "1.2.99"));
        assert!(is_newer("2.0.0", "1.99.99"));
        assert!(!is_newer("1.2.3", "1.2.3"));
        assert!(!is_newer("1.2.3", "1.2.4"));
        // Numeric, not lexical: "10" beats "9".
        assert!(is_newer("1.10.0", "1.9.0"));
    }

    #[test]
    fn treats_missing_components_as_zero() {
        assert!(!is_newer("1.2", "1.2.0"));
        assert!(!is_newer("1.2.0", "1.2"));
        assert!(is_newer("1.3", "1.2.9"));
    }

    #[test]
    fn ranks_a_release_above_its_prereleases() {
        assert!(is_newer("1.0.0", "1.0.0-rc.1"));
        assert!(!is_newer("1.0.0-rc.1", "1.0.0"));
        assert!(is_newer("1.0.0-rc.2", "1.0.0-rc.1"));
        // Numeric identifiers compare numerically, and sort below alphanumeric.
        assert!(is_newer("1.0.0-rc.10", "1.0.0-rc.2"));
        assert!(is_newer("1.0.0-alpha.beta", "1.0.0-alpha.1"));
        // A longer prerelease outranks the prefix it extends.
        assert!(is_newer("1.0.0-rc.1", "1.0.0-rc"));
    }

    #[test]
    fn ignores_build_metadata() {
        // Per semver, build metadata carries no precedence.
        assert!(!is_newer("1.2.3+build.9", "1.2.3+build.1"));
        assert!(is_newer("1.2.4+a", "1.2.3+z"));
    }

    #[test]
    fn relates_a_channel_target_in_both_directions() {
        // The real data this was built against: the alpha tag is ahead of latest,
        // so switching to alpha is an update and switching back is a rollback.
        assert_eq!(
            relate("0.1.2-alpha.3", "0.1.1-rc.2"),
            VersionRelation::Newer
        );
        assert_eq!(
            relate("0.1.1-rc.2", "0.1.2-alpha.3"),
            VersionRelation::Older
        );
        assert_eq!(relate("0.1.1-rc.2", "0.1.1-rc.2"), VersionRelation::Same);
    }

    #[test]
    fn maps_a_relation_to_the_pages_state() {
        // Only `outdated` raises the title bar pill; a rollback must not nag.
        assert_eq!(VersionRelation::Newer.state(), "outdated");
        assert_eq!(VersionRelation::Older.state(), "rollback");
        assert_eq!(VersionRelation::Same.state(), "current");
    }

    #[test]
    fn normalizes_known_channels_and_rejects_everything_else() {
        assert_eq!(normalize_channel("latest"), "latest");
        assert_eq!(normalize_channel("alpha"), "alpha");
        assert_eq!(normalize_channel(" alpha "), "alpha");
        assert_eq!(normalize_channel("ALPHA"), "alpha");
        // Anything off the allowlist falls back rather than reaching a command
        // line: this value is interpolated into `npm view`.
        assert_eq!(normalize_channel("next"), DEFAULT_CHANNEL);
        assert_eq!(normalize_channel(""), DEFAULT_CHANNEL);
        assert_eq!(normalize_channel("latest; rm -rf /"), DEFAULT_CHANNEL);
        assert_eq!(normalize_channel("../../etc"), DEFAULT_CHANNEL);
    }

    #[test]
    fn resolves_a_channel_out_of_dist_tags() {
        // Shaped like the real `npm view @deepseek-ai/dsh dist-tags --json`.
        let tags = match serde_json::json!({
            "latest": "0.1.1-rc.2",
            "next": "0.1.1-rc.2",
            "alpha": "0.1.2-alpha.3",
            "broken": "not-a-version",
        }) {
            serde_json::Value::Object(m) => m,
            _ => unreachable!(),
        };

        assert!(matches!(
            channel_dsh_version(&tags, "alpha"),
            ChannelLookup::Version(v) if v == "0.1.2-alpha.3"
        ));
        assert!(matches!(
            channel_dsh_version(&tags, "latest"),
            ChannelLookup::Version(v) if v == "0.1.1-rc.2"
        ));
        // A tag the package does not publish is "nothing here", not a failure --
        // the panel says so instead of showing an error.
        assert!(matches!(
            channel_dsh_version(&tags, "beta"),
            ChannelLookup::Missing
        ));
        assert!(matches!(
            channel_dsh_version(&tags, "broken"),
            ChannelLookup::Failed
        ));
    }

    #[test]
    fn has_a_manifest_for_every_channel() {
        // A channel with no manifest could never be checked, and the panel offers
        // exactly the channels in CHANNELS.
        for channel in CHANNELS {
            assert!(
                shell_manifest(channel).is_some(),
                "no updater manifest for channel {channel}"
            );
        }
        assert!(shell_manifest("beta").is_none());
    }

    #[test]
    fn ignores_lines_without_a_url() {
        assert_eq!(extract_url("Error: EADDRINUSE"), None);
        assert_eq!(extract_url(""), None);
        // https is never the local GUI; we only ever serve plain http.
        assert_eq!(extract_url("https://127.0.0.1:3080"), None);
    }
}
