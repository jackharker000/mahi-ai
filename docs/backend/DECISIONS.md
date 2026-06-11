# Decision Records

ADR-lite records for the product-level calls flagged in [`README.md`](./README.md) §10.

> **Status convention:** `provisional` = adopted as the working default to unblock Phase 0
> planning; the owner can override any of these, and overriding early is cheap. `accepted` =
> confirmed by the owner.

---

## D0 — Shared-core language & runtime

**Status:** provisional · **Decides:** README §5 (Decision #0)

**Context.** Mode A requires the agent core to run *on the iPhone, fully offline*, while the same
brain must also run as a Mac daemon, a Linux-capable CLI, and (mode D) hosted. A TypeScript core
would force a second native implementation for mode A; a Swift core is weak off-Apple.

**Decision.** A **portable Rust core** (orchestration, tooling logic, data/sync/crypto,
mode-selection, QUIC/tunnel transport) behind a stable FFI, with **thin Swift shims** for
genuinely Apple-native I/O: VideoToolbox encode/decode, MLX / Apple Foundation Models inference,
ScreenCaptureKit, IOKit thermal, Bonjour, Keychain/Secure Enclave.

**Consequences.** One implementation of every contract; slower iteration than TS and FFI
ergonomics to manage (async streaming across the boundary); the Rust ecosystem aligns with the
hard parts (`boringtun`, `quinn`, CRDT and crypto crates).

---

## D1 — Compute-mode preference order

**Status:** provisional · **Decides:** README §6

**Context.** The feature spec writes "degrade gracefully (D→C→B→A)", but the local-first ethos
implies preferring your own Mac over a hosted API. "Preference order" and "degradation direction"
were being conflated.

**Decision.** **Local-first preference:** when a Mac is reachable, prefer **B (LAN) > C (remote)**;
use **D (hosted)** only when no Mac is reachable (and cost preference allows); **A (on-device)** is
always available, is the only option offline, and is the explicit preference when the user selects
privacy/free. The spec's "D→C→B→A" is read as the *degradation direction* (most-connected to
least-connected), not preference. A pinned mode is never silently overridden — unavailability
emits an explicit pin-violation event.

**Consequences.** The `InferenceRouter` policy in domain ③ §4 stands as designed. Hosted spend
becomes the exception, not the default, for Mac owners.

---

## D2 — Key-recovery model

**Status:** provisional · **Decides:** README §7 item 17, §9 risk "key loss"

**Context.** Zero-knowledge E2E sync means losing the root key loses all synced data. Recovery
options trade convenience against the zero-knowledge guarantee.

**Decision.** Default to a **passphrase-wrapped key backup** (user-held passphrase wraps the root
key; the vendor/relay never holds recoverable material), with **opt-in iCloud Keychain escrow** as
a convenience layer for users who choose it. Final recovery UX must be settled **before v1 sync
ships**; Phase 0 only requires the key-rotation/backup *interface* to exist.

**Consequences.** Preserves the local-first/zero-knowledge posture by default; users who opt out
of any backup accept data loss on key loss (stated plainly in onboarding).

---

## D3 — Build priority

**Status:** provisional · **Decides:** README §10 item 4

**Context.** Mac takeover (§5.4 of the feature spec) is the stated market differentiator; should
it jump ahead of sync/remote in the roadmap?

**Decision.** **Keep the Phase 0→5 order.** Takeover's hard dependencies — pairing, the secure
multiplexed bus, session management (Phase 2) and the tunnel for remote use (Phase 3) — must exist
first regardless; pulling the streaming pipeline earlier would not ship a usable takeover sooner.
Takeover remains the headline target the earlier phases build toward.

**Consequences.** First public-facing differentiated milestone is Phase 4; Phases 1–3 each still
ship a usable product increment (assistant → Mac brain → continuity/remote).
