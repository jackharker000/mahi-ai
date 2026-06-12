# Domain ⑤ — Data, Sync, Continuity & Security/Privacy

> Detailed design for the data plane + security backbone — the trust layer every other domain
> writes through. See [`../README.md`](../README.md) for the synthesis. Module/file paths are
> **illustrative** pending the shared-core language decision.

## 1. Overview & responsibilities

- **Identity & key management:** single logical identity, per-device keypairs, cross-device key exchange.
- **Durable local storage:** all first-party data on each device; authoritative local copy.
- **Sync engine:** E2E-encrypted replication; P2P-first, relay-optional, local-only-possible.
- **Continuity:** checkpoint model so any device can resume any session.
- **Task queue:** single distributed queue observable from every surface.
- **Capability registry:** per-device knowledge of what each device can do.
- **Permission service:** first-use prompts, tier/scope/expiry, revocation.
- **Audit log:** append-only, tamper-evident record of every consequential action.
- **Hard-rules engine:** policy evaluator that blocks prohibited actions before execution.
- **Prompt-injection & sanitization hooks:** insertion points for untrusted-content filtering.

## 2. Component breakdown

| Component | Responsibility | Isolation |
|---|---|---|
| IdentityService | Identity, device enrollment, key derivation | Secure Enclave where available |
| KeyStore | Device key, identity key, per-record DEKs | OS keychain + optional enclave |
| LocalStore | Durable per-device DB; canonical for that device | Encrypted at rest |
| SyncEngine | Produces/consumes encrypted sync envelopes; merge | Background; uses Connectivity transport |
| SessionContinuityManager | Writes/reads checkpoints; resolves latest holder | On top of SyncEngine |
| TaskQueueService | Append-only distributed task log; routes to capable device | On LocalStore + SyncEngine |
| CapabilityRegistry | Stores per-device capability declarations; serves routing | Read by Core; written by Compute/Tooling |
| PermissionService | Evaluates grants; stores them; emits revocations | Called synchronously on every tool invocation |
| AuditLog | Append-only, hash-chained event store | Isolated write path; no delete API |
| HardRulesEngine | Declarative policy evaluator; pre-execution gate | Runs before PermissionService approves |
| SanitizationPipeline | Hooks for untrusted content before the agent sees it | Called by Core before context injection |

## 3. Key interfaces & data models

```
Identity { id /* stable across devices */, publicKey: Curve25519, devices: DeviceID[] }
DeviceKey { deviceID, devicePublicKey: Curve25519, enrolledAt, revokedAt? }
// Key derivation: identity root (Secure Enclave) → per-collection KDK → per-record DEK (HKDF)

EncryptedRecord { id, collectionType: Conversation|Memory|Skill|Setting|Task|Artifact|AuditEvent,
  schemaVersion, authorDevice, vectorClock: Map<DeviceID,u64>,
  encryptedPayload /* AES-256-GCM, key = DEK(collectionType, recordID) */, payloadMAC, createdAt, updatedAt }

// Handed to Connectivity as an opaque blob + routing metadata
SyncEnvelope { fromDevice, sessionToken, records: EncryptedRecord[], tombstones: RecordID[],
  vectorClockSnapshot, hmac /* identity-key signed */ }

TaskItem { taskID, originDevice, assignedDevice?, 
  status: Pending|Running|AwaitingApproval|Completed|Cancelled|Failed,
  approvalWall?: ApprovalRequest, payload: EncryptedRecord, capabilities: CapabilityRef[],
  createdAt, updatedAt, expiresAt?, auditTrail: AuditEventID[] }
// Cross-device observe: any device subscribes to the task collection; cancel/approve = write status update → synced

CapabilityEntry { deviceID, publishedBy: Compute|Tooling, capabilityID /* e.g. "llm.on-device.7b","tool.browser" */,
  spec: JSON, availableOffline, lastHeartbeat, ttl }

PermissionGrant { grantID, subject: ToolID|FeatureID, scope: string[],
  tier: AllowOnce|AllowAlways|Deny, accessLevel: ReadOnly|Click|Full,
  grantedAt, expiresAt?, revokedAt?, deviceID /* grants are per-device */, promptText }

AuditEvent { eventID, prevHash: SHA256 /* chain */, deviceID, agentSessionID,
  eventType: ToolCall|FileAccess|UIAction|SyncOp|PermissionChange|HardRuleBlock|ApprovalWall,
  actor: User|Agent|System, resourceRef, outcome: Allowed|Denied|Pending, metadata, timestamp,
  signature /* device key signs eventID+prevHash+timestamp */ }

SandboxPolicy { policyID, appliesTo: ToolID|"*", networkAccess: None|LoopbackOnly|AllowList,
  fsAccess: None|TempOnly|ScopedPaths, allowedScopedPaths[], processSpawn, clipboardAccess,
  hardRuleRefs: HardRuleID[], version }

HardRule { ruleID, name, description /* surfaced in UI */, matchPredicate: CEL,
  action: Block|RequireApproval|Warn, immutable }
// Built-in immutables: no financial transactions, no credential exfiltration,
//                      no bulk PII export, no sending without an approval wall
```

### Merge / conflict strategy
- **Conversations:** messages immutable once flushed; concurrent appends merged by causal order in the vector clock, tie-broken by `authorDevice`.
- **Memory & Skills:** field-level LWW (CRDT map); destructive overwrite requires user approval; a "conflict pending review" state for contradictory facts.
- **Settings:** LWW per key.
- **Audit log:** append-only (no merge conflicts); each device keeps its own chain; identity service cross-signs on sync.

## 4. Mode matrix

| Concern | A (offline) | B/C (Mac) | D (cloud) |
|---|---|---|---|
| Memory & skills | full synced copy (stale if unsynced) | live sync; Mac authoritative for heavy content | sync via relay; user controls what uploads |
| Task scheduling | phone-local; phone-capable tasks only | routes to Mac; Mac executes; phone observes | any device; cloud relay queues |
| Continuity | checkpoint saved locally; resume on reconnect | P2P checkpoint handoff in seconds | relay-mediated |
| Local-only mode | SyncEngine disabled; no outbound traffic (network-policy verified) | n/a | n/a |
| Auditability | full local signed log; no phoning home | local + P2P merge | cloud-replicated, user-auditable |

## 5. Deferred decisions (in order)
1. **Local store** (resolve first): **SQLite + SQLCipher** (native iOS/Mac/CLI, transparent encryption); validate write throughput under audit-log volume (switch only if p95 write >20 ms).
2. **Sync engine / conflict:** **Automerge** (CRDT) if per-entity docs stay <500 KB; otherwise split — Automerge for conversations, LWW log for settings. (Yjs has JS-bridging friction; custom log-based sync = high cost.)
3. **Encryption + key management:** HKDF + AES-256-GCM per record for MVP; design a key-rotation interface; evaluate **MLS (RFC 9420)** for the multi-device key-agreement layer before v1; defer Signal Double Ratchet unless real-time message secrecy is needed.
4. **Relay** (last; local-only mode defers it entirely): dumb encrypted blob store + routing; self-hostable (Cloudflare Worker + R2 / Fly.io / coturn); relay never sees plaintext.

## 6. Top risks
- **Key loss vs zero-knowledge** — losing the root key loses all synced data. Options: iCloud-Keychain backup (breaks zero-knowledge), passphrase-wrapped backup, social recovery. **Hardest product call — decide before v1.**
- **Sync conflicts on memory** — field-level merge reduces them but semantic contradictions need a "conflict pending review" UX.
- **Audit-log tamper evidence** — per-device hash chains are good, but a compromised device can rewrite its own chain pre-sync; cross-device cross-signing hardens it (added complexity).
- **Multi-user (§8) later** — design record schema with `ownerID` + `aclRef` now, even if unpopulated.
- **Offline-provable no-phone-home** — local-only mode must be enforced at build time (no background-network entitlement on iOS / localhost-bind on Mac/CLI), not just a runtime flag.
- **Approval walls + UX latency** — hard rules block synchronously; an offline approval must park durably (`AwaitingApproval` + `expiresAt`); the expiry policy is undefined.

## 7. Phased build
- **MVP (local-only, single device):** LocalStore (SQLite+SQLCipher) with all collection schemas; IdentityService (single device); KeyStore (device key + HKDF DEKs); PermissionService (AllowOnce/Always/Deny + first-use prompts); AuditLog (local chain); HardRulesEngine (immutable built-ins); SanitizationPipeline; the Core store contract + the Tooling permission/audit contract.
- **v1 (two-device P2P sync):** SyncEngine with the chosen CRDT; cross-device identity enrollment + key exchange; SessionContinuityManager + checkpoints; TaskQueueService (cross-device observe/cancel/approve); CapabilityRegistry; user-configurable hard rules; audit cross-signing; key-rotation interface.
- **Later:** relay design + hosted option; multi-device key recovery; audit export; multi-user ACL overlay (§8); remote-session auto-expiry + kill switch in all clients; relay-mediated capability routing (mode D).

### Illustrative module layout
`data-domain/{identity/IdentityService, store/LocalStore, audit/AuditLog, permissions/PermissionService, sync/SyncEngine}`
