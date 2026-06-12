# Phase 0 — Foundations & Contracts: Implementation Plan

> **Status:** ready to execute · builds on the [backend plan](../README.md) (§8 Phase 0) and the
> provisional decisions in [`DECISIONS.md`](../DECISIONS.md) (esp. D0: Rust core + Swift shims).
>
> Produced as two parallel implementation-design passes, reconciled here:
> 1. [`01-workspace-and-contracts.md`](./01-workspace-and-contracts.md) — Cargo workspace, the
>    `mahi-contracts` seam crate (real type sketches), UniFFI strategy, CI, hygiene.
> 2. [`02-trust-backbone.md`](./02-trust-backbone.md) — the `mahi-data` single-device trust layer:
>    SQLCipher schema, key hierarchy, permissions, hash-chained audit, hard rules, sanitization.

## What Phase 0 delivers

A compiling, CI-green Cargo workspace in which the **cross-domain contracts are locked**, the
**trust backbone works end-to-end on one device**, and a **"hello agent" streaming test** runs a
user turn through a mock provider into the encrypted store — the foundation all five domains then
build on in parallel.

## Reconciliations (the two plans, merged)

The passes were designed against the same seams; merging required five alignments, **canonical
choices in bold**:

1. **Crate naming/layout** — the trust plan's standalone `data-domain`/`data-domain-ffi` crates fold into the **workspace layout: `crates/mahi-data`, single `crates/mahi-ffi`** (trust tasks T0 and T11 are absorbed by workspace tasks 0.1 and 0.9).
2. **Store contract shape** — the trust plan sketched one monolithic `DataStore` trait; **canonical is the contracts crate's trait *family*** (ConversationStore, MessageStore, …) plus the composite handle; `mahi-data::LocalStore` implements the family.
3. **Contracts v0.1.0 gains three traits** — the trust schema and domain ① need **`CheckpointStore`, `ArtifactStore`, `SettingsStore`**, which the workspace pass omitted; they're added to Task 0.3's scope.
4. **`device_keys` carries two keypairs** — X25519 (ECDH, v1) and Ed25519 (signing) as **separate columns** (trust risk R1, adopted).
5. **Workspace dependencies** — add the trust plan's crypto set to `[workspace.dependencies]`: `aes-gcm`, `hkdf`, `sha2`, `ed25519-dalek`, `zeroize`, `rand`, plus `rusqlite` (SQLCipher feature) and a migration runner.

## Merged task sequence

Workspace tasks keep their `0.x` numbers; trust tasks `T1–T10` expand task 0.6. Two tracks
parallelize after 0.5.

| Order | Task | Source | Size |
|---|---|---|---|
| 1 | 0.1 Workspace skeleton (9 crates + toolchain) | workspace | S |
| 2 | 0.2 CI (fmt/clippy/test/doc + iOS/macOS/Linux cross-compile) | workspace | S |
| 3 | 0.3 `mahi-contracts` v0.1.0 — all seam types **+ Checkpoint/Artifact/Settings stores** | workspace+rec.3 | M |
| 4 | 0.4 `testkit` mocks (provider, tool contract, in-memory stores) | workspace | M |
| 5 | 0.5 Domain crate structural stubs | workspace | M |
| — | *— tracks split —* | | |
| 6a | **Track A (trust):** T1 schema/migrations → T2 keystore → T3 crypto → T4 LocalStore → T5 audit → T6 hard rules → T7 permissions → T8 sanitization → T9 eval-order wiring → T10 airplane-mode CI | trust | 6M+L+3S |
| 6b | **Track B (engine):** 0.7 compute routing skeleton → 0.8 agent-loop skeleton → 0.9 UniFFI + Swift package | workspace | M+L+L |
| 7 | 0.10 **"Hello Agent" e2e** (LocalDataStore + router + mock provider; joins the tracks) | workspace | M |
| 8 | 0.11 `mahi-daemon` SSE stub (axum, OpenAI-compatible) | workspace | S |

**Rough effort:** ≈ 9 engineer-weeks serially; the two tracks bring it to ~5–6 calendar weeks with
two engineers. (S ≈ 1d, M ≈ 2–3d, L ≈ 4–5d.)

## Exit criteria

- `cargo build/test --workspace` and clippy `-D warnings` green on Linux + macOS; iOS cross-compile produces `libmahi_ffi.a`.
- `mahi-contracts` v0.1.0 fully documented (`deny(missing_docs)`), compiling on all three targets, with a CHANGELOG and the semver discipline in force.
- `mahi-data` passes: CRUD on every collection, audit chain tamper tests, permission park-and-resume flow, hard-rule pre-emption test, and the **netns airplane-mode test** (zero non-loopback network syscalls; an injected network call fails CI).
- The `e2e_hello_agent` test streams a turn through the mock provider and persists the assistant message — in <5 s on a clean runner.
- `mahi-daemon` answers a streaming `curl` against `POST /v1/chat/completions`.

## Validate-early list (front-loaded unknowns)

1. **SQLCipher on iOS** — evaluate the CommonCrypto-backed build *before* committing to `bundled-sqlcipher` (OpenSSL conflict risk) — gate inside T4/0.6.
2. **FFI streaming + synchronous `cancel()`** — prove the cancellation path in Task 0.9 before any surface work depends on it.
3. **FTS5 plaintext-inside-SQLCipher boundary** (trust R2) — confirm the threat model before shipping memory search.
4. **AllowOnce expiry semantics** (trust R3: 60 s, grant-time) — product confirmation; cheap to change now.
5. **`async-trait` overhead on hot store paths** — benchmark `MessageStore::append` under a 100 tok/s stream early (workspace risk).
