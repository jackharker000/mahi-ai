// `import Mahi` is the UniFFI-generated module for the Rust core, produced by
// `scripts/build-xcframework.sh` (which installs `Sources/Mahi/Mahi.swift` +
// `Artifacts/MahiFFI.xcframework`). When those artifacts are absent this file
// compiles to nothing and the app runs on `PreviewMockEngine`.
//
// This adapter wraps the real streaming FFI (`MahiEngineHandle` + `TurnHandle`):
// turns stream token-by-token with tool events and approvals, and the engine
// also exposes local-model management, hosted config, and subagents. The Rust
// `*Ffi` value types are mapped to MahiKit's domain types here, so the rest of
// the app never imports `Mahi`.

#if canImport(Mahi)

import Foundation
import Mahi

/// `MahiEngineProtocol` backed by the real Rust core via the streaming FFI.
public final class FfiBufferedEngine: MahiEngineProtocol, @unchecked Sendable {
    private let handle: MahiEngineHandle

    /// Open (or create) the encrypted store under `dataDirectory`, wiring the
    /// agent's computer-use tools to the real Mac controller. `@MainActor`
    /// because the controller is constructed on the main actor.
    @MainActor
    public init(dataDirectory: URL) throws {
        try FileManager.default.createDirectory(
            at: dataDirectory, withIntermediateDirectories: true
        )
        let host = ComputerUseHostBridge()
        handle = try MahiEngineHandle.withStoreAndComputer(
            dataDir: dataDirectory.path, host: host
        )
    }

    /// Ephemeral in-memory engine (used by tests).
    public init() throws {
        handle = try MahiEngineHandle.inMemory()
    }

    // MARK: Conversations & turns

    public func createConversation(mode: ComputeMode) async throws -> UUID {
        let handle = self.handle
        let raw = try await Task.detached { try handle.createConversation() }.value
        guard let id = UUID(uuidString: raw) else { throw EngineError.invalidIdentifier(raw) }
        return id
    }

    public func listConversations(limit: Int) async throws -> [Conversation] {
        let handle = self.handle
        let summaries = try await Task.detached { try handle.listConversations() }.value
        return summaries.prefix(limit).compactMap { summary in
            guard let id = UUID(uuidString: summary.id) else { return nil }
            return Conversation(
                id: id,
                title: summary.title,
                modeAtCreation: Self.mode(from: summary.mode)
            )
        }
    }

    public func history(conversationID: UUID) async throws -> [Message] {
        let handle = self.handle
        let idString = conversationID.uuidString
        let records = try await Task.detached { try handle.history(conversationId: idString) }.value
        return records.map { record in
            Message(
                id: UUID(uuidString: record.id) ?? UUID(),
                conversationID: conversationID,
                role: Self.role(from: record.role),
                content: [.text(record.text)],
                mode: .macLan,
                sequenceNum: record.sequenceNum
            )
        }
    }

    public func runTurn(
        conversationID: UUID, userText: String
    ) async throws -> any TurnHandleProtocol {
        let handle = self.handle
        let convID = conversationID.uuidString
        let inner = try await Task.detached {
            try handle.startTurn(conversationId: convID, text: userText)
        }.value
        return FfiTurnHandle(inner)
    }

    public func resolveApproval(approvalID: UUID, approved: Bool) async throws {
        let handle = self.handle
        let id = approvalID.uuidString
        try await Task.detached {
            try handle.resolveApproval(approvalId: id, approved: approved)
        }.value
    }

    // MARK: Local model management

    public func modelCatalog() async throws -> [CatalogModel] {
        let handle = self.handle
        let ffi = try await Task.detached { try handle.modelCatalog() }.value
        return ffi.map(Self.catalogModel(from:))
    }

    public func startDownload(modelID: String) async throws {
        let handle = self.handle
        try await Task.detached { try handle.startDownload(modelId: modelID) }.value
    }

    public func cancelDownload(modelID: String) async throws {
        let handle = self.handle
        try await Task.detached { try handle.cancelDownload(modelId: modelID) }.value
    }

    public func deleteModel(modelID: String) async throws {
        let handle = self.handle
        try await Task.detached { try handle.deleteModel(modelId: modelID) }.value
    }

    public func activateModel(modelID: String) async throws {
        let handle = self.handle
        try await Task.detached { try handle.activateModel(modelId: modelID) }.value
    }

    public func runtimeStatus() async throws -> RuntimeStatus {
        let handle = self.handle
        let ffi = await Task.detached { handle.runtimeStatus() }.value
        return Self.runtimeStatus(from: ffi)
    }

    // MARK: Hosted config & subagents

    public func setHostedConfig(_ config: HostedConfig?) async throws {
        let handle = self.handle
        let ffi = config.map(Self.hostedConfigFfi(from:))
        try await Task.detached { try handle.setHostedConfig(config: ffi) }.value
    }

    public func spawnSubagents(goals: [String]) async throws -> [String] {
        let handle = self.handle
        return try await Task.detached { try handle.spawnSubagents(goals: goals) }.value
    }

    // MARK: - Type mapping

    private static func mode(from raw: String) -> ComputeMode {
        switch raw.lowercased() {
        case "maclan": return .macLan
        case "macremote": return .macRemote
        case "hosted": return .hosted
        default: return .onDevice
        }
    }

    private static func role(from raw: String) -> MessageRole {
        MessageRole(rawValue: raw) ?? .assistant
    }

    private static func catalogModel(from ffi: CatalogModelFfi) -> CatalogModel {
        CatalogModel(
            id: ffi.id,
            displayName: ffi.displayName,
            family: ffi.family,
            sizeBytes: Int64(ffi.sizeBytes),
            quantization: ffi.quantization,
            contextWindow: Int(ffi.contextWindow),
            toolCalling: ffi.toolCalling,
            description: ffi.description,
            state: modelState(from: ffi.state)
        )
    }

    private static func modelState(from ffi: ModelStateFfi) -> ModelState {
        switch ffi {
        case .notInstalled:
            return .notInstalled
        case let .downloading(progress, bytesDownloaded):
            return .downloading(progress: progress, bytesDownloaded: Int64(bytesDownloaded))
        case .installed:
            return .installed
        case .active:
            return .active
        }
    }

    private static func runtimeStatus(from ffi: RuntimeStatusFfi) -> RuntimeStatus {
        switch ffi {
        case .noModel:
            return .noModel
        case .preparingRuntime:
            return .preparingRuntime
        case let .starting(modelId):
            return .starting(modelID: modelId)
        case let .running(modelId):
            return .running(modelID: modelId)
        case let .failed(message):
            return .failed(message: message)
        }
    }

    private static func hostedConfigFfi(from config: HostedConfig) -> HostedConfigFfi {
        let provider: String
        switch config.provider {
        case .anthropic: provider = "anthropic"
        case .openAICompatible: provider = "openai"
        }
        return HostedConfigFfi(
            provider: provider,
            apiKey: config.apiKey,
            model: config.model,
            baseUrl: config.baseURL
        )
    }
}

/// Bridges the FFI `TurnHandle` (poll/cancel) to MahiKit's `TurnHandleProtocol`,
/// mapping each `AgentEventFfi` to a domain `AgentEvent`.
private final class FfiTurnHandle: TurnHandleProtocol, @unchecked Sendable {
    private let inner: Mahi.TurnHandle

    init(_ inner: Mahi.TurnHandle) {
        self.inner = inner
    }

    func pollBatch(maxEvents: UInt32) async throws -> [AgentEvent] {
        let inner = self.inner
        // `pollBatch` blocks in Rust until events arrive (or the turn ends), so
        // run it off the cooperative pool.
        let ffiEvents = await Task.detached { inner.pollBatch(maxEvents: maxEvents) }.value
        return ffiEvents.compactMap(Self.event(from:))
    }

    func cancel() {
        inner.cancel()
    }

    private static func event(from ffi: AgentEventFfi) -> AgentEvent? {
        switch ffi {
        case let .turnStarted(conversationId, messageId):
            return .turnStarted(
                conversationID: UUID(uuidString: conversationId) ?? UUID(),
                messageID: UUID(uuidString: messageId) ?? UUID(),
                mode: .macLan
            )
        case let .textDelta(text):
            return .textDelta(text)
        case let .toolProgress(text):
            return .tool(.chunk(data: text))
        case let .toolResult(output):
            return .tool(.result(outputJSON: output, truncated: false))
        case let .toolError(message):
            return .tool(.error(message: message, retryable: false))
        case let .approvalRequired(approvalId, summary):
            return .approvalRequired(
                approvalID: UUID(uuidString: approvalId) ?? UUID(),
                summary: summary
            )
        case let .turnFinished(reason):
            return .turnFinished(reason: finishReason(from: reason))
        case let .error(message):
            return .error(message: message)
        }
    }

    private static func finishReason(from raw: String) -> FinishReason {
        switch raw.lowercased() {
        case "toolcall": return .toolCall
        case "maxtokens": return .maxTokens
        case "cancelled": return .cancelled
        case "error": return .error
        default: return .stop
        }
    }
}

#endif
