# Domain ③ — Compute, Serving & Mode Selection

> Detailed design for how inference compute is provisioned and selected across modes A–D.
> See [`../README.md`](../README.md) for the synthesis. Module/file paths are **illustrative**
> pending the shared-core language decision.

## 1. Overview & responsibilities

Owns the full lifecycle of inference compute: provisioned, selected, routed, and exposed to the
Agent Core as a single uniform surface. Does not own networking (Connectivity provides channels +
server lists) or agent planning/memory/tools (Core). Contract: given a request with capability
requirements, return a stream of tokens from the best-available source, with an observable mode
label attached.

- Run/manage inference engines on Mac (daemon) and iPhone (on-device)
- Proxy requests to hosted providers (mode D)
- Select among A/B/C/D dynamically and on user pin
- Publish capability descriptors consumed by Core and the Data/Sync registry

## 2. Component breakdown

- **InferenceRouter** — central decision engine; selects mode per request; owns pin state; single entry point from Core.
- **MacInferenceServer** — Mac daemon wrapping the serving stack; OpenAI-compatible endpoint over LAN + tunnel; queue, concurrency, thermal/power guards.
- **OnDeviceRuntime** — iOS process; loads a small model (MLX Swift / llama.cpp / CoreML / Apple Foundation Models); tokenization, streaming, vision preprocessing, dictation.
- **ModelDownloadManager** — iOS catalog; download/delete; storage accounting; per-model capability labels.
- **HostedProviderGateway** — translates uniform request to provider wire format; retry, key rotation, cost metering; same streaming interface.
- **ModeStateStore** — persists active mode, pin, per-device overrides (synced via Data).
- **CapabilityRegistry publisher** — packages each mode's descriptors into a snapshot; writes to Data/Sync on change.
- **WakeController** — Magic Packet / provider wake signal; tracks wake-pending + retry budget.
- **ThermalPowerMonitor** — Mac: IOKit GPU temp/power/memory pressure; iOS: `ProcessInfo.thermalState` + battery; emits throttle/suspend.

## 3. Key interfaces & data models

```
// Uniform inference interface (Agent Core facing)
interface InferenceProvider {
  descriptor(): ModelDescriptor
  generate(req: InferenceRequest): AsyncStream<InferenceChunk>
  canHandle(req: InferenceRequest): CanHandleResult
}
InferenceRequest { messages: [Message] /* text+image */, tools?: [ToolSpec], maxTokens,
  temperature, streamingHint, requiredCaps: CapabilitySet /* {vision, toolCalling, minContextWindow} */ }
InferenceChunk { delta, toolCallDelta?, finishReason?, activeMode: ComputeMode, latencyHint? }  // every chunk echoes activeMode
CanHandleResult { capable, missingCaps: CapabilitySet, escalationHint?: ComputeMode }

ModelDescriptor { id, displayName, contextWindow, capabilities: CapabilitySet,
  limitations: [LimitationLabel] /* "noLocalRepo","maxImages:1" */, sizeBytes?, quantization?,
  source: onDevice|macLocal|macRemote|hosted(provider), perfProfile: { ttftMs, tokPerSec } }

ServerDescriptor { id, displayName, reachability: [lan|tunnel],
  health: available|busy|thermalThrottle|sleeping|unreachable, loadFactor: 0..1,
  powerSource: ac|battery, availableModels, gpuVRAM, lastHeartbeat }

// Mode selection
ModeSelectionInput { reachableServers, onDeviceState, networkQuality: none|lan|wwan(q),
  userPin?, request, costPreference: free|lowCost|any, powerPreference: lowPower|normal|performance }
ModeSelectionOutput { selectedMode, selectedProvider, rationale: [SelectionReason], degradedFrom? }
enum ComputeMode { onDevice, macLAN, macRemote, hosted }

// Wake-on-demand
interface WakeController { requestWake(server): async WakeResult }   // returns when health==available OR throws WakeTimeout (≤30s)
// Needed FROM Connectivity: reachableServers() -> [ServerDescriptor]; secureChannel(to:); sendWakeSignal(to:)
// Published TO Data: DeviceCapabilitySnapshot { deviceId, timestamp, modes: {ComputeMode: ModeCapability} }
```

## 4. Mode matrix & selection logic

| Dimension | A on-device | B Mac LAN | C Mac remote | D hosted |
|---|---|---|---|---|
| Model ceiling | ~4–5 GB (8GB phone) | unlimited | same as B | provider |
| Context | 4k–8k | 32k–128k+ | same as B | 8k–200k+ |
| Tool calling | via Apple FM / capable small model | full | full | partial |
| Local repo | no | full FS | full (tunneled) | no |
| Network | none (airplane-safe) | LAN | WAN tunnel | WAN |
| Latency (TTFT) | 25–60 ms | 50–200 ms | 200–800 ms | 300–2000 ms |
| Cost | free | free | free + tunnel infra | $/token |

```
auto-selection (preferred → fallback):
  1. macLAN   — reachable on LAN, health ok
  2. macRemote — reachable via tunnel, health ok
  3. hosted   — network available, cost pref != .free
  4. onDevice — always available; last resort OR (cost==.free AND no Mac)
override triggers:
  - userPin set      → force it; if unavailable, EMIT pin-violation (never silent fallback)
  - requiredCaps ⊄ mode → skip mode
  - Mac thermalThrottle → demote for new requests; drain in-flight
  - battery <20% + no AC → prefer hosted over onDevice for heavy tasks
  - networkQuality==none → onDevice only (unless pin==hosted → offline warning)
```
*(See README §6 — the literal spec order "D→C→B→A" vs. this local-first preference needs your confirmation.)*
`ModeStateStore` holds `activeMode + pinned`; every chunk echoes `activeMode`; UI subscribes.

## 5. Deferred decisions
- **On-device runtime:** prototype Apple Foundation Models (zero friction) → add MLX Swift (best throughput, ~61 tok/s on a 2B) → CoreML for battery/background → llama.cpp for GGUF breadth.
- **Apple Foundation Models as free tier — YES:** register as `source: onDevice, id: apple-foundation-models-system` with explicit limitation labels (`maxContextWindow: 4096`, no complex `codeGen`). Fall through to a downloaded model or hosted if unavailable (older device / no Apple Intelligence).
- **Mac serving stack:** Ollama wrapper (OpenAI-compatible, launchd daemon) for MVP → pluggable `MacServingBackend`, add MLX-LM for high-throughput Mac minis → thermal-aware backend selection.
- **Hosted abstraction:** OpenAI-compatible wire format as canonical (Together/Fireworks/Groq/Mistral need zero translation); thin adapters for Anthropic/Google; a LiteLLM-style proxy is a valid gateway internal.

## 6. Top risks
- **Thermal/battery on Mac** — sustained load throttles Apple Silicon; gate *new* requests, don't abort in-flight; surface `thermalThrottle`.
- **Cold-start / wake latency** — WoL on a sleeping Mac mini is 10–25 s (unacceptable interactively). Default mitigation: never sleep while on AC (surfaced in onboarding); pre-wake heuristic on app open.
- **Offline-provability (mode A)** — zero network at inference time. Apple FM may emit telemetry out of our control → reserve the "verified offline" badge for downloaded models only.
- **Capability mislabeling** — validate caps via a benchmark at download/load (`capabilityVerified` flag), not metadata, or the core sends tool schemas a small model can't honor.
- **Multi-Mac load balancing** — stale heartbeats mis-route; Connectivity defines heartbeat-freshness TTL; stale ⇒ `unreachable`.
- **Pin + degradation UX** — a pinned-but-unavailable mode must produce an explicit pin-violation event, never silent fallback.

## 7. Phased build
- **MVP:** OnDeviceRuntime (Apple FM only) + HostedProviderGateway (one provider); 2-mode router (A/D); ModelDescriptor + CapabilitySet; active-mode shown (no pin); local ModeStateStore.
- **v1:** MacInferenceServer (Ollama + launchd); WakeController (Magic Packet); ThermalPowerMonitor (Mac + iOS); full 4-mode router policy; ModelDownloadManager + MLX Swift; user pin + pin-violation events; CapabilityRegistry publisher; multi-provider hosted + fallback.
- **Later:** multi-Mac load-aware routing; pluggable `MacServingBackend` (MLX-LM); CoreML/ANE path; scheduled tasks on Mac while phone off; capability-benchmark runner; thermal-aware backend switching; per-feature compute overrides.

### Illustrative module layout
`Compute/{InferenceRouter, InferenceProvider, MacInferenceServer, OnDeviceRuntime, HostedProviderGateway}`

### Sources (2026)
WWDC 2026 Foundation Models (provider routing); Apple Foundation Models docs; iPhone runtime
benchmarks (MLX vs llama.cpp vs LiteRT-LM vs CoreML); MLX-vs-llama.cpp context-window tradeoffs;
Apple-Silicon local-LLM concurrency/tail-latency; LLM API provider price/limit comparisons.
