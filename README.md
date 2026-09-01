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

Regenerating icons and cutting a release are covered in
[CONTRIBUTING.md](CONTRIBUTING.md).

## How it works

The window never navigates. It stays on the bundled shell page (`ui/index.html`)
for its whole life: the shell draws the title bar and hosts the dsh GUI in an
iframe. That is what allows the title bar to be any colour — a native caption
cannot be tinted before Windows 11, and this app targets Win10 too. The sampling
script is injected into every frame, since the shell cannot read across origins
into the dsh page; the reasoning is in the comments around `THEME_WATCH_JS` in
`src-tauri/src/main.rs`.

The backend is spawned as `dsh web --port 0 --no-open` from your home directory
(override with `DSH_DESKTOP_WORKDIR`), and its stdout is parsed for the local URL.
Startup first probes port 3080 (`DSH_DESKTOP_PROBE_PORT`) and reuses a `dsh web`
already serving there — in which case quitting this app leaves that backend
running, because it is not ours to kill.

Behaviour worth knowing as a user: closing the window hides it to the tray and the
backend keeps serving, so quit from the tray menu to actually stop. The backend log
is at `%LOCALAPPDATA%\com.dsh.desktop\logs\dsh-backend.log` and is **overwritten on
every launch**, so copy it before restarting if you need it.

[README.zh.md](README.zh.md) is the more detailed document.

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
- **Shell updates are manual.** The tray's "检查桌面端更新" is the only trigger. An
  update replaces the running binary and needs a restart, so it should not happen
  unannounced. The title bar's 检查更新 button is the other trigger.
- **dsh updates are checked, never applied silently.** One `npm view` after each
  boot (through your own registry config); a newer version only lights up a pill in
  the title bar. Upgrading replaces the files the running backend executes from, so
  it takes a click: the app stops the backend it owns, reinstalls, and boots again.
  A `dsh web` this app does not own — a browser tab's — holds those files open, so
  the upgrade refuses rather than corrupting it.
- **Updating with the GUI open asks twice.** Stopping the backend is a force kill,
  so anything the model was part-way through is cut off — and there is no promised
  way to ask dsh whether an agent is running, so the app does not pretend to know.
  With a session on screen the action button arms first and states what will
  happen; with no GUI up there is nothing to interrupt and it goes straight
  through.
- **Two channels, picked per side** (latest / alpha) in the title bar's update
  panel. For dsh they are npm dist-tags; for the shell they are two updater
  manifests — `alpha` is a fixed prerelease tag (see
  `.github/workflows/release.yml`) and honestly reports "nothing published" until
  the first prerelease exists. The choice lives in `channels.json`, which is what
  the startup check reads.
- **Channels can be switched backwards**, and the resolved version number is what
  gets installed, never a tag. When a channel is behind what you have, the button
  becomes a rollback; for dsh it first copies `~/.dsh` to
  `~/.dsh.bak-<version>-<timestamp>` and aborts if that fails. **State is not
  migrated backwards** — `task-board/ledger-v2.json`, `storages/` and
  `settings.yaml` are all versioned, and an older dsh may not read what a newer one
  wrote.
- **No autostart or global hotkeys.** Both change user-visible behaviour, and
  there is no settings UI to turn them off, so neither is enabled.
- **One-click install uses your default npm registry.** Behind a corporate proxy
  it may simply fail; install by hand in that case.

## Links

- [Releases](https://github.com/ColdSand803/dsh-desktop/releases)
- [Issues](https://github.com/ColdSand803/dsh-desktop/issues)
- [Linux.do](https://linux.do/) — community discussion

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). For security issues, see
[SECURITY.md](SECURITY.md) — please do not open a public issue.

## Licence

[MIT](LICENSE) © ColdSand803

Icons are derived from the DeepSeek whale in dsh's `favicon.svg` (MIT, © 2026
DeepSeek). dsh itself is not bundled or redistributed — it is installed separately
via npm and launched as a subprocess. See [NOTICE](NOTICE).
