# DSH Desktop

[简体中文](README.zh.md)

A desktop client for [DeepSeek Harness](https://www.npmjs.com/package/@deepseek-ai/dsh)
(`dsh`). It wraps `dsh web` into a native Windows app using [Tauri v2](https://tauri.app).

> **Status:** works, but young. Only `0.1.0` exists, and `dsh` itself is a
> developer preview whose output format this app parses — see
> [Known limitations](#known-limitations).

- **Title bar matches dsh's sidebar.** The bar is drawn in the page (the window is
  created with `decorations: false`), and its colour comes from dsh's own design
  token `--dsw-specific-sidebar-fill` as concrete RGB — not two hardcoded shades.
  Theme switches follow, including third-party themes, and it looks the same on
  Windows 10 and 11.
- **Works without dsh installed.** Startup probes the environment first. With no
  `dsh`, the window stays on a guidance page offering a one-click
  `npm i -g @deepseek-ai/dsh` with a live log, then boots straight in. With no npm
  either, it points you at nodejs.org.
- **Reuses a running backend.** If a `dsh web` is already serving on port 3080 —
  your browser tab, say — the app displays that instead of starting a second one.
- **Lives in the tray.** Closing the window hides it; the backend keeps running.
  Quit from the tray menu to shut everything down.
- **No console flash.** Every subprocess is spawned with `CREATE_NO_WINDOW`.
- **Nothing left behind.** The backend tree is confined to a Windows Job Object
  with `KILL_ON_JOB_CLOSE`, so `dsh web` is reaped even if the app is killed from
  Task Manager or you log off.
- **Single instance.** Launching again focuses the existing window.

## Requirements

| | |
|---|---|
| Windows | 10 or 11 (x64) |
| WebView2 | Usually already installed on Win10/11 |
| Node.js + npm | Needed for the Tauri CLI, and for the in-app dsh install |
| Rust | stable >= 1.77, to build from source |
| `dsh` | **Optional** — installable from inside the app |

## Install

Grab the installer from [Releases](https://github.com/ColdSand803/dsh-desktop/releases).
It is a **per-user** install: it goes into your user directory, does not prompt for
UAC, and registers under `HKCU`.

## Build from source

```bash
npm install
npm run dev              # dev mode
npm run build            # exe + NSIS installer
npm run build:no-bundle  # exe only
```

Use `npm run dev` rather than `cargo run` inside `src-tauri` — the Tauri CLI
changes the working directory.

## How it works

The window never navigates. It stays on the bundled shell page
(`ui/index.html`) for its whole life: the shell draws the title bar and hosts the
dsh GUI in an iframe. That is what allows the title bar to be any colour — a
native caption cannot be tinted before Windows 11, and this app targets Win10 too.

Theme colours are sampled by a script injected into **every frame**
(`initialization_script_for_all_frames`), because the shell cannot read across
origins into the dsh page. The dsh frame reports over a `dsh-theme` event, falling
back to encoding colours into `document.title` as `[dsh:RRGGBB:RRGGBB]` when the
event bridge is unavailable.

The backend is spawned as `dsh web --port 0 --no-open` from your home directory
(override with `DSH_DESKTOP_WORKDIR`), and its stdout is parsed for the local URL.

For the full behavioural detail — port probing, the guided-install states, log
rotation, the exit path — see [README.zh.md](README.zh.md), which is the more
detailed document.

## Known limitations

- **Windows only in practice.** There are `sh -c` branches, but Job Object cleanup
  is Windows-specific and nothing is verified elsewhere. CI runs Windows only.
- **Exit is a force kill on Windows.** `taskkill /T` without `/F` is refused by
  every process in this tree, because a windowless console process has no window
  to send a close message to — so waiting is pure delay. Consequence:
  **quitting while dsh is installing a plugin risks corrupting that plugin's
  directory.** Wait for installs to finish. A proper fix means graceful shutdown
  (`CTRL_BREAK`), which is not implemented.
- **Depends on dsh's stdout format.** The GUI address is found by parsing
  `dsh web: http://127.0.0.1:<port>`. dsh is a developer preview and says it will
  make breaking changes; if that line changes, the app stops finding the address.
- **Updates are manual.** The tray's "检查更新" is the only trigger. An update
  replaces the running binary and needs a restart, so it should not happen
  unannounced.
- **No autostart or global hotkeys.** Both change user-visible behaviour, and
  there is no settings UI to turn them off, so neither is enabled.
- **One-click install uses your default npm registry.** Behind a corporate proxy
  it may simply fail; install by hand in that case.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). For security issues, see
[SECURITY.md](SECURITY.md) — please do not open a public issue.

## Licence

[MIT](LICENSE) © ColdSand803

Icons are derived from the DeepSeek whale in dsh's `favicon.svg` (MIT, © 2026
DeepSeek). dsh itself is not bundled or redistributed — it is installed separately
via npm and launched as a subprocess. See [NOTICE](NOTICE).
