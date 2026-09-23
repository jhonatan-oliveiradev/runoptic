# RunOptic

**See what your AI tools are doing.**

Local-first observability for AI coding agents, providers, gateways and local
models.

> **Status:** early development / Gate 2. The Windows baseline is being
> established before WSL discovery and the normalized telemetry architecture
> are introduced.

## What RunOptic is becoming

RunOptic is a developer-facing observability layer for AI-assisted software
development. It is designed to answer operational questions without forcing all
traffic through a RunOptic cloud:

- Which agent is working, waiting, done or idle?
- How much quota remains and when does it reset?
- How many input, output, cached and reasoning tokens were used?
- What did a model, provider or project cost?
- Where is the workload running — Windows, WSL or a local runtime?
- How healthy and performant are local models and API-backed sessions?

The product is **Windows + WSL first**, provider-agnostic and local-first.

## Planned surfaces

- **RunOptic HUD** — compact real-time status.
- **RunOptic Dashboard** — historical usage, costs and performance.
- **RunOptic Doctor** — diagnostics for collectors and environments.
- **RunOptic CLI** — scriptable status and troubleshooting.

## NX Agent telemetry

RunOptic can ingest the privacy-preserving `nx.telemetry.v1` stream emitted by NX Agent on its local server (default port `48666`):

```text
POST /v1/telemetry/nx-agent
GET  /v1/telemetry/nx-agent
```

The collector records model/provider identity, latency, token counters, tool outcomes and permission denials. NX Agent does not send prompt text, response text, tool arguments or home-state payloads through this protocol.

## Current Gate 2 baseline

The current implementation is derived from the Windows port of Codenotch
`v1.15.0`. Gate 2 is intentionally conservative: first establish an
independent, reproducible RunOptic Windows build; then add new architecture.

Current technical direction:

- Rust + Tauri 2 / WebView2
- native Windows integration
- provider adapters inherited from the upstream baseline
- NSIS packaging
- read-only access to provider-owned credentials where practical
- explicit stale/error states instead of fabricated telemetry

## Build the Windows app

Prerequisites:

- Windows 11
- Rust with the MSVC toolchain
- WebView2 runtime
- Node.js for the UI syntax check / Tauri CLI packaging

From `windows/`:

```powershell
node scripts/check-ui-scripts.mjs
cargo build --locked
cargo test --locked
cargo clippy --all-targets --locked
```

The imported source directories still use their upstream physical names during
Gate 2. Package/binary identity is being migrated independently so path renames
do not obscure functional regressions.

## Design principles

- **Local-first** — no prompt-routing requirement just to observe development.
- **Honest telemetry** — unknown stays unknown; official, derived and estimated
  observations must be distinguishable.
- **Low overhead** — monitoring cannot become the performance problem.
- **Provider-agnostic** — no vendor owns the domain model.
- **Calm instrumentation** — dense, precise and operational rather than chatty.

## Brand

**RunOptic**  
*See what your AI tools are doing.*

Visual direction: optics × telemetry × runtime state, with a Carbon/Graphite
foundation and Signal green (`#A7F432`) reserved for active/healthy state.

## Upstream and license

RunOptic is an independent project derived in part from
[Codenotch](https://github.com/vinzdg/codenotch), originally by Vinz.

See [UPSTREAM.md](./UPSTREAM.md) for the imported baseline and attribution.
The upstream MIT license and copyright notice are preserved in
[LICENSE](./LICENSE).

RunOptic is not an official Codenotch edition and does not imply endorsement by
the upstream author.
