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

## Regenerating the icons

The icon source is `favicon.svg` from dsh's frontend package (the DeepSeek whale).
`scripts/gen-icons.js` renders two things from it:

- `app-icon.png` — 1024×1024, white whale on the brand-blue gradient tile. Feed it
  to `npx tauri icon` to produce the full set plus `.ico`.
- `src-tauri/icons/tray.png` — transparent background, blue whale, so it stays
  visible on a light Windows tray.

```bash
node scripts/gen-icons.js
npx tauri icon app-icon.png
```

The script locates `favicon.svg` itself: `require.resolve` first (dsh installed
locally), then dsh's global install under `npm root -g`. If neither works it errors
with the paths it tried. Override explicitly:

```powershell
$env:DSH_FAVICON = 'C:\path\to\favicon.svg'
node scripts/gen-icons.js
```

Committed output is already in the repo, so you only need this if the upstream
icon changes.

## Releasing

Pushing a `v*` tag runs `.github/workflows/release.yml`: build, sign, and open a
**draft** release with the installer and `latest.json` attached. Review it, then
publish by hand.

```bash
# The tag version must match `version` in tauri.conf.json, or the updater will not
# consider the release newer than what users already have.
git tag vX.Y.Z && git push --tags
```

### Channels

A tag whose version carries a prerelease suffix goes to the **alpha** channel; a
bare one goes to **latest**. The workflow decides by looking for a `-` in the tag.

```bash
git tag v0.1.1-alpha.1 && git push --tags   # alpha
git tag v0.1.1         && git push --tags   # latest
```

The two are published differently, and the difference is load-bearing:

- **latest** relies on GitHub's own `releases/latest/download/` alias, which
  resolves to the newest release that is neither a draft nor a prerelease. So a
  stable release is created as a **draft** for review, and users see nothing until
  you publish it.
- **alpha** cannot use that alias — skipping prereleases is exactly what it does.
  Instead the updater points at one permanent prerelease tagged `alpha`, and the
  workflow overwrites its `latest.json` on every prerelease build. A prerelease is
  therefore **published outright, not drafted**: the mirror step downloads
  `latest.json` from it, and a draft's assets are not reachable that way.

The `alpha` release exists only to hold that manifest. Do not delete it, and do not
publish it as latest — the endpoint is a fixed URL (`SHELL_MANIFESTS` in
`main.rs`), so a missing one takes the whole alpha channel down. Until the first
prerelease is pushed, the URL 404s and the panel reports "该通道还没有发布",
which is the intended empty state rather than an error.

### Signing keys

Updates are signature-verified and an unsigned update is refused, so a fork needs
its own keypair:

```bash
npx tauri signer generate -w ~/.tauri/dsh-desktop.key
```

Put the **public** key in `plugins.updater.pubkey` in `src-tauri/tauri.conf.json`,
and add one Actions secret, `TAURI_SIGNING_PRIVATE_KEY`, holding the private key
file's *contents* (not its path).

**If you generated the key without a password, do not create
`TAURI_SIGNING_PRIVATE_KEY_PASSWORD` at all.** GitHub rejects empty secret values,
so you would have to put something in it — and Tauri treats any non-empty value as
a real password, tries to decrypt with it, and fails:

```
failed to decode secret key: incorrect updater private key password:
Wrong password for that key
```

The failure is easy to misread, and worth knowing about in general: the installer
is built and reported as built, and only the *signing* step errors afterwards. A
successful bundle says nothing about whether the release will work. The workflow
still references the variable; with no such secret it resolves to an empty string,
which is what a passwordless key needs.

Keep the private key backed up. Lose it and you can never ship an update to
existing installs again — the public key is already compiled into their binaries,
so a new keypair silently strands every one of them. Leak it and anyone can push an
update your users' clients will accept as authentic.

## Reporting bugs

Include your Windows version, whether `dsh` is on PATH (`where dsh`), and the
backend log at `%LOCALAPPDATA%\com.dsh.desktop\logs\dsh-backend.log`. The log is
overwritten each launch, so grab it before restarting.

For anything security-related, see [SECURITY.md](SECURITY.md) instead — please do
not open a public issue.

## Licence

Contributions are under [MIT](LICENSE), same as the project.
