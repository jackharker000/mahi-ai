# Domain ② — Tooling, Integrations & Coding Agent

> Detailed design for the capability/tool surface. See [`../README.md`](../README.md) for the
> synthesis. Module/file paths are **illustrative** pending the shared-core language decision.

## 1. Overview & responsibilities

Owns the entire tool surface the Agent Core invokes. It does not make model calls; it receives
structured invocations and returns structured results. Responsible for: executing built-in tools
in the correct compute context, brokering third-party connectors, managing the coding agent's
stateful job lifecycle, and enforcing computer-use capability contracts. Emits all required
permission-check requests and audit events but does **not** own those systems.

## 2. Component breakdown

- **ToolRegistry** — catalog of all tools/connectors; answers capability queries per compute mode; single source of truth for `describe()`.
- **BuiltinToolExecutor** — dispatches built-in invocations to the right sub-executor by mode.
- **WebToolSuite** — web search + fetch, citation extraction, result sanitization.
- **FileToolSuite** — read/write/search/organize for phone files (A/B/C/D) and Mac files (B/C only); path scoping.
- **SandboxedExecutor** — isolated process/container for shell + user code; no network unless granted.
- **DocumentGenerator** — docx/xlsx/pptx/pdf/markdown.
- **DataAnalysisSuite** — runs code against datasets; returns tables + chart artifacts.
- **ConnectorBroker** — discovers/installs/authenticates/proxies external connectors via a standard manifest; manages permission tiers + token lifecycle.
- **CodingAgent** — stateful, repo-aware loop: edits, test/lint cycles, git; owns the job model + background harness.
- **ComputerUseEngine** — screen capture, OCR/UI-element detection, input dispatch; per-app tiers + action log.
- **ActionLogger** — writes every invocation + result + approval decision to the audit bus (boundary with Permission/Audit domain).

## 3. Key interfaces & data models

```
// Uniform tool contract — every tool implements describe/invoke/cancel
ToolDescriptor { id, displayName, category: BuiltIn|Connector|CodingAgent|ComputerUse,
  availableInModes: Set<A|B|C|D>, requiredPermissions: PermissionKey[],
  inputSchema, outputSchema, requiresApproval: bool }   // hard flag for send/delete/execute

ToolInvocation { toolId, args, context: { sessionId, userId, computeMode, traceId }, stream: bool }
ToolResult { status: ok|error|cancelled|approval_required, output, citations: Citation[],
  approvalToken?, truncated: bool }
//  describe() -> ToolDescriptor ; invoke(ToolInvocation) -> AsyncIterator<ToolResultChunk> ; cancel(traceId)

// Connector manifest + permission tiers
ConnectorManifest { connectorId, version, displayName, iconUrl,
  transport: StdIO|HTTP|SSE, tools: ToolDescriptor[], authScheme: OAuth2|ApiKey|None,
  permissionTiers: { read[], write[], admin[] }, sandboxed: bool }
ConnectorInstance { instanceId, connectorId, userId, grantedTier: read|write|admin,
  tokenRef /* opaque ref into encrypted vault */, installedAt, lastUsed, enabled }

// Coding-agent job
CodingJob { jobId, repoPath, intent, status: queued|running|awaiting_approval|completed|failed,
  currentStep, pendingDiff?: UnifiedDiff, iterationCount, maxIterations /* hard cap */,
  outputLog: LogRef, createdAt, updatedAt, expiresAt, initiatingDevice }

// Computer-use capability + action
ComputerUseCapability { appId /* bundle id or "*" */, tier: ReadOnly|ClickOnly|Full,
  browserScope?: { urlPatterns[] } }
ComputerUseAction { actionId, type: screenshot|ocr|click|type|scroll|drag|shortcut|browser_navigate,
  target: { appId, elementDescription, coordinates? },
  requiresExplicitApproval /* login/payment */, executedAt, approved }

// Audit event (to Permission/Audit domain)
AuditEvent { eventId, traceId, sessionId, userId, toolId, args: RedactedArgs,
  permissionsChecked[], permissionGranted, approvalRequired, approvalGranted?,
  resultStatus, durationMs, timestamp }
```

## 4. Mode matrix

| Tool cluster | A | B | C | D |
|---|---|---|---|---|
| Web search/fetch | partial (cached) | full | full | full |
| Phone file tools | full | read-only bridge | read-only bridge | none |
| Mac file tools | none | full | full | none |
| Sandboxed shell/code | none | full | full | limited (cloud sandbox) |
| Document generation | basic (md) | full | full | full |
| Data analysis | none | full | full | partial |
| Connectors (read/write) | cached / queued | full | full | full |
| Coding agent | none | full | full | none |
| Computer-use | none | full | full (tunnel) | none |

`ToolRegistry.getAvailableTools(computeMode)` ensures the core never invokes an unavailable tool;
every descriptor carries `availableInModes`, and a `CapabilityReport` drives honest UI state.

## 5. Deferred decisions
- **Connector protocol:** MCP baseline; wrap custom connectors in an MCP-compatible shim.
- **Sandboxing:** macOS App Sandbox + seccomp first (ships fastest); Firecracker for cloud; WASI for doc-gen.
- **Document generation:** Pandoc + python-docx/openpyxl/python-pptx for MVP; LibreOffice headless for high fidelity.
- **OCR / UI detection:** Accessibility API (AXUIElement) primary; Apple Vision OCR fallback; CV model later.
- **Connector token vault:** platform Keychain for device-local; encrypted field for cross-device configs; **never sync raw tokens** (only `tokenRef`).

## 6. Top risks
- **Sandbox escape** (highest blast radius) → no network/host-FS by default, output caps, kill timeout.
- **Prompt-injection via tool results** → sanitization layer; structured envelopes separating data vs instructions; citation/content separation.
- **Connector token storage / revocation propagation** across devices.
- **Approval-wall bypass** in computer-use → reliable login/payment detection (URL patterns, AX roles, keyword list).
- **Long-running job orphaning** → process supervisor (launchd/XPC) + reboot recovery model.
- **Connector supply-chain** → manifest signing + review before any community catalog.

## 7. Phased build
- **MVP (B/C):** ToolRegistry + uniform contract; WebToolSuite; FileToolSuite (Mac); App-Sandbox shell; ActionLogger; 2 bundled MCP connectors (GitHub, Calendar); basic synchronous CodingJob.
- **v1:** mode-aware routing; phone file tools (A); background CodingJob harness w/ cross-device observability; ComputerUseEngine (AX + Vision); full connector lifecycle (tiers, revocation); DocumentGenerator; DataAnalysisSuite.
- **Later:** community connector catalog + signing; cloud sandbox (D); CV-model targeting; LibreOffice high-fidelity; connector marketplace + SDK.

### Illustrative module layout
`tool-layer/{ToolRegistry, contracts/ToolContract, connectors/ConnectorBroker, coding/CodingJob, audit/ActionLogger}`
