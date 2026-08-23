# Contributing

Thanks for looking. This is a small personal project, so the bar is "does it keep
working and stay explainable" rather than any formal process.

## Getting set up

```bash
npm install
npm run dev
```

You need a Rust stable toolchain (>= 1.77) and WebView2 (already present on most
Win10/11 installs). `dsh` itself is optional for development — without it the app
lands on the guidance page, which is a legitimate state to work on.

Run `npm run dev`, not `cargo run` inside `src-tauri`: the Tauri CLI changes the
working directory, so a bare `cargo run` behaves differently.

## Before you open a PR

```bash
cd src-tauri
cargo fmt
cargo clippy --all-targets -- -D warnings
cargo test
```

CI runs exactly these three on windows-latest and `-D warnings` means a lint is a
failure, so please run them locally first.

Windows-only by design: the app targets Windows (Job Object cleanup, `taskkill`,
DWM border tinting). The `sh -c` branches exist but nothing off-Windows is
verified, so CI does not pretend otherwise.

## What tends to matter in review

- **Say why, not what, in comments.** The code has a lot of comments explaining
  why something is done a surprising way — the OEM codepage in the backend log,
  why the Windows exit path cannot be graceful, why the title bar is drawn in the
  page. Those exist because each one was re-litigated at least once. If you change
  such a behaviour, update the reasoning with it.
- **Don't claim verification you didn't do.** Much of this cannot be checked
  without a GUI run and a real `dsh` on PATH. "Compiles and tests pass, not run in
  a window" is a fine thing to write in a PR, and more useful than silence. Two
  bugs in this repo's history existed because "it compiles" was treated as "it
  works".
- **Keep the dsh origin's capability minimal.** The dsh page renders model output.
  It has `core:event:allow-emit` and should not gain more without a concrete
  reason.
- **Behaviour the user can see is a decision, not an implementation detail.**
  Autostart, global hotkeys and automatic update checks are deliberately absent
  because turning them on by default is the wrong default and there is no settings
  UI to turn them off. A PR adding one should say how a user disables it.

## Reporting bugs

Include your Windows version, whether `dsh` is on PATH (`where dsh`), and the
backend log at `%LOCALAPPDATA%\com.dsh.desktop\logs\dsh-backend.log`. The log is
overwritten each launch, so grab it before restarting.

For anything security-related, see [SECURITY.md](SECURITY.md) instead — please do
not open a public issue.

## Licence

Contributions are under [MIT](LICENSE), same as the project.
