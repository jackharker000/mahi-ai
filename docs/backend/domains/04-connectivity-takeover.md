# Domain ④ — Connectivity, Pairing & Takeover Streaming

> Detailed design for all device-to-device transport. See [`../README.md`](../README.md) for the
> synthesis. Module/file paths are **illustrative** pending the shared-core language decision.

## 1. Overview & responsibilities

Owns every bit that crosses a device-to-device boundary: how devices find each other, establish/
revoke trust, maintain a persistent secure channel, and how that channel carries compute traffic,
sync, agent handoff, screen frames, and control events. It does **not** own agent logic, data
schemas, or computer-use safety policy — it transports their payloads under a typed multiplexed
envelope.

## 2. Component breakdown

- **DiscoveryService** — emits/listens for peer advertisements on the LAN; produces a live `PeerList`.
- **PairingManager** — one-time enrollment: QR/proximity challenge, key exchange, writes a `TrustRecord` to the device keychain (never to a third-party server).
- **TrustStore** — local keychain-backed store of `TrustRecord`s; source of truth for "is this peer allowed."
- **PeerRegistry** — in-memory `PeerDescriptor`s annotated with reachability, last-seen, latency, load hint; exported to Compute via `PeerRegistrySnapshot`.
- **SessionManager** — issues per-session `SessionToken`; idle timeout; kill-switch from either side; drives the Mac-side "phone is controlling" indicator; auto-expiry.
- **TunnelCoordinator** — mode C: hole-punching, relay fallback; produces a `TransportSocket` identical in API to the LAN socket (downstream is transport-agnostic).
- **RelayClient** — thin client to a self-hostable/provider relay; activated only when direct fails; relay sees only ciphertext.
- **SecureChannel** — wraps any `TransportSocket` in E2E encryption; exposes one `MultiplexedBus`.
- **MultiplexedBus** — single bidirectional encrypted connection multiplexed into named channels; handles framing, backpressure, reconnection.
- **ScreenStreamPipeline** — Mac capture → encode → packetize into `FrameChunk`s on `STREAM_VIDEO`; phone decode → render.
- **InputControlChannel** — phone touch/keyboard serialized as `InputEvent`s on `INPUT`; Mac reconstructs HID events.
- **FileTransferManager** — chunked bidirectional transfer over `FILE_XFR` with resume tokens.
- **ApprovalBroker** — serializes `ApprovalPrompt`s with preview blobs onto `APPROVAL`; waits for signed `ApprovalResponse`.

## 3. Key interfaces & data models

```
PeerDescriptor { peerId, displayName, publicKey,
  addresses: [{ transport: mdns|quic-direct|relay, addr }],
  lastSeen, latencyMs?, loadHint: 0..1 }
TrustRecord { peerId, pairingDate, localPrivKey: SecretRef, peerPublicKey,
  pairingMethod: qr|proximity, revoked, revokedAt? }
SessionToken { sessionId, peerId, issuedAt, expiresAt,
  capabilities: [inference|takeover|file_xfr], hmac /* signed w/ pairing key */ }

// MultiplexedBus logical channels (u8)
0x01 CONTROL      — session lifecycle, heartbeat, kill-switch (highest priority)
0x02 INFERENCE    — streaming inference tokens / request-response
0x03 SYNC         — handoff state, task-queue, checkpoints (Data-domain payload)
0x04 APPROVAL     — ApprovalPrompt + ApprovalResponse
0x05 STREAM_VIDEO — FrameChunk stream
0x06 INPUT        — InputEvent stream
0x07 FILE_XFR     — chunked file transfer
0x08 AUDIO        — reserved (later)

MultiplexedEnvelope { channel: u8, seqNo: u64, sessionId, payloadType, payloadBytes /* domain-owned */ }
InputEvent { type: touch_move|tap|scroll|pinch|key|modifier|clipboard_push,
  x?, y?, dx?, dy?, keyCode?, modifiers?[], clipboardPayload?, timestamp, agentGenerated: bool }
FrameChunk { frameId, chunkIdx, totalChunks, codec: h264|h265|av1|vp9, keyFrame, data, captureTimestamp }
ApprovalPrompt { promptId, taskId, description, previewType: screenshot|diff|command, previewData,
  actions: [approve|deny|modify], expiresAt }
```

## 4. Mode matrix

| Capability | B (LAN) | C (remote tunnel) |
|---|---|---|
| Discovery | active | not active; use stored `PeerDescriptor` |
| Pairing | QR/proximity in person | requires prior LAN pairing |
| SecureChannel | direct TCP/QUIC | TunnelCoordinator (hole-punch → relay) |
| Inference / sync / handoff | yes | yes |
| Takeover streaming | full frame rate | relay-constrained |
| File transfer | yes | relay-constrained throughput |
| Audio | later | later |

## 5. Deferred decisions (in order)
1. **LAN discovery:** mDNS/DNS-SD (Bonjour) primary; UDP-broadcast fallback for enterprise Wi-Fi that suppresses multicast.
2. **NAT traversal / tunnel:** WireGuard (`boringtun` userspace, production-stable on iOS/macOS) + self-hostable (headscale-compatible) coordinator for mode C; QUIC direct for mode B. (`libp2p` QUIC hole-punching has matured as an alternative.)
3. **Relay fallback:** `coturn`-compatible self-hostable relay; Cloudflare Tunnel as an opt-in escape hatch. Relay is untrusted (ciphertext only).
4. **Screen-streaming codec:** H.265 via VideoToolbox (sub-30 ms encode on M-series) as default; AV1 as quality mode (VideoToolbox AV1 since macOS 14). Wrap in **custom QUIC framing, not full WebRTC**, to keep the stack thin.
5. **P2P transport/handshake:** **QUIC** (TLS 1.3 built in, multiplexed streams, **connection migration** survives LTE↔Wi-Fi handoff) with a **Noise_XX** handshake over the pairing key (replaces the TLS cert ceremony).

## 6. Top risks
- **Takeover latency budget** — target <100 ms glass-to-glass LAN, <200 ms relay; profile encode + QUIC + decode on real A-/M-series hardware before codec lock-in.
- **NAT/relay reliability** — symmetric/carrier-grade NAT defeats hole-punching ~15–20%; relay fallback must be seamless + automatic.
- **Kill-switch correctness** — must ride the `CONTROL` channel with priority scheduling so it fires even if VIDEO/INPUT are saturated.
- **Agent/manual hybrid safety** — `agentGenerated` is advisory; the computer-use safety domain enforces its own gate.
- **Session auto-expiry** — expired sessions not resumable; reconnect needs a fresh `SessionToken` signed by the pairing key (no replay).
- **Phone battery during streaming** — continuous H.265 decode + QUIC receive is heavy; adaptive frame rate (drop to 10 fps when idle, ramp on touch) as a first-class `CONTROL` signal.
- **Multi-Mac arbitration** — PeerRegistry exposes load hints; tie-breaking (latency vs load) is Compute's call, not silent here.

## 7. Phased build
- **MVP (modes A+B):** mDNS discovery, PeerRegistry, PairingManager (QR), TrustStore; SecureChannel over direct TCP/QUIC; `INFERENCE`+`CONTROL` only; SessionManager (idle timeout + kill-switch); Compute integration via `PeerRegistrySnapshot`.
- **v1 (mode C):** TunnelCoordinator (WireGuard + coordinator); RelayClient (coturn) fallback; `SYNC`+`APPROVAL` channels; handoff reconnection (seqNo-based resume).
- **v1.1 (takeover):** ScreenStreamPipeline (VideoToolbox H.265 + QUIC framing); InputControlChannel; `FILE_XFR`; ApprovalBroker with previews; Mac "phone is controlling" indicator; adaptive frame rate.
- **Later:** `AUDIO` channel; AV1 quality mode; multi-Mac load-balanced routing; self-hosted relay analytics.

### Illustrative module layout
`connectivity/{MultiplexedBus, PairingManager, TunnelCoordinator, ScreenStreamPipeline, SessionManager}`
