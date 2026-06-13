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
| `mahi-compute` | `InferenceRouter` (local-first selection + pin), on-device provider, Anthropic + OpenAI-compatible **tool-calling** streaming providers (`hosted`); **managed local model runtime** (`local-llm`): curated quantized GGUF catalog, resumable downloads, `llama-server` lifecycle, local provider | 15 + 26 (hosted) + 57 (local-llm) |
| `mahi-tooling` | Uniform tool registry; file/shell/web tools; **Claude-style computer-use** (`ComputerController` + safety); **MCP client** (Claude-Desktop-compatible `mcp_servers.json`, namespaced tools); connector broker | 17 + 19 + 2 (mcp) |
| `mahi-agent-core` | `MahiEngine`: streaming agent loop, multi-step + **parallel** tool use, approval gate, **parallel subagents**, context compaction | 12 + 8 (tool loop) |
| `mahi-connectivity` | Multiplexed bus + loopback TCP transport + session/pairing scaffolding | 17 |
| `mahi-ffi` | **UniFFI** bridge → `MahiEngineHandle`: **streaming `TurnHandle`** (poll/cancel), model management (catalog/download/activate/status), hosted config, subagents; Swift bindings generate cleanly | 5 (handle) |

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
  and SwiftUI views: chat transcript + composer + live model badge + tool bubbles, conversation
  sidebar, a **Models** browser (download/run/delete local models, runtime status), an **Agents**
  launcher (parallel subagents), the **approval sheet** (command/diff/screenshot previews,
  allow-once/always/deny), and settings (incl. hosted API key + model).
- `project.yml` (XcodeGen), `Info.plist`, entitlements, and `scripts/build-xcframework.sh`.

It builds and runs on the in-process **`PreviewMockEngine`** so the UI works before the Rust core
is linked: `brew install xcodegen && (cd macos && xcodegen generate) && open macos/Mahi.xcodeproj`.

The macOS CI job (`build Mahi.app`) is **advisory** (`continue-on-error`); it needs a signing
identity and the steps below, so it does not gate the PR.

## Known gaps / next steps

1. **Vision for computer use.** Computer use drives the Mac from the Accessibility tree (text) +
   CGEvent today; feeding screenshots to a vision-capable model as image content (so it can *see*
   the screen like Claude) needs an image `ContentBlock` + provider plumbing. Text/AX works now.
2. **Signing + notarization.** The DMG is unsigned (first launch needs *System Settings → Privacy
   & Security → Open Anyway*). Notarization needs an Apple Developer ID.
3. **Connectivity beyond loopback.** mDNS discovery, the QUIC/Noise transport, the tunnel
   (mode C), and Mac-takeover streaming are scaffolded as contracts/stubs (Phase 2).
4. **Persistence niceties.** FTS/vector memory search, SQLCipher whole-file encryption, and audit
   cross-signing are deferred (see `docs/backend/phase-0/02-trust-backbone.md`).

## How the pieces map to the four compute modes

- **A (on-device):** a managed local `llama-server` runs curated quantized GGUF models the app
  downloads itself (no Ollama/Homebrew/Xcode needed) + the encrypted local store — fully offline.
- **B/C (Mac LAN/remote):** `mahi-daemon` serves inference; `mahi-connectivity` carries it (LAN
  today; tunnel is Phase 2).
- **D (hosted):** the Anthropic / OpenAI-compatible providers behind the `hosted` feature.
