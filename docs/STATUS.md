# Mahi — Build Status

A snapshot of what exists, what's verified, and what's next. Architecture lives in
[`docs/backend/`](./backend/README.md); stack decisions in [`docs/backend/DECISIONS.md`](./backend/DECISIONS.md).

## Verified in CI (Rust core — builds, lints, and tests on every push)

The entire Rust workspace builds on `cargo`, is **clippy-clean with `-D warnings`**, and passes
its test suite under the **pinned Rust 1.96** toolchain (`rust-toolchain.toml`), on Linux and macOS.

| Crate | What it does | Tests |
|---|---|---|
| `mahi-contracts` | Cross-domain seam types + traits + error taxonomy; `testkit` mocks | doc-tests |
| `mahi-data` | Encrypted SQLite store: per-record AES-256-GCM (HKDF key tree, file keystore), hash-chained audit log, permissions; `open_store` / `open_in_memory` | 4 + doc |
| `mahi-compute` | `InferenceRouter` (local-first selection + pin), on-device provider, Anthropic + OpenAI-compatible streaming providers (`hosted`) | 15 + 18 (hosted) |
| `mahi-tooling` | Uniform tool registry; file/shell/web tools; **Claude-style computer-use** (`ComputerController` + safety) ; connector broker | 17 + 13 |
| `mahi-agent-core` | `MahiEngine`: streaming agent loop, multi-step tool use, approval gate, **parallel subagents**, context compaction | 12 |
| `mahi-connectivity` | Multiplexed bus + loopback TCP transport + session/pairing scaffolding | 17 |
| `mahi-ffi` | **UniFFI** bridge → `MahiEngineHandle`; Swift bindings generate cleanly | — |

### Binaries (verified by running them)

- **`mahi` (CLI)** — interactive REPL over the full stack: streaming chat, `/tools`, inline
  approvals, and `/spawn` parallel subagents. On-device by default; `--features hosted` + an API
  key uses Anthropic/OpenAI. Run: `cargo run -p mahi-cli` or `./scripts/dev.sh`.
- **`mahi-daemon`** — the Mac "home server": an OpenAI-compatible HTTP API
  (`/health`, `/v1/models`, `/v1/chat/completions` streaming + buffered) over the router, so
  phones (modes B/C) can use this machine's brain. Run: `cargo run -p mahi-daemon` then
  `curl http://127.0.0.1:11434/v1/models`.
- **`uniffi-bindgen`** — generates the Swift bindings for the Mac app.

## Written, builds on a Mac (not compile-checked in CI — no Swift toolchain on the Linux runner)

The **macOS app** under [`macos/`](../macos/):

- `MahiKit` — Swift domain models, `MahiEngineProtocol`, `PreviewMockEngine`, and the
  `ComputerController` design (ScreenCaptureKit / Accessibility / CGEvent).
- `Mahi` app target — `MahiApp` (menu-bar + window + Settings), `AppModel` (engine view-model),
  and SwiftUI views: chat transcript + composer + mode badge, conversation sidebar, the
  **approval sheet** (command/diff/screenshot previews, allow-once/always/deny), and settings.
- `project.yml` (XcodeGen), `Info.plist`, entitlements, and `scripts/build-xcframework.sh`.

It builds and runs on the in-process **`PreviewMockEngine`** so the UI works before the Rust core
is linked: `brew install xcodegen && (cd macos && xcodegen generate) && open macos/Mahi.xcodeproj`.

The macOS CI job (`build Mahi.app`) is **advisory** (`continue-on-error`); it needs a signing
identity and the steps below, so it does not gate the PR.

## Known gaps / next steps

1. **FFI surface.** The shipped `mahi-ffi` is a *buffered* facade (`MahiEngineHandle.send` returns
   the full reply). The app's `FfiEngine` adapter targets a *streaming* FFI (per-token events +
   computer-use callback injection) and is kept **dormant** (`#if canImport(MahiStreamingFFI)`)
   until that lands. Next: expose the engine's native streaming over UniFFI (a `TurnHandle` with
   `poll_batch`/`cancel`) and a `ComputerController` callback interface, then flip the guard.
2. **Real computer-use on the Mac.** The Rust tool contract + safety are done and tested against a
   `MockComputerController`; the macOS `ScreenCaptureKit`/`CGEvent` implementation needs on-device
   wiring (and entitlements beyond the Phase-0 sandbox).
3. **Connectivity beyond loopback.** mDNS discovery, the QUIC/Noise transport, the tunnel
   (mode C), and Mac-takeover streaming are scaffolded as contracts/stubs (Phase 2).
4. **Persistence niceties.** FTS/vector memory search, SQLCipher whole-file encryption, and audit
   cross-signing are deferred (see `docs/backend/phase-0/02-trust-backbone.md`).

## How the pieces map to the four compute modes

- **A (on-device):** `OnDeviceProvider` + the encrypted local store — works fully offline today.
- **B/C (Mac LAN/remote):** `mahi-daemon` serves inference; `mahi-connectivity` carries it (LAN
  today; tunnel is Phase 2).
- **D (hosted):** the Anthropic / OpenAI-compatible providers behind the `hosted` feature.
