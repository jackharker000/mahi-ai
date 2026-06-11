# Mahi AI

A **local-first, multi-surface personal AI assistant** — iOS, Mac, and CLI sharing one
portable Rust agent core, with **four compute modes** the app auto-selects and degrades
between:

| Mode | What it is |
|---|---|
| **A — On-device** | Small model on the phone, fully offline |
| **B — Mac LAN** | Your Mac is the brain, over the local network (big models, full tools) |
| **C — Mac remote** | Same as B, from anywhere, over a secure tunnel |
| **D — Hosted** | Cloud model API when no Mac is reachable |

> **Status: early — Phase 0.** Workspace, contracts, and trust backbone are under
> construction. Expect stubs, churn, and unannounced breakage.

## Repo layout

| Path | What it is |
|---|---|
| `crates/mahi-contracts` | Shared contract types and traits — the seams every other crate codes against |
| `crates/mahi-data` | Local store, identity/keys, permissions, audit log (trust backbone) |
| `crates/mahi-compute` | Inference router and providers for modes A–D (`hosted` feature gates cloud providers) |
| `crates/mahi-agent-core` | The agent loop: sessions, context, routing, approval gate |
| `crates/mahi-tooling` | Uniform tool contract, registry, built-in tools (`net` feature gates network tools) |
| `crates/mahi-connectivity` | Device-to-device transport: discovery, pairing, multiplexed bus |
| `crates/mahi-ffi` | UniFFI bindings exposing the core to Swift |
| `bins/mahi-cli` | Command-line surface (`mahi` binary) |
| `bins/mahi-daemon` | Mac/home-server daemon |
| `macos/` | SwiftUI macOS app (XcodeGen project consuming the UniFFI xcframework) |
| `docs/backend/` | Architecture plan, decision records, Phase 0 implementation plan |

## Build & test the Rust core

Needs stable Rust (MSRV 1.82). The core builds and tests on Linux and macOS:

```sh
make build   # cargo build --workspace
make test    # cargo test --workspace
make lint    # rustfmt check + clippy -D warnings (same as CI)
```

## Run the CLI

```sh
make cli                 # in-memory store + mock provider (no keys, no network)
./scripts/dev.sh --help  # pass your own flags instead of the defaults
```

The daemon: `make daemon`.

## Build the Mac app (macOS only)

Requires Xcode and [XcodeGen](https://github.com/yonaskolb/XcodeGen) (`brew install xcodegen`),
plus the Apple Rust targets (`rustup target add aarch64-apple-darwin x86_64-apple-darwin aarch64-apple-ios`):

```sh
make app
```

This builds `mahi-ffi` for the Apple targets, generates the UniFFI Swift bindings into
`macos/MahiKit/Generated/`, packages `macos/Frameworks/Mahi.xcframework`, then runs
`xcodegen generate` and `xcodebuild` for the `Mahi` scheme. To build just the framework:
`make xcframework`.

## Architecture & decisions

- **Architecture overview:** [`docs/backend/README.md`](docs/backend/README.md) — the five
  domains, cross-domain contracts, and the phased roadmap.
- **Stack decisions:** [`docs/backend/DECISIONS.md`](docs/backend/DECISIONS.md) — ADR-lite
  records (Rust core + Swift shims, compute-mode preference order, key recovery, build order).

## CI

- `rust.yml` — fmt, clippy (`-D warnings`), tests, and build for the whole workspace, plus
  feature-gated tests (`mahi-compute --features hosted`, `mahi-tooling --features net`).
- `macos.yml` — advisory unsigned build of the macOS app (xcframework → xcodegen → xcodebuild).

## License

MIT
