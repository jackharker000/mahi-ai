import Foundation
import MahiKit
import SwiftUI

/// The app-facing view model. Owns the engine seam and drives one chat
/// conversation: loading history, sending turns, streaming assistant output,
/// and surfacing approval prompts.
@MainActor
public final class AppModel: ObservableObject {
    @Published public private(set) var conversations: [Conversation] = []
    @Published public private(set) var messages: [Message] = []
    @Published public private(set) var streamingText: String = ""
    @Published public private(set) var isStreaming = false
    @Published public private(set) var activeMode: ComputeMode = .onDevice
    @Published public var selectedID: UUID?
    @Published public var composer: String = ""
    @Published public var pendingApproval: PendingApproval?
    @Published public var errorText: String?

    /// A gated action awaiting the user's decision.
    public struct PendingApproval: Identifiable {
        public let id: UUID
        public let summary: String
        public let preview: ApprovalPreview
    }

    private let engine: any MahiEngineProtocol
    private var turnTask: Task<Void, Never>?

    public init(engine: any MahiEngineProtocol = EngineFactory.makeDefault()) {
        self.engine = engine
    }

    public func bootstrap() async {
        await reloadConversations()
        if let first = conversations.first {
            await select(first.id)
        } else {
            await newConversation()
        }
    }

    public func reloadConversations() async {
        do { conversations = try await engine.listConversations(limit: 100) } catch {
            errorText = error.localizedDescription
        }
    }

    public func newConversation() async {
        do {
            let id = try await engine.createConversation(mode: .onDevice)
            await reloadConversations()
            await select(id)
        } catch {
            errorText = error.localizedDescription
        }
    }

    public func select(_ id: UUID) async {
        selectedID = id
        do { messages = try await engine.history(conversationID: id) } catch {
            errorText = error.localizedDescription
        }
    }

    public func send() {
        let text = composer.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !text.isEmpty, !isStreaming, let convID = selectedID else { return }
        composer = ""
        messages.append(
            Message(
                conversationID: convID,
                role: .user,
                content: [.text(text)],
                mode: activeMode,
                sequenceNum: Int64(messages.count)
            )
        )
        isStreaming = true
        streamingText = ""
        turnTask = Task { await runTurn(convID: convID, text: text) }
    }

    public func cancel() {
        turnTask?.cancel()
    }

    public func resolveApproval(_ decision: ApprovalDecision) {
        guard let pending = pendingApproval else { return }
        pendingApproval = nil
        Task {
            do {
                try await engine.resolveApproval(approvalID: pending.id, approved: decision.approves)
            } catch {
                errorText = error.localizedDescription
            }
        }
    }

    private func runTurn(convID: UUID, text: String) async {
        do {
            let handle = try await engine.runTurn(conversationID: convID, userText: text)
            for try await event in handle.events() {
                switch event {
                case .turnStarted(_, _, let mode):
                    activeMode = mode
                case .textDelta(let delta):
                    streamingText += delta
                case .modeHandoff(_, let to):
                    activeMode = to
                case .approvalRequired(let id, let summary):
                    pendingApproval = PendingApproval(
                        id: id,
                        summary: summary,
                        preview: ApprovalPreview.parse(summary: summary)
                    )
                case .tool(let toolEvent):
                    appendToolNote(toolEvent, convID: convID)
                case .turnFinished:
                    flushAssistant(convID: convID)
                case .error(let message):
                    errorText = message
                }
            }
        } catch is CancellationError {
            // expected on user cancel
        } catch {
            errorText = error.localizedDescription
        }
        flushAssistant(convID: convID)
        isStreaming = false
        await reloadConversations()
    }

    private func flushAssistant(convID: UUID) {
        guard !streamingText.isEmpty else { return }
        messages.append(
            Message(
                conversationID: convID,
                role: .assistant,
                content: [.text(streamingText)],
                mode: activeMode,
                sequenceNum: Int64(messages.count)
            )
        )
        streamingText = ""
    }

    private func appendToolNote(_ event: ToolEvent, convID: UUID) {
        let note: String?
        switch event {
        case .result(let outputJSON, _): note = "tool result: \(outputJSON)"
        case .error(let message, _): note = "tool error: \(message)"
        case .citation(let citation): note = "source: \(citation.url)"
        default: note = nil
        }
        guard let note else { return }
        messages.append(
            Message(
                conversationID: convID,
                role: .tool,
                content: [.text(note)],
                mode: activeMode,
                sequenceNum: Int64(messages.count)
            )
        )
    }
}
