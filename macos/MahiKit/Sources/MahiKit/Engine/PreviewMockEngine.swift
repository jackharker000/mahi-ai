import Foundation

/// An in-memory, scripted `MahiEngineProtocol` used for SwiftUI previews, unit tests,
/// and running the app before the Rust core has been built
/// (`scripts/build-xcframework.sh`). It streams plausible turns — including tool
/// activity and an approval gate when the prompt looks action-like — with realistic
/// pacing so the streaming UI can be exercised end to end.
public final class PreviewMockEngine: MahiEngineProtocol, @unchecked Sendable {
    private let state: MockEngineState
    private let chunkDelay: Duration

    public init(seeded: Bool = false, chunkDelay: Duration = .milliseconds(24)) {
        self.state = MockEngineState()
        self.chunkDelay = chunkDelay
        if seeded {
            Task { await state.seedSampleData() }
        }
    }

    /// A pre-seeded engine for SwiftUI previews.
    public static func preview() -> PreviewMockEngine {
        PreviewMockEngine(seeded: true)
    }

    // MARK: MahiEngineProtocol

    public func createConversation(mode: ComputeMode) async throws -> UUID {
        await state.createConversation(mode: mode)
    }

    public func listConversations(limit: Int) async throws -> [Conversation] {
        await state.listConversations(limit: limit)
    }

    public func history(conversationID: UUID) async throws -> [Message] {
        await state.history(conversationID: conversationID)
    }

    public func runTurn(
        conversationID: UUID, userText: String
    ) async throws -> any TurnHandleProtocol {
        let buffer = MockEventBuffer()
        let handle = MockTurnHandle(buffer: buffer)
        let script = TurnScript.select(for: userText)
        let state = self.state
        let delay = self.chunkDelay

        handle.producer = Task {
            await state.appendUserMessage(conversationID: conversationID, text: userText)
            let messageID = UUID()
            let mode = await state.activeMode
            await buffer.push(.turnStarted(
                conversationID: conversationID, messageID: messageID, mode: mode
            ))

            var assistantText = ""
            do {
                for step in script.steps {
                    try Task.checkCancellation()
                    switch step {
                    case .text(let paragraph):
                        for chunk in Self.chunked(paragraph) {
                            try Task.checkCancellation()
                            try await Task.sleep(for: delay)
                            assistantText += chunk
                            await buffer.push(.textDelta(chunk))
                        }
                    case .tool(let event):
                        try await Task.sleep(for: .milliseconds(180))
                        await buffer.push(.tool(event))
                    case .approval(let summary, let level):
                        let approvalID = UUID()
                        await buffer.push(.approvalRequired(
                            approvalID: approvalID, summary: summary
                        ))
                        _ = level // carried in the summary for the mock
                        let approved = await state.waitForApproval(id: approvalID)
                        let outcome = approved
                            ? "\n\nApproved — done. "
                            : "\n\nUnderstood, I won't do that. "
                        assistantText += outcome
                        await buffer.push(.textDelta(outcome))
                        if !approved {
                            await finish(
                                buffer: buffer, state: state, conversationID: conversationID,
                                text: assistantText, mode: mode, reason: .stop
                            )
                            return
                        }
                    case .modeHandoff(let from, let to):
                        await state.setActiveMode(to)
                        await buffer.push(.modeHandoff(from: from, to: to))
                    }
                }
                await finish(
                    buffer: buffer, state: state, conversationID: conversationID,
                    text: assistantText, mode: mode, reason: .stop
                )
            } catch {
                await finish(
                    buffer: buffer, state: state, conversationID: conversationID,
                    text: assistantText, mode: mode, reason: .cancelled
                )
            }
        }
        return handle
    }

    public func resolveApproval(approvalID: UUID, approved: Bool) async throws {
        await state.resolveApproval(id: approvalID, approved: approved)
    }

    // MARK: Helpers

    private func finish(
        buffer: MockEventBuffer,
        state: MockEngineState,
        conversationID: UUID,
        text: String,
        mode: ComputeMode,
        reason: FinishReason
    ) async {
        await state.appendAssistantMessage(
            conversationID: conversationID, text: text, mode: mode
        )
        await buffer.push(.turnFinished(reason: reason))
        await buffer.finish()
    }

    /// Split text into word-sized chunks to mimic token streaming.
    private static func chunked(_ text: String) -> [String] {
        var chunks: [String] = []
        var current = ""
        for character in text {
            current.append(character)
            if character == " " || current.count >= 12 {
                chunks.append(current)
                current = ""
            }
        }
        if !current.isEmpty { chunks.append(current) }
        return chunks
    }
}

// MARK: - Turn handle

final class MockTurnHandle: TurnHandleProtocol, @unchecked Sendable {
    private let buffer: MockEventBuffer
    var producer: Task<Void, Never>?

    init(buffer: MockEventBuffer) {
        self.buffer = buffer
    }

    func pollBatch(maxEvents: UInt32) async throws -> [AgentEvent] {
        await buffer.nextBatch(max: Int(maxEvents))
    }

    func cancel() {
        producer?.cancel()
        let buffer = self.buffer
        Task {
            await buffer.push(.turnFinished(reason: .cancelled))
            await buffer.finish()
        }
    }
}

/// FIFO buffer bridging the scripted producer to `pollBatch` consumers.
actor MockEventBuffer {
    private var queue: [AgentEvent] = []
    private var finished = false
    private var waiters: [CheckedContinuation<Void, Never>] = []

    func push(_ event: AgentEvent) {
        guard !finished else { return }
        queue.append(event)
        wakeWaiters()
    }

    func finish() {
        finished = true
        wakeWaiters()
    }

    /// Suspends until at least one event is available; returns `[]` once the
    /// stream has ended — exactly the documented `poll_batch` semantics.
    func nextBatch(max maxEvents: Int) async -> [AgentEvent] {
        while queue.isEmpty && !finished {
            await withCheckedContinuation { waiters.append($0) }
        }
        let count = Swift.min(maxEvents, queue.count)
        let batch = Array(queue.prefix(count))
        queue.removeFirst(count)
        return batch
    }

    private func wakeWaiters() {
        let pending = waiters
        waiters.removeAll()
        pending.forEach { $0.resume() }
    }
}

// MARK: - Scripts

/// A canned turn outline picked from the user's text.
struct TurnScript {
    enum Step {
        case text(String)
        case tool(ToolEvent)
        case approval(summary: String, level: DestructiveLevel)
        case modeHandoff(from: ComputeMode, to: ComputeMode)
    }

    let steps: [Step]

    static func select(for userText: String) -> TurnScript {
        let lowered = userText.lowercased()
        if lowered.contains("click") || lowered.contains("open ")
            || lowered.contains("computer") || lowered.contains("screenshot") {
            return computerUse
        }
        if lowered.contains("delete") || lowered.contains("run ")
            || lowered.contains("install") {
            return gatedCommand
        }
        if lowered.contains("search") || lowered.contains("look up")
            || lowered.contains("find") {
            return webSearch
        }
        return plainAnswer
    }

    static let plainAnswer = TurnScript(steps: [
        .text(
            "Here's what I'd suggest:\n\n"
            + "1. **Start small** — sketch the flow before wiring anything.\n"
            + "2. Keep the seams explicit so each part can be swapped later.\n"
            + "3. Ship the boring version first, then iterate.\n\n"
            + "Want me to expand on any of these?"
        ),
    ])

    static let webSearch = TurnScript(steps: [
        .text("Let me look that up. "),
        .tool(.chunk(data: "web.search: querying…")),
        .tool(.citation(Citation(
            url: "https://example.com/article",
            title: "A useful article",
            excerpt: "The relevant passage, quoted for the citation preview."
        ))),
        .tool(.result(outputJSON: #"{"results": 3}"#, truncated: false)),
        .text(
            "Based on what I found, the short answer is **yes** — with one caveat "
            + "worth knowing about. The longer answer depends on your setup; the "
            + "cited article walks through the details."
        ),
    ])

    static let gatedCommand = TurnScript(steps: [
        .text("I can do that. I'll need to run a command — requesting approval first. "),
        .approval(
            summary: "command:rm -rf ~/Library/Caches/example-stale-cache",
            level: .high
        ),
        .text("The cache directory is gone and the app will rebuild it on next launch."),
    ])

    static let computerUse = TurnScript(steps: [
        .text("I'll take care of that on screen. "),
        .tool(.chunk(data: "computer.screenshot: capturing main display")),
        .tool(.chunk(data: "computer.describe: 14 actionable elements found")),
        .approval(
            summary: "Click \u{201C}Submit\u{201D} in Safari — example.com/form",
            level: .high
        ),
        .tool(.chunk(data: "computer.click: (842, 513) left ×1")),
        .modeHandoff(from: .onDevice, to: .macLan),
        .text("Done — the form was submitted and the confirmation page loaded."),
    ])
}

// MARK: - State

/// All mutable mock state, isolated on one actor.
actor MockEngineState {
    private var conversations: [UUID: Conversation] = [:]
    private var messages: [UUID: [Message]] = [:]
    private var approvalWaiters: [UUID: CheckedContinuation<Bool, Never>] = [:]
    private(set) var activeMode: ComputeMode = .onDevice

    func setActiveMode(_ mode: ComputeMode) {
        activeMode = mode
    }

    func createConversation(mode: ComputeMode) -> UUID {
        let conversation = Conversation(modeAtCreation: mode)
        conversations[conversation.id] = conversation
        messages[conversation.id] = []
        activeMode = mode
        return conversation.id
    }

    func listConversations(limit: Int) -> [Conversation] {
        Array(
            conversations.values
                .sorted { $0.updatedAt > $1.updatedAt }
                .prefix(limit)
        )
    }

    func history(conversationID: UUID) -> [Message] {
        messages[conversationID] ?? []
    }

    func appendUserMessage(conversationID: UUID, text: String) {
        append(conversationID: conversationID, role: .user, text: text)
        if var conversation = conversations[conversationID], conversation.title == nil {
            conversation.title = String(text.prefix(48))
            conversations[conversationID] = conversation
        }
    }

    func appendAssistantMessage(conversationID: UUID, text: String, mode: ComputeMode) {
        append(conversationID: conversationID, role: .assistant, text: text, mode: mode)
    }

    private func append(
        conversationID: UUID,
        role: MessageRole,
        text: String,
        mode: ComputeMode? = nil
    ) {
        var thread = messages[conversationID] ?? []
        thread.append(Message(
            conversationID: conversationID,
            role: role,
            content: [.text(text)],
            mode: mode ?? activeMode,
            sequenceNum: Int64(thread.count)
        ))
        messages[conversationID] = thread
        if var conversation = conversations[conversationID] {
            conversation.updatedAt = .now
            conversations[conversationID] = conversation
        }
    }

    func waitForApproval(id: UUID) async -> Bool {
        await withCheckedContinuation { approvalWaiters[id] = $0 }
    }

    func resolveApproval(id: UUID, approved: Bool) {
        approvalWaiters.removeValue(forKey: id)?.resume(returning: approved)
    }

    func seedSampleData() {
        let first = Conversation(
            title: "Plan the quarterly review deck",
            createdAt: .now.addingTimeInterval(-7200),
            updatedAt: .now.addingTimeInterval(-3600),
            modeAtCreation: .macLan
        )
        let second = Conversation(
            title: "Debug the sync engine",
            createdAt: .now.addingTimeInterval(-86_400),
            updatedAt: .now.addingTimeInterval(-80_000),
            modeAtCreation: .onDevice
        )
        conversations[first.id] = first
        conversations[second.id] = second
        messages[first.id] = [
            Message(
                conversationID: first.id, role: .user,
                content: [.text("Help me outline the quarterly review deck.")],
                mode: .macLan, sequenceNum: 0
            ),
            Message(
                conversationID: first.id, role: .assistant,
                content: [.text(
                    "Here's a five-slide outline:\n\n"
                    + "1. **Headline results** — two numbers, one chart\n"
                    + "2. Wins and misses\n"
                    + "3. Customer signal\n"
                    + "4. Next quarter's bets\n"
                    + "5. Asks"
                )],
                mode: .macLan, sequenceNum: 1
            ),
        ]
        messages[second.id] = []
    }
}
