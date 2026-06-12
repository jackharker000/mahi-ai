# Phase 0 — Single-Device Trust Backbone (`mahi-data`)

> Implementation-level plan for the second half of Phase 0: the local-only data/security layer
> every other domain writes through. Builds on [`../domains/05-data-sync-security.md`](../domains/05-data-sync-security.md)
> and the workspace defined in [`01-workspace-and-contracts.md`](./01-workspace-and-contracts.md)
> (this all lives in `crates/mahi-data`). Phase 0 = one device, no sync, no enrollment — but the
> schema reserves every column v1 needs so structural expansion requires no migration.

## 1. SQLite schema (SQLCipher-encrypted file)

### Key/identity metadata

```sql
CREATE TABLE identity (
    id              TEXT PRIMARY KEY,         -- UUID, stable across devices
    created_at      INTEGER NOT NULL,         -- Unix ms
    schema_version  INTEGER NOT NULL DEFAULT 1
);

CREATE TABLE device_keys (
    device_id           TEXT PRIMARY KEY,
    identity_id         TEXT NOT NULL REFERENCES identity(id),
    public_key_x25519   BLOB NOT NULL,        -- 32B, ECDH (v1 key agreement)
    public_key_ed25519  BLOB NOT NULL,        -- 32B, signing (audit) — separate keypairs, two columns
    key_fingerprint     TEXT NOT NULL UNIQUE, -- hex SHA-256 of public keys
    enrolled_at         INTEGER NOT NULL,
    revoked_at          INTEGER,              -- NULL = active
    is_local_device     INTEGER NOT NULL DEFAULT 1
);

CREATE TABLE kdk_registry (                   -- derivation labels only; key bytes never stored
    kdk_id          TEXT PRIMARY KEY,         -- "kdk.<collection_type>"
    collection_type TEXT NOT NULL UNIQUE,
    hkdf_info       BLOB NOT NULL,            -- 32B context label
    derived_at      INTEGER NOT NULL,
    rotated_at      INTEGER                   -- rotation is v1; column is the hook
);

CREATE TABLE dek_nonce_log (                  -- append-only; DB-enforced nonce uniqueness
    record_id       TEXT NOT NULL,
    collection_type TEXT NOT NULL,
    nonce           BLOB NOT NULL UNIQUE,     -- 12B AES-GCM nonce
    created_at      INTEGER NOT NULL,
    PRIMARY KEY (record_id, collection_type)
);
```

### Collection tables — shared encrypted-envelope pattern

Every collection table has the same shape:

- **Common plaintext metadata:** `id`, `schema_version`, `author_device_id`, `created_at`, `updated_at`, `is_tombstoned`, plus **reserved-for-v1** columns `owner_id`, `acl_ref` (multi-user) and `vc_clock` (vector clock, msgpack `Map<device_id,u64>`), all NULL in Phase 0.
- **Per-table plaintext index fields:** the minimum non-PII columns needed to query/route without decrypting.
- **Encrypted envelope:** `encrypted_payload BLOB` (AES-256-GCM ciphertext+tag) + `payload_nonce BLOB` (12B).

Exemplar (`messages`); the other tables differ only in their index fields:

```sql
CREATE TABLE messages (
    id TEXT PRIMARY KEY, schema_version INTEGER NOT NULL DEFAULT 1,
    conversation_id TEXT NOT NULL REFERENCES conversations(id),
    author_device_id TEXT NOT NULL REFERENCES device_keys(device_id),
    created_at INTEGER NOT NULL,              -- immutable once written
    updated_at INTEGER NOT NULL, is_tombstoned INTEGER NOT NULL DEFAULT 0,
    owner_id TEXT, acl_ref TEXT, vc_clock BLOB,
    role TEXT NOT NULL,                       -- 'user'|'assistant'|'tool'|'system'
    sequence_num INTEGER NOT NULL,            -- monotonic per conversation
    encrypted_payload BLOB NOT NULL, payload_nonce BLOB NOT NULL,
    UNIQUE(conversation_id, sequence_num)
);
CREATE INDEX idx_messages_conv_seq ON messages(conversation_id, sequence_num) WHERE is_tombstoned=0;
```

| Table | Plaintext index fields (beyond common) |
|---|---|
| `conversations` | `space_id`, `mode_at_creation` ('A'–'D') |
| `memory_records` | `space_id`, `kind` (fact/preference/project_note/system), `user_visible`, `user_confirmed` |
| `skills` | `slug` UNIQUE, `scope`, `installed_by`, `version` |
| `settings` | `setting_key` UNIQUE (namespaced, e.g. `core.theme`) |
| `task_items` | `origin_device_id`, `assigned_device_id`, `status`, `expires_at` (queue routing without decryption) |
| `artifacts` | `conversation_id`, `artifact_kind`, `content_type`, `size_bytes`, `blob_storage` ('inline'\|'file'), `blob_path` |
| `checkpoints` | `conversation_id`, `captured_at`, `mode_at_capture` |

**Artifact blob strategy:** ≤64 KB inline in `encrypted_payload`; larger artifacts written as an
encrypted file under the app sandbox (`blob_path`), same AES-GCM envelope, with the encrypted file
header kept in the row so metadata never requires loading the file. Avoids SQLite page bloat.

### Security/policy tables

```sql
CREATE TABLE audit_events (
    id TEXT PRIMARY KEY, schema_version INTEGER NOT NULL DEFAULT 1,
    device_id TEXT NOT NULL REFERENCES device_keys(device_id),
    agent_session_id TEXT,
    event_type TEXT NOT NULL, actor TEXT NOT NULL,        -- 'User'|'Agent'|'System'
    resource_ref TEXT, outcome TEXT NOT NULL,             -- 'Allowed'|'Denied'|'Pending'
    metadata BLOB NOT NULL, metadata_nonce BLOB NOT NULL, -- encrypted JSON (may contain PII)
    timestamp INTEGER NOT NULL,
    prev_hash BLOB NOT NULL,                              -- SHA-256 of previous row; zeros for row 0
    event_hash BLOB NOT NULL UNIQUE,                      -- SHA-256(id||prev_hash||device_id||event_type||outcome||timestamp)
    signature BLOB NOT NULL                               -- Ed25519 over event_hash
);  -- append-only at the application level; no DELETE path exists
CREATE INDEX idx_audit_latest ON audit_events(device_id, timestamp DESC);

CREATE TABLE permission_grants (
    id TEXT PRIMARY KEY, subject TEXT NOT NULL, scope_json TEXT NOT NULL,
    tier TEXT NOT NULL,                                   -- 'AllowOnce'|'AllowAlways'|'Deny'
    access_level TEXT NOT NULL,                           -- 'ReadOnly'|'Click'|'Full'
    device_id TEXT NOT NULL REFERENCES device_keys(device_id),
    prompt_text TEXT NOT NULL,                            -- verbatim text shown to the user
    granted_at INTEGER NOT NULL, expires_at INTEGER, revoked_at INTEGER,
    last_used_at INTEGER, use_count INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX idx_grants_subject ON permission_grants(subject, device_id) WHERE revoked_at IS NULL;

CREATE TABLE hard_rules (
    id TEXT PRIMARY KEY, name TEXT NOT NULL, description TEXT NOT NULL,
    predicate_dsl TEXT NOT NULL, action TEXT NOT NULL,    -- 'Block'|'RequireApproval'|'Warn'
    immutable INTEGER NOT NULL DEFAULT 0, enabled INTEGER NOT NULL DEFAULT 1,
    created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL
);  -- built-in immutable rules seeded by migration
```

Plus an FTS5 virtual table `memory_fts(content, record_id UNINDEXED)` for the Phase 0
`semantic_search` stub — see R2 below for the threat-model note.

## 2. Key hierarchy

```
root_key (256-bit, platform keystore — Secure Enclave / Keychain on Apple; 0600 file on Linux CI)
  └─ per-collection KDK   = HKDF(root_key, salt=device_id||"v0", info="mahi.kdk.<collection>")
       └─ per-record DEK  = HKDF(KDK, salt=record_id, info="mahi.dek.v0")   — derived per access, never persisted
SQLCipher file key        = HKDF(root_key, salt=device_id, info="mahi.sqlcipher.v0") — re-derived each open
```

```rust
pub trait PlatformKeystore: Send + Sync {
    fn load_or_create_root_key(&self) -> Result<RootKeyHandle, KeystoreError>;
    fn sign(&self, data: &[u8]) -> Result<[u8; 64], KeystoreError>;   // Ed25519, for audit
    fn device_public_key(&self) -> Result<[u8; 32], KeystoreError>;
    fn destroy_root_key(&self) -> Result<(), KeystoreError>;          // factory reset
}
// Swift shim: SecItemAdd/CopyMatching, kSecAttrAccessibleWhenUnlockedThisDeviceOnly,
// Secure Enclave access control where present. Linux shim: file key for CI/dev only.

impl KeyDerivationService {
    pub fn derive_dek(&self, collection: CollectionType, record_id: &RecordId)
        -> Result<Zeroizing<[u8; 32]>, CryptoError>;
    pub fn rotate_kdk(&self, collection: CollectionType)
        -> Result<RotationTicket, CryptoError>;   // Phase 0: Err(NotImplemented); interface locked
}

impl RecordCipher {
    // AAD = record_id bytes → ciphertext is bound to its record
    pub fn encrypt(dek, plaintext, aad) -> Result<(Vec<u8>, [u8; 12]), CryptoError>;
    pub fn decrypt(dek, ct_with_tag, nonce, aad) -> Result<Zeroizing<Vec<u8>>, CryptoError>;
}
```

- **Nonces:** `OsRng`, logged to `dek_nonce_log` before commit (DB-enforced uniqueness; collision ⇒ regenerate + retry); encrypt→log→insert in one transaction.
- **Zeroization:** `zeroize::Zeroizing` on all DEK material, dropped at call-scope end; `RootKeyHandle` zeroes on Drop.
- **When MLS lands (v1):** MLS epoch keys replace the `root_key → KDK` step into the *same* HKDF tree; `kdk_registry` gains `mls_epoch`; `derive_dek` signature unchanged — no consumer updates. **Must NOT change:** record IDs (AAD + salt), nonce-log uniqueness invariant, envelope column order.
- **Crates:** `aes-gcm`, `hkdf`, `sha2`, `ed25519-dalek`, `zeroize`, `rand` (added to workspace deps).

## 3. PermissionService

```rust
pub enum CheckResult { Granted(GrantId), Denied(DenyReason), PromptRequired(PromptToken) }

impl PermissionService {
    pub fn check(&self, subject, scope, required_access) -> Result<CheckResult, PermError>;
    pub fn grant(&self, subject, scope, tier, access_level, prompt_text, expires_at) -> Result<GrantId, PermError>;
    pub fn revoke(&self, grant_id) -> Result<(), PermError>;
    pub fn list(&self, filter) -> Result<Vec<PermissionGrant>, PermError>;
    pub fn sweep_expired(&self) -> Result<u32, PermError>;            // 60s interval task
    pub fn resolve_prompt(&self, token, decision) -> Result<CheckResult, PermError>;
}
```

**First-use prompt flow:** `check()` → `PromptRequired(token)` → caller parks; a
`PermissionPromptEvent` goes out on an in-process broadcast channel → surface shows the prompt →
surface calls `resolve_prompt(token, decision)` → grant/deny row written → the parked future is
signalled via a `tokio::sync::oneshot` keyed by token → invocation resumes. AllowOnce grants get
`expires_at = now + 60 s` (see R3); AllowAlways `NULL`; Deny persists until the user revokes it.

## 4. AuditLog

`append` (holding a `Mutex<ChainState>`): compute
`event_hash = SHA-256(event_id || prev_hash || device_id || event_type || outcome || timestamp)`,
sign with the device Ed25519 key, encrypt `metadata` under the audit-collection KDK, insert in one
transaction, advance the chain head.

- **PII-safe chain:** `metadata` is *excluded* from the hash input by design, so it can be redacted later (replaced with a tombstone + re-encrypted) without breaking the chain; redaction itself is a new audit event.
- **`verify_chain`:** replay from row 0 (or checkpoint), recompute hashes, check `prev_hash` linkage, verify signatures; returns `{ total_events, broken_at: Option<u64>, signature_failures }`.
- **Tamper tests:** corrupt row 50's hash ⇒ `broken_at: Some(50)`; delete row 75 ⇒ break at 76; mutate an `outcome` ⇒ mismatch at that row; property test: ∀ random mutation at k, `broken_at <= k`.

## 5. HardRulesEngine

**Decision: custom predicate DSL, not CEL, for Phase 0.** Rust CEL crates are pre-1.0 with
incomplete spec coverage and a heavy dependency tree; Phase 0 needs only 4–6 built-in rules over a
flat context. An S-expression predicate DSL (~200 LOC: `ToolIdMatches`, `FieldContains`,
`And/Or/Not`) is auditable with zero external attack surface — the right property for a security
gate. Swap for CEL in v1 when user-configurable rules justify it. The parser must `Err`, never
panic (fuzz target in T6).

Built-in immutable rules (seeded by migration): **no financial transactions** (Block), **no
credential exfiltration** (Block), **no bulk PII export** (RequireApproval), **no send without an
approval wall** (RequireApproval).

**Evaluation order (hard rules cannot be overridden by a standing grant):**

```
invocation → HardRulesEngine.evaluate
               Block → reject + AuditEvent(HardRuleBlock)
               Warn  → AuditEvent(HardRuleWarn) → PermissionService.check
               Allow → PermissionService.check → Denied | PromptRequired (park) | Granted → execute
```

## 6. SanitizationPipeline

Hook: `sanitize(SanitizationInput{content, source: ToolResult|WebFetch|UserInput|SkillInjection})
→ {content, flags, action: PassThrough|Redact|Block|MarkForReview}`, invoked by the core's
ContextAssembler before any tool result/web content enters the context window.

**Phase 0 implements:** instruction-pattern detector (regex for injection markers — flags +
`MarkForReview`); link-confirmation marker (URLs tagged; core injects an "external link — not
followed" note); credential-pattern scanner (AWS/GitHub/Bearer formats → `Redact`).
**Phase 0 stubs:** semantic-similarity injection detection (needs v1 vector index); per-skill
context-injection validation.

## 7. Fulfilling the contracts

`mahi-data`'s `LocalStore` implements the **trait family in `mahi-contracts`** (ConversationStore,
MessageStore, MemoryStore, SkillStore, TaskStore, AuditLog, PermissionService — plus
CheckpointStore/ArtifactStore/SettingsStore, added to contracts in Task 0.3; see the phase README
reconciliations).

- **semantic_search (Phase 0):** FTS5 full-text stub; replaced by a vector index (e.g. sqlite-vec/HNSW) in v1.
- **subscribe/notify:** per-process `tokio::sync::broadcast` of `ChangeEvent{collection, record_id, kind}` published after commit; handle drops cleanly; in-process only.
- **SQLCipher key:** re-derived from the root key on every connection open (`PRAGMA key`), never stored.

## 8. Test strategy

- **Crypto units:** HKDF determinism; 10k-concurrent-insert nonce uniqueness; encrypt/decrypt round-trip ∀ sizes 0–64 KB; wrong-AAD must fail.
- **Property tests (proptest):** chain integrity under arbitrary mutation; permission-grant idempotency; hard-rule determinism.
- **Airplane-mode provable (Linux CI):** run the offline integration suite inside an `ip netns` with loopback only, and assert via `strace -e trace=network` that there are **zero** non-loopback `connect/bind/sendto` calls. A deliberately-added `reqwest::get` must fail the job (T10 acceptance).
- **Permission flows:** park-and-resume on first use; AllowOnce expiry sweep re-prompts; Deny blocks until revoked; **hard rule pre-empts a standing AllowAlways grant**.

## 9. Task breakdown (S ≈ 1d, M ≈ 2–3d, L ≈ 4–5d)

These expand workspace Task 0.6 (see [`README.md`](./README.md) for the merged sequence):

| # | Task | Size | Acceptance |
|---|---|---|---|
| T1 | Schema + versioned migrations (all DDL, FTS5, seeded hard rules) | M | migrations idempotent; all tables present |
| T2 | `PlatformKeystore` trait + Linux file shim (Apple shims compile-stubbed) | M | load → sign → verify round-trip on Linux |
| T3 | KeyDerivationService + RecordCipher | M | all §8 crypto units pass; 10k nonce test green |
| T4 | SQLCipher `LocalStore` — all store traits + subscribe + FTS5 stub | L | CRUD on all collections; change events emitted; wrong key fails open |
| T5 | AuditLog (chain + signing + verify) | M | tamper tests pass; clean 1k-event chain verifies |
| T6 | HardRulesEngine (DSL parser ~200 LOC + evaluator + fuzz target) | S | 4 built-ins evaluate correctly; parser never panics |
| T7 | PermissionService (check/grant/revoke/sweep/resolve_prompt) | M | all §8 flow tests pass |
| T8 | SanitizationPipeline (3 detectors) | S | 10 known injection patterns flagged; credentials redacted |
| T9 | Evaluation-order wiring + integration test (stub ToolInvokeContract) | M | Block/Warn/Approve paths chained correctly |
| T10 | Airplane-mode CI job (netns + strace) | S | suite passes offline; injected network call fails CI |

## 10. Risks & open items

- **R1 — keypair duality:** X25519 (ECDH) and Ed25519 (signing) are separate keypairs → **two columns** in `device_keys` (adopted in the DDL above); the Linux shim implements both.
- **R2 — FTS5 plaintext memory:** memory content is cleartext *inside* the SQLCipher-encrypted file (FTS5 can't index ciphertext). Acceptable iff the threat boundary is the encrypted file at rest — confirm before shipping.
- **R3 — AllowOnce expiry = 60 s** is inferred, not specced; confirm the value and grant-time vs last-use-time semantics (`last_used_at` supports either).
- **R4 — `rotate_kdk` stub:** must be real before v1 (restartable batch re-encryption); add a startup integrity check (one record decrypts per collection) so a missed rotation fails loudly, not silently.
- **R5 — DSL robustness:** malformed predicates must parse to `Err`, never panic; fuzz-tested.
- **R6 — broadcast lag:** a slow subscriber past channel capacity gets `Lagged` and misses events; `SubscriptionHandle` must expose `lagged()` so observers re-sync from the DB — correctness issue for the task-observer pattern.
- **R7 — chain-head mutex:** serialized audit appends are fine single-device; a high-frequency coding agent could bottleneck — document the write-ahead trade-off (weakens real-time tamper evidence) before changing it in v1.
