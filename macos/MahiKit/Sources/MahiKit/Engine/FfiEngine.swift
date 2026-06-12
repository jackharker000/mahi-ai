// NOTE: `import Mahi` below is the UniFFI-generated Swift module for the Rust core
// (`crates/mahi-ffi`). It does not live in this repository as source — it is produced
// by the build script `macos/scripts/build-xcframework.sh`, which compiles the Rust
// crate into `MahiKit/Artifacts/MahiFFI.xcframework` and emits the generated bindings
// into `MahiKit/Sources/Mahi/Mahi.swift`. Until that script has run, this whole file
// compiles to nothing (`canImport(Mahi)` is false) and the app runs on
// `PreviewMockEngine`.
//
// The names used here (`MahiEngine`, `MahiEngineBuilder`, `TurnHandle.pollBatch`,
// `cancel`, `ComputerController`, `AgentEventFfi`) are normative per
// docs/backend/phase-0/03-engine-facade.md. This file is the single place to adjust
// if the generated spellings drift.
//
// NOTE: the shipped `mahi-ffi` exposes a *buffered* facade (`MahiEngineHandle`); this
// adapter targets the richer *streaming* FFI (TurnHandle/pollBatch + computer-use
// callback injection) which is the next FFI iteration. It is guarded on a module that
// does not exist yet, so it stays dormant and the app always builds on
// `PreviewMockEngine`.

#if canImport(MahiStreamingFFI)

import Foundation
import Mahi

// MARK: - Engine adapter

/// `MahiEngineProtocol` implementation backed by the real Rust core.
public final class FfiEngine: MahiEngineProtocol, @unchecked Sendable {
    private let inner: MahiEngine
    private let controllerAdapter: FfiComputerControllerAdapter

    public init(
        configuration: EngineConfiguration,
        computerController: any ComputerControlling
    ) throws {
        // Keep a strong reference: UniFFI callback interfaces are held by the Rust
        // side, but the adapter must outlive every turn regardless.
        controllerAdapter = FfiComputerControllerAdapter(wrapping: computerController)

        var builder = MahiEngineBuilder()
            .dataPath(path: configuration.dataDirectory.path)
            .deviceId(id: configuration.deviceID.uuidString)
            .computerController(controller: controllerAdapter)
        if let key = configuration.hostedAPIKey, !key.isEmpty {
            builder = builder.hostedProvider(
                apiKey: key,
                modelId: configuration.hostedModelID
            )
        }
        if let modelID = configuration.onDeviceModelID {
            builder = builder.onDeviceModel(modelId: modelID)
        }
        builder = builder.pin(mode: configuration.pinnedMode.map(Self.ffiMode(from:)))
        inner = try builder.build()
    }

    public func createConversation(mode: ComputeMode) async throws -> UUID {
        let raw = try await inner.createConversation(mode: Self.ffiMode(from: mode))
        return try Self.uuid(from: raw)
    }

    public func listConversations(limit: Int) async throws -> [Conversation] {
        let records = try await inner.listConversations(limit: UInt32(clamping: limit))
        return try records.map(Self.conversation(from:))
    }

    public func history(conversationID: UUID) async throws -> [Message] {
        let records = try await inner.history(conversationId: conversationID.uuidString)
        return try records.map(Self.message(from:))
    }

    public func runTurn(
        conversationID: UUID, userText: String
    ) async throws -> any TurnHandleProtocol {
        let handle = try await inner.runTurn(
            conversationId: conversationID.uuidString,
            userText: userText
        )
        return FfiTurnHandle(inner: handle)
    }

    public func resolveApproval(approvalID: UUID, approved: Bool) async throws {
        try await inner.resolveApproval(
            approvalId: approvalID.uuidString,
            approved: approved
        )
    }
}

// MARK: - Turn handle adapter

/// Wraps the FFI `TurnHandle` (`poll_batch`/`cancel`) behind `TurnHandleProtocol`.
final class FfiTurnHandle: TurnHandleProtocol, @unchecked Sendable {
    private let inner: TurnHandle

    init(inner: TurnHandle) {
        self.inner = inner
    }

    func pollBatch(maxEvents: UInt32) async throws -> [AgentEvent] {
        let batch = try await inner.pollBatch(max: maxEvents)
        return try batch.map(FfiEngine.agentEvent(from:))
    }

    func cancel() {
        inner.cancel()
    }
}

// MARK: - Computer-use callback adapter

/// Bridges the FFI-exported `ComputerController` callback interface to the
/// Swift-native `ComputerControlling` implementation (`MahiComputerController`).
final class FfiComputerControllerAdapter: ComputerController, @unchecked Sendable {
    private let wrapped: any ComputerControlling

    init(wrapping wrapped: any ComputerControlling) {
        self.wrapped = wrapped
    }

    func screenshot() async throws -> ScreenshotFfi {
        let shot = try await wrapped.screenshot()
        return ScreenshotFfi(
            pngData: shot.pngData,
            width: UInt32(clamping: shot.pixelWidth),
            height: UInt32(clamping: shot.pixelHeight),
            scale: shot.scale
        )
    }

    func describeScreen() async throws -> [ScreenElementFfi] {
        let elements = try await wrapped.describeScreen()
        return elements.map { element in
            ScreenElementFfi(
                elementId: element.id,
                role: element.role,
                title: element.title,
                value: element.value,
                x: element.frame.origin.x,
                y: element.frame.origin.y,
                width: element.frame.size.width,
                height: element.frame.size.height,
                actionable: element.isActionable
            )
        }
    }

    func click(x: Double, y: Double, button: MouseButtonFfi, clickCount: UInt32) async throws {
        try await wrapped.click(
            at: CGPoint(x: x, y: y),
            button: MouseButton(ffi: button),
            clickCount: Int(clickCount)
        )
    }

    func typeText(text: String) async throws {
        try await wrapped.typeText(text)
    }

    func pressKey(combo: String) async throws {
        try await wrapped.pressKey(KeyCombo(parsing: combo))
    }

    func scroll(x: Double, y: Double, deltaX: Double, deltaY: Double) async throws {
        try await wrapped.scroll(
            at: CGPoint(x: x, y: y),
            deltaX: deltaX,
            deltaY: deltaY
        )
    }

    func drag(fromX: Double, fromY: Double, toX: Double, toY: Double) async throws {
        try await wrapped.drag(
            from: CGPoint(x: fromX, y: fromY),
            to: CGPoint(x: toX, y: toY)
        )
    }
}

private extension MouseButton {
    init(ffi: MouseButtonFfi) {
        switch ffi {
        case .left: self = .left
        case .right: self = .right
        case .middle: self = .middle
        }
    }
}

// MARK: - Type mapping

extension FfiEngine {
    static func ffiMode(from mode: ComputeMode) -> Mahi.ComputeMode {
        switch mode {
        case .onDevice: return .onDevice
        case .macLan: return .macLan
        case .macRemote: return .macRemote
        case .hosted: return .hosted
        }
    }

    static func mode(from ffi: Mahi.ComputeMode) -> ComputeMode {
        switch ffi {
        case .onDevice: return .onDevice
        case .macLan: return .macLan
        case .macRemote: return .macRemote
        case .hosted: return .hosted
        }
    }

    static func uuid(from raw: String) throws -> UUID {
        guard let id = UUID(uuidString: raw) else {
            throw EngineError.invalidIdentifier(raw)
        }
        return id
    }

    static func conversation(from ffi: ConversationFfi) throws -> Conversation {
        Conversation(
            id: try uuid(from: ffi.id),
            title: ffi.title,
            createdAt: Date(timeIntervalSince1970: TimeInterval(ffi.createdAtEpochMs) / 1000),
            updatedAt: Date(timeIntervalSince1970: TimeInterval(ffi.updatedAtEpochMs) / 1000),
            modeAtCreation: mode(from: ffi.modeAtCreation)
        )
    }

    static func message(from ffi: MessageFfi) throws -> Message {
        Message(
            id: try uuid(from: ffi.id),
            conversationID: try uuid(from: ffi.conversationId),
            role: role(from: ffi.role),
            content: ffi.content.map(contentBlock(from:)),
            modelID: ffi.modelId,
            mode: mode(from: ffi.mode),
            createdAt: Date(timeIntervalSince1970: TimeInterval(ffi.createdAtEpochMs) / 1000),
            sequenceNum: ffi.sequenceNum
        )
    }

    static func role(from ffi: MessageRoleFfi) -> MessageRole {
        switch ffi {
        case .user: return .user
        case .assistant: return .assistant
        case .tool: return .tool
        case .system: return .system
        }
    }

    static func contentBlock(from ffi: ContentBlockFfi) -> ContentBlock {
        switch ffi {
        case .text(let text):
            return .text(text)
        case .toolCall(let callId, let toolId, let argsJson):
            return .toolCall(callID: callId, toolID: toolId, argsJSON: argsJson)
        case .toolResult(let callId, let outputJson):
            return .toolResult(callID: callId, outputJSON: outputJson)
        case .artifactRef(let artifactId):
            return .artifactRef(artifactID: UUID(uuidString: artifactId) ?? UUID())
        }
    }

    static func finishReason(from ffi: FinishReasonFfi) -> FinishReason {
        switch ffi {
        case .stop: return .stop
        case .toolCall: return .toolCall
        case .maxTokens: return .maxTokens
        case .cancelled: return .cancelled
        case .error: return .error
        }
    }

    static func destructiveLevel(from ffi: DestructiveLevelFfi) -> DestructiveLevel {
        switch ffi {
        case .low: return .low
        case .high: return .high
        case .critical: return .critical
        }
    }

    static func toolEvent(from ffi: ToolEventFfi) throws -> ToolEvent {
        switch ffi {
        case .chunk(let data):
            return .chunk(data: data)
        case .citation(let url, let title, let excerpt):
            return .citation(Citation(url: url, title: title, excerpt: excerpt))
        case .approvalRequired(let approvalId, let summary, let level):
            return .approvalRequired(
                approvalID: try uuid(from: approvalId),
                summary: summary,
                destructiveLevel: destructiveLevel(from: level)
            )
        case .result(let outputJson, let truncated):
            return .result(outputJSON: outputJson, truncated: truncated)
        case .error(let message, let retryable):
            return .error(message: message, retryable: retryable)
        case .cancelled:
            return .cancelled
        }
    }

    static func agentEvent(from ffi: AgentEventFfi) throws -> AgentEvent {
        switch ffi {
        case .turnStarted(let conversationId, let messageId, let mode):
            return .turnStarted(
                conversationID: try uuid(from: conversationId),
                messageID: try uuid(from: messageId),
                mode: Self.mode(from: mode)
            )
        case .textDelta(let text):
            return .textDelta(text)
        case .tool(let event):
            return .tool(try toolEvent(from: event))
        case .modeHandoff(let from, let to):
            return .modeHandoff(from: mode(from: from), to: mode(from: to))
        case .approvalRequired(let approvalId, let summary):
            return .approvalRequired(
                approvalID: try uuid(from: approvalId),
                summary: summary
            )
        case .turnFinished(let reason):
            return .turnFinished(reason: finishReason(from: reason))
        case .error(let message):
            return .error(message: message)
        }
    }
}

#endif
