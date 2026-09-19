# RunOptic for Windows

RunOptic is a local-first observability app for AI coding agents and providers.
The Windows application is built with Rust + Tauri 2 / WebView2.

This tree began from the Codenotch Windows implementation imported at
`v1.15.0`. During Gate 2, physical source directories intentionally retain
their upstream names while the shipped product identity moves to RunOptic.

## Current providers inherited from the baseline

- Claude Code
- Codex
- Cursor
- Grok
- Antigravity

Provider availability depends on the relevant local tool/account being present.
RunOptic prefers real provider/tool state and degrades visibly to stale or
inferred state when necessary.

## Development

From this `windows/` directory:

```powershell
node scripts/check-ui-scripts.mjs
cargo build --locked
cargo test --locked
cargo clippy --all-targets --locked
```

Run the app:

```powershell
.\target\debug\runoptic.exe
.\target\debug\runoptic.exe doctor
```

Release build:

```powershell
cargo build --release --locked
.\target\release\runoptic.exe doctor
```

## Installer

The package workflow builds the helper separately and then asks Tauri to create
an NSIS installer:

```powershell
cargo build --release --locked -p runoptic-hook --target-dir target/hook
cd codenotch
npx --yes @tauri-apps/cli@2.11.4 build --config tauri.bundle.conf.json -- --locked
```

Expected public artifact name:

```text
RunOptic-Setup.exe
```

## Local data

RunOptic stores its own settings, diagnostic logs, persisted usage snapshots and
glyph overrides under:

```text
%APPDATA%\runoptic
```

Provider credentials remain owned by their original tools and should be treated
as read-only.

## Gate 2 constraint

Do not add WSL discovery yet. First keep build, tests, Clippy, packaging,
installation, `runoptic doctor` and uninstall green under the independent
RunOptic identity.

## Attribution

See the repository root [UPSTREAM.md](../UPSTREAM.md) and
[LICENSE](../LICENSE).
