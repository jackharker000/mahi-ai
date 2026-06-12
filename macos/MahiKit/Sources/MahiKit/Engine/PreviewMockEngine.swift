import Foundation
import os

/// An in-memory, scripted `MahiEngineProtocol` used for SwiftUI previews, unit tests,
/// and running the app before the Rust core has been built
/// (`scripts/build-xcframework.sh`). It streams plausible turns — including tool
/// activity and an approval gate when the prompt looks action-like — with realistic
/// pacing so the streaming UI can be exercised end to end. It also simulates the
/// Ollama-style local model manager: downloads with progress, a managed runtime
/// lifecycle, hosted-provider configuration, and parallel subagents.
public final class PreviewMockEngine: MahiEngineProtocol, @unchecked Sendable {
    private let state: MockEngineState
    private let models: MockModelStore
    private let chunkDelay: Duration

    /// - Parameters:
    ///   - seeded: pre-populate sample conversations (used by previews).
    ///   - chunkDelay: pacing between streamed text chunks.
    ///   - downloadTick: how often a simulated download advances (~8% per tick).
    ///   - activationStep: one third of the simulated runtime spin-up time
    ///     (`noModel → preparingRuntime → starting → running` takes ~3 steps).
    public init(
        seeded: Bool = false,
        chunkDelay: Duration = .milliseconds(24),
        downloadTick: Duration = .milliseconds(200),
        activationStep: Duration = .milliseconds(500)
    ) {
        self.state = MockEngineState()
        self.models = MockModelStore(
            downloadTick: downloadTick,
            activationStep: activationStep
        )
        self.chunkDelay = chunkDelay
        if seeded {
            Task { await state.seedSampleData() }
        }
    }

    /// A pre-seeded engine for SwiftUI previews.
    public static func preview() -> PreviewMockEngine {
        PreviewMockEngine(seeded: true)
    }

    // MARK: MahiEngineProtocol — conversations

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
        let state = self.state
        let delay = self.chunkDelay

        handle.producer = Task {
            await state.appendUserMessage(conversationID: conversationID, text: userText)
            let turnNumber = await state.nextTurnNumber()
            var script = TurnScript.select(for: userText)
            // Occasionally gate an otherwise-plain turn behind an approval so the
            // ApprovalSheet path stays exercised even for benign prompts.
            if turnNumber.isMultiple(of: 5), !script.containsApproval {
                script = script.appendingPeriodicApproval()
            }
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

    // MARK: MahiEngineProtocol — local model management

    public func modelCatalog() async throws -> [CatalogModel] {
        models.catalogSnapshot()
    }

    public func startDownload(modelID: String) async throws {
        try models.startDownload(modelID: modelID)
    }

    public func cancelDownload(modelID: String) async throws {
        try models.cancelDownload(modelID: modelID)
    }

    public func deleteModel(modelID: String) async throws {
        try models.deleteModel(modelID: modelID)
    }

    public func activateModel(modelID: String) async throws {
        try models.activateModel(modelID: modelID)
    }

    public func runtimeStatus() async throws -> RuntimeStatus {
        models.runtimeSnapshot()
    }

    // MARK: MahiEngineProtocol — hosted provider

    public func setHostedConfig(_ config: HostedConfig?) async throws {
        models.setHostedConfig(config)
    }

    /// The last stored hosted config (test/preview introspection).
    public var hostedConfig: HostedConfig? {
        models.hostedConfigSnapshot()
    }

    // MARK: MahiEngineProtocol — subagents

    public func spawnSubagents(goals: [String]) async throws -> [String] {
        try await Task.sleep(for: .milliseconds(200))
        try Task.checkCancellation()
        return goals.map { goal in
            let trimmed = goal.trimmingCharacters(in: .whitespacesAndNewlines)
            return "\(trimmed.isEmpty ? "(empty goal)" : trimmed) → done (mock)"
        }
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

// MARK: - Model store

/// All local-model-manager mock state, guarded by one unfair lock so the engine
/// stays a plain `Sendable` class (no actor hops on the hot polling paths).
final class MockModelStore: Sendable {
    private struct Guarded: Sendable {
        var catalog: [CatalogModel] = MockModelStore.seedCatalog()
        var runtime: RuntimeStatus = .noModel
        var hostedConfig: HostedConfig?
        var downloadTasks: [String: Task<Void, Never>] = [:]
        /// Bumped on every activate/delete so a stale activation task stops writing.
        var activationEpoch: Int = 0
    }

    private let lock = OSAllocatedUnfairLock(initialState: Guarded())
    private let downloadTick: Duration
    private let activationStep: Duration
    /// Fraction of the file fetched per download tick.
    private let downloadIncrement: Double = 0.08

    init(downloadTick: Duration, activationStep: Duration) {
        self.downloadTick = downloadTick
        self.activationStep = activationStep
    }

    // MARK: Snapshots

    func catalogSnapshot() -> [CatalogModel] {
        lock.withLock { $0.catalog }
    }

    func runtimeSnapshot() -> RuntimeStatus {
        lock.withLock { $0.runtime }
    }

    func hostedConfigSnapshot() -> HostedConfig? {
        lock.withLock { $0.hostedConfig }
    }

    func setHostedConfig(_ config: HostedConfig?) {
        lock.withLock { $0.hostedConfig = config }
    }

    // MARK: Downloads

    func startDownload(modelID: String) throws {
        let sizeBytes: Int64 = try lock.withLock { state in
            guard let index = state.catalog.firstIndex(where: { $0.id == modelID }) else {
                throw EngineError.invalidIdentifier(modelID)
            }
            switch state.catalog[index].state {
            case .notInstalled:
                state.catalog[index].state = .downloading(progress: 0, bytesDownloaded: 0)
                return state.catalog[index].sizeBytes
            case .downloading, .installed, .active:
                // Already fetching or on disk — nothing to start.
                throw EngineError.engine(
                    message: "\(state.catalog[index].displayName) is already downloaded or downloading."
                )
            }
        }

        let task = Task.detached { [weak self] in
            guard let self else { return }
            var progress = 0.0
            while !Task.isCancelled && progress < 1.0 {
                do {
                    try await Task.sleep(for: self.downloadTick)
                } catch {
                    return // cancelled mid-sleep; cancelDownload resets the state
                }
                progress = min(1.0, progress + self.downloadIncrement)
                let finished = self.advanceDownload(modelID: modelID, progress: progress, sizeBytes: sizeBytes)
                if finished { return }
            }
        }

        lock.withLock { state in
            state.downloadTasks[modelID] = task
        }
    }

    /// Returns true when the download reached `.installed` (or was torn down).
    private func advanceDownload(modelID: String, progress: Double, sizeBytes: Int64) -> Bool {
        lock.withLock { state in
            guard let index = state.catalog.firstIndex(where: { $0.id == modelID }),
                  case .downloading = state.catalog[index].state else {
                // Cancelled or deleted out from under us — stop quietly.
                state.downloadTasks[modelID] = nil
                return true
            }
            if progress >= 1.0 {
                state.catalog[index].state = .installed
                state.downloadTasks[modelID] = nil
                return true
            }
            state.catalog[index].state = .downloading(
                progress: progress,
                bytesDownloaded: Int64(progress * Double(sizeBytes))
            )
            return false
        }
    }

    func cancelDownload(modelID: String) throws {
        let task: Task<Void, Never>? = try lock.withLock { state in
            guard let index = state.catalog.firstIndex(where: { $0.id == modelID }) else {
                throw EngineError.invalidIdentifier(modelID)
            }
            if case .downloading = state.catalog[index].state {
                state.catalog[index].state = .notInstalled
            }
            return state.downloadTasks.removeValue(forKey: modelID)
        }
        task?.cancel()
    }

    func deleteModel(modelID: String) throws {
        let task: Task<Void, Never>? = try lock.withLock { state in
            guard let index = state.catalog.firstIndex(where: { $0.id == modelID }) else {
                throw EngineError.invalidIdentifier(modelID)
            }
            let wasServing = state.runtime.currentModelID == modelID
                || state.catalog[index].state == .active
            state.catalog[index].state = .notInstalled
            if wasServing {
                state.runtime = .noModel
                state.activationEpoch += 1 // stop any in-flight activation
            }
            return state.downloadTasks.removeValue(forKey: modelID)
        }
        task?.cancel()
    }

    // MARK: Runtime activation

    func activateModel(modelID: String) throws {
        let epoch: Int = try lock.withLock { state in
            guard let index = state.catalog.firstIndex(where: { $0.id == modelID }) else {
                throw EngineError.invalidIdentifier(modelID)
            }
            switch state.catalog[index].state {
            case .installed, .active:
                break // weights are on disk; ok to (re)load
            case .notInstalled, .downloading:
                throw EngineError.engine(
                    message: "\(state.catalog[index].displayName) is not installed yet."
                )
            }
            state.activationEpoch += 1
            state.runtime = .preparingRuntime
            return state.activationEpoch
        }

        Task.detached { [weak self] in
            guard let self else { return }
            do {
                try await Task.sleep(for: self.activationStep)
                guard self.advanceActivation(epoch: epoch, to: .starting(modelID: modelID)) else { return }
                try await Task.sleep(for: self.activationStep)
                try await Task.sleep(for: self.activationStep)
                _ = self.advanceActivation(epoch: epoch, to: .running(modelID: modelID))
            } catch {
                // Cancelled sleep — a newer activation/delete owns the runtime now.
            }
        }
    }

    /// Apply one activation phase iff no newer activate/delete superseded `epoch`.
    /// Returns false when stale.
    private func advanceActivation(epoch: Int, to status: RuntimeStatus) -> Bool {
        lock.withLock { state in
            guard state.activationEpoch == epoch else { return false }
            state.runtime = status
            if case .running(let runningID) = status {
                for index in state.catalog.indices {
                    if state.catalog[index].id == runningID {
                        state.catalog[index].state = .active
                    } else if state.catalog[index].state == .active {
                        state.catalog[index].state = .installed
                    }
                }
            }
            return true
        }
    }

    // MARK: Seed catalog

    static func seedCatalog() -> [CatalogModel] {
        [
            CatalogModel(
                id: "qwen2.5-0.5b-instruct-q4_k_m",
                displayName: "Qwen2.5 0.5B Instruct",
                family: "Qwen2.5",
                sizeBytes: 398_000_000,
                quantization: "Q4_K_M",
                contextWindow: 32_768,
                toolCalling: true,
                description: "Tiny, instant-loading model for quick local replies and smoke tests."
            ),
            CatalogModel(
                id: "llama-3.2-3b-instruct-q4_k_m",
                displayName: "Llama 3.2 3B Instruct",
                family: "Llama 3.2",
                sizeBytes: 2_000_000_000,
                quantization: "Q4_K_M",
                contextWindow: 131_072,
                toolCalling: true,
                description: "Fast small Llama with a long context — a good laptop default."
            ),
            CatalogModel(
                id: "llama-3.1-8b-instruct-q4_k_m",
                displayName: "Llama 3.1 8B Instruct",
                family: "Llama 3.1",
                sizeBytes: 4_900_000_000,
                quantization: "Q4_K_M",
                contextWindow: 131_072,
                toolCalling: true,
                description: "The all-round 8B workhorse; strong tool calling and reasoning."
            ),
            CatalogModel(
                id: "qwen2.5-7b-instruct-q4_k_m",
                displayName: "Qwen2.5 7B Instruct",
                family: "Qwen2.5",
                sizeBytes: 4_700_000_000,
                quantization: "Q4_K_M",
                contextWindow: 32_768,
                toolCalling: true,
                description: "Balanced quality and speed; excellent multilingual coverage."
            ),
            CatalogModel(
                id: "qwen2.5-coder-7b-q4_k_m",
                displayName: "Qwen2.5 Coder 7B",
                family: "Qwen2.5",
                sizeBytes: 4_700_000_000,
                quantization: "Q4_K_M",
                contextWindow: 32_768,
                toolCalling: true,
                description: "Code-specialized Qwen; the local pick for diffs and refactors."
            ),
            CatalogModel(
                id: "mistral-7b-v0.3-q4_k_m",
                displayName: "Mistral 7B v0.3",
                family: "Mistral",
                sizeBytes: 4_400_000_000,
                quantization: "Q4_K_M",
                contextWindow: 32_768,
                toolCalling: true,
                description: "Classic 7B with native function calling since v0.3."
            ),
            CatalogModel(
                id: "phi-3.5-mini-q4_k_m",
                displayName: "Phi-3.5 mini",
                family: "Phi",
                sizeBytes: 2_400_000_000,
                quantization: "Q4_K_M",
                contextWindow: 131_072,
                toolCalling: false,
                description: "Compact reasoning model; great summaries, no tool calls."
            ),
            CatalogModel(
                id: "gemma-2-9b-q4_k_m",
                displayName: "Gemma 2 9B",
                family: "Gemma 2",
                sizeBytes: 5_800_000_000,
                quantization: "Q4_K_M",
                contextWindow: 8_192,
                toolCalling: false,
                description: "High-quality prose and chat; shorter context, no tool calls."
            ),
        ]
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

    var containsApproval: Bool {
        steps.contains { step in
            if case .approval = step { return true }
            return false
        }
    }

    /// Inject a low-stakes approval gate (plus a tool pair acting on it) so the
    /// approval UI is exercised even on otherwise-plain turns.
    func appendingPeriodicApproval() -> TurnScript {
        TurnScript(steps: steps + [
            .text("\n\nWhile I'm here, I'd like to save a note about this conversation. "),
            .approval(
                summary: "command:echo \"conversation summary\" >> ~/Documents/mahi-notes.md",
                level: .low
            ),
            .tool(.chunk(data: "fs.append: ~/Documents/mahi-notes.md")),
            .tool(.result(outputJSON: #"{"bytesWritten": 24}"#, truncated: false)),
            .text("Saved the note."),
        ])
    }

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
        .text("Let me check what we already know. "),
        .tool(.chunk(data: "memory.search: scanning 128 local notes")),
        .tool(.result(outputJSON: #"{"matches": 2}"#, truncated: false)),
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
        .tool(.chunk(data: "shell.exec: rm -rf ~/Library/Caches/example-stale-cache")),
        .tool(.result(outputJSON: #"{"exitCode": 0}"#, truncated: false)),
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
        .tool(.result(outputJSON: #"{"clicked": true}"#, truncated: false)),
        .modeHandoff(from: .onDevice, to: .macLan),
        .text("Done — the form was submitted and the confirmation page loaded."),
    ])
}

// MARK: - State

/// All mutable conversation/approval mock state, isolated on one actor.
actor MockEngineState {
    private var conversations: [UUID: Conversation] = [:]
    private var messages: [UUID: [Message]] = [:]
    private var approvalWaiters: [UUID: CheckedContinuation<Bool, Never>] = [:]
    private var turnCounter = 0
    private(set) var activeMode: ComputeMode = .onDevice

    func setActiveMode(_ mode: ComputeMode) {
        activeMode = mode
    }

    /// 1-based count of turns run on this engine (drives the periodic approval).
    func nextTurnNumber() -> Int {
        turnCounter += 1
        return turnCounter
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
