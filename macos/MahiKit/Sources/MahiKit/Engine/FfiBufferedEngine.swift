// `import Mahi` is the UniFFI-generated module for the Rust core, produced by
// `scripts/build-xcframework.sh` (which installs `Sources/Mahi/Mahi.swift` +
// `Artifacts/MahiFFI.xcframework`). When those artifacts are absent this file
// compiles to nothing and the app runs on `PreviewMockEngine`.
//
// This adapter wraps the Phase-0 *buffered* FFI (`MahiEngineHandle`): a turn
// returns the assistant's full reply in one shot, which we surface through
// `TurnHandleProtocol` as a single text event so the UI code is identical to
// the future streaming path.

#if canImport(Mahi)

import Foundation
import Mahi

/// `MahiEngineProtocol` backed by the real Rust core via the buffered FFI.
public final class FfiBufferedEngine: MahiEngineProtocol, @unchecked Sendable {
    private let handle: MahiEngineHandle

    /// Open (or create) the encrypted store under `dataDirectory`.
    public init(dataDirectory: URL) throws {
        try FileManager.default.createDirectory(
            at: dataDirectory, withIntermediateDirectories: true
        )
        handle = try MahiEngineHandle.withStore(dataDir: dataDirectory.path)
    }

    /// Ephemeral in-memory engine (used by tests).
    public init() throws {
        handle = try MahiEngineHandle.inMemory()
    }

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
                mode: .onDevice,
                sequenceNum: record.sequenceNum
            )
        }
    }

    public func runTurn(
        conversationID: UUID, userText: String
    ) async throws -> any TurnHandleProtocol {
        BufferedTurnHandle(handle: handle, conversationID: conversationID, userText: userText)
    }

    public func resolveApproval(approvalID: UUID, approved: Bool) async throws {
        // The buffered FFI has no approval channel yet; on-device turns never
        // park on approvals (no tool-calling model), so this is a no-op.
    }

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
}

/// Runs the buffered `send` once and replays it as a tiny event stream.
private final class BufferedTurnHandle: TurnHandleProtocol, @unchecked Sendable {
    private let handle: MahiEngineHandle
    private let conversationID: UUID
    private let userText: String
    private let delivered = NSLock()
    private var finished = false

    init(handle: MahiEngineHandle, conversationID: UUID, userText: String) {
        self.handle = handle
        self.conversationID = conversationID
        self.userText = userText
    }

    func pollBatch(maxEvents: UInt32) async throws -> [AgentEvent] {
        delivered.lock()
        let alreadyDone = finished
        finished = true
        delivered.unlock()
        if alreadyDone { return [] }

        let handle = self.handle
        let convID = conversationID
        let text = userText
        let reply = try await Task.detached {
            try handle.send(conversationId: convID.uuidString, text: text)
        }.value
        return [
            .turnStarted(conversationID: convID, messageID: UUID(), mode: .onDevice),
            .textDelta(reply.text),
            .turnFinished(reason: .stop),
        ]
    }

    func cancel() {
        // Buffered turns are short; cancellation lands with the streaming FFI.
    }
}

#endif
