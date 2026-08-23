# Security Policy

## Reporting a vulnerability

Please report security issues privately, not as a public issue. Use GitHub's
[private vulnerability reporting](https://github.com/ColdSand803/dsh-desktop/security/advisories/new)
on this repository.

Include what you did, what happened, and the version (`0.1.0` is the only release
so far). A proof of concept helps but is not required. This is a personal project
with no SLA — expect a reply in days, not hours.

If the issue is in **dsh itself** rather than this shell, report it to
[DeepSeek Harness](https://www.npmjs.com/package/@deepseek-ai/dsh). This project
launches `dsh web` as a subprocess; it does not bundle or modify it.

## What this app does, so you know what to look at

Worth knowing before you audit it, and worth knowing as a user:

- **Runs a local HTTP server.** `dsh web --port 0` is spawned as a child process
  and bound to loopback. The desktop window loads it in an iframe. On startup the
  app also probes `127.0.0.1:3080` and will **reuse** a `dsh web` already serving
  there rather than starting its own — so a process you did not start can end up
  displayed in this window. Override the probe port with `DSH_DESKTOP_PROBE_PORT`.
- **Installs software on request.** The guided install runs
  `npm install -g @deepseek-ai/dsh` when you click the button. The package name is
  a Rust-side constant and the IPC command takes no arguments, so the page cannot
  redirect it to another package. It is never triggered automatically.
- **Auto-update is manual and signature-checked.** Nothing is fetched unless you
  pick "检查更新" from the tray. Updates are verified against the minisign public
  key in `src-tauri/tauri.conf.json`; an unsigned or mis-signed update is refused.
- **Capability surface is split deliberately.** The bundled shell page gets
  `core:default` plus the window controls its self-drawn title bar needs. The dsh
  origin, which renders model output, gets `core:event:allow-emit` and nothing
  else (`src-tauri/capabilities/remote-theme.json`) — no window control, no path
  or app APIs.
- **Force-kills the backend tree on exit** (`taskkill /T /F` on Windows) and
  confines it to a Job Object so it cannot outlive the app. See the known
  limitation about plugin-install corruption in the README.

## Known limitations, already documented

These are in the README under 已知限制 and are not news:

- Quitting **while dsh is installing a plugin** can corrupt that plugin's
  directory, because the Windows exit path cannot be graceful. Wait for installs
  to finish before quitting.
- The one-click install uses whatever npm registry is configured. On a network
  that needs a proxy it will simply fail; install by hand instead.

## Scope

Out of scope: anything requiring an attacker who already has code execution or
your user account on the machine, and vulnerabilities in dsh, Node.js, npm,
WebView2, or Tauri themselves — report those upstream.
