import Foundation
import MahiKit
import SwiftUI

/// The app-facing view model. Owns the engine seam and drives one chat
/// conversation: loading history, sending turns, streaming assistant output,
/// and surfacing approval prompts. Also fronts the local-model manager
/// (catalog, downloads, runtime), hosted-provider config, and subagent runs.
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

    // Local model manager
    @Published public private(set) var models: [CatalogModel] = []
    @Published public private(set) var runtime: RuntimeStatus = .noModel
    /// The context window (tokens) the user has chosen for new model runs /
    /// hosted turns. Larger = more context, slower.
    @Published public var contextTokens: Int = 8192

    // Tool activity within the current streaming turn
    @Published public private(set) var activeTool: ActiveTool?

    // Transient info banner (e.g. a /compact summary).
    @Published public var infoText: String?

    // Subagents
    @Published public private(set) var agentResults: [String] = []
    @Published public private(set) var isRunningAgents = false

    /// A gated action awaiting the user's decision.
    public struct PendingApproval: Identifiable {
        public let id: UUID
        public let summary: String
        public let preview: ApprovalPreview
    }

    /// The tool currently running inside the streaming turn (drives the
    /// spinner bubble in `ChatView`).
    public struct ActiveTool: Equatable {
        public let name: String
        public let detail: String
    }

    private let engine: any MahiEngineProtocol
    private var turnTask: Task<Void, Never>?
    private var modelPollTask: Task<Void, Never>?

    public init(engine: (any MahiEngineProtocol)? = nil) {
        // Built in the @MainActor init body (not a default argument) so the
        // engine's main-actor-bound construction is correctly isolated.
        self.engine = engine ?? EngineFactory.makeDefault()
    }

    public func bootstrap() async {
        await reloadConversations()
        if let first = conversations.first {
            await select(first.id)
        } else {
            await newConversation()
        }
        await refreshModels()
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

    // MARK: - Local model manager

    /// Fetch the catalog + runtime status and keep polling while anything is
    /// downloading or the runtime is spinning up.
    public func refreshModels() async {
        await refreshModelStateOnce()
        startModelPollingIfNeeded()
    }

    public func download(modelID: String) async {
        do { try await engine.startDownload(modelID: modelID) } catch {
            errorText = error.localizedDescription
        }
        await refreshModels()
    }

    public func cancelDownload(modelID: String) async {
        do { try await engine.cancelDownload(modelID: modelID) } catch {
            errorText = error.localizedDescription
        }
        await refreshModels()
    }

    public func delete(modelID: String) async {
        do { try await engine.deleteModel(modelID: modelID) } catch {
            errorText = error.localizedDescription
        }
        await refreshModels()
    }

    public func activate(modelID: String, contextTokens: Int? = nil) async {
        let tokens = contextTokens ?? self.contextTokens
        self.contextTokens = tokens
        do { try await engine.activateModel(modelID: modelID, contextTokens: tokens) } catch {
            errorText = error.localizedDescription
        }
        await refreshModels()
    }

    /// Resize the context window for subsequent turns (hosted models support
    /// very large windows; bigger is slower).
    public func setContextWindow(tokens: Int) async {
        contextTokens = tokens
        do { try await engine.setContextWindow(contextTokens: tokens) } catch {
            errorText = error.localizedDescription
        }
    }

    /// Toggle extended thinking and its token budget for subsequent turns.
    public func setThinking(enabled: Bool, budgetTokens: Int) async {
        do {
            try await engine.setThinking(
                enabled: enabled,
                budgetTokens: UInt32(max(0, budgetTokens))
            )
        } catch {
            errorText = error.localizedDescription
        }
    }

    /// Set (empty clears) a persistent goal for the current conversation.
    public func setGoal(_ goal: String) async {
        guard let convID = selectedID else { return }
        do { try await engine.setGoal(conversationID: convID, goal: goal) } catch {
            errorText = error.localizedDescription
        }
    }

    /// Compact the current conversation, surfacing the summary as an info banner.
    public func compact() async {
        guard let convID = selectedID else { return }
        do {
            let summary = try await engine.compact(conversationID: convID)
            infoText = summary.isEmpty ? "Nothing to compact yet." : "Compacted. \(summary)"
            await select(convID)
        } catch {
            errorText = error.localizedDescription
        }
    }

    /// User-facing display name for a catalog id, when known.
    public func displayName(forModelID id: String) -> String? {
        models.first { $0.id == id }?.displayName
    }

    /// One-line runtime summary for headers and toolbar badges.
    public var runtimeHeadline: String {
        switch runtime {
        case .noModel:
            return "No model loaded"
        case .preparingRuntime:
            return "Preparing runtime…"
        case .starting(let id):
            return "Starting \(displayName(forModelID: id) ?? id)…"
        case .running(let id):
            return "Running \(displayName(forModelID: id) ?? id)"
        case .failed(let message):
            return "Runtime failed: \(message)"
        }
    }

    private var needsModelPolling: Bool {
        if runtime.isTransitioning { return true }
        return models.contains { model in
            if case .downloading = model.state { return true }
            return false
        }
    }

    private func refreshModelStateOnce() async {
        do {
            models = try await engine.modelCatalog()
            runtime = try await engine.runtimeStatus()
        } catch {
            errorText = error.localizedDescription
        }
    }

    private func startModelPollingIfNeeded() {
        guard modelPollTask == nil, needsModelPolling else { return }
        modelPollTask = Task { [weak self] in
            while !Task.isCancelled {
                try? await Task.sleep(for: .milliseconds(300))
                guard let self, !Task.isCancelled else { return }
                await self.refreshModelStateOnce()
                if !self.needsModelPolling { break }
            }
            self?.modelPollTask = nil
        }
    }

    // MARK: - Hosted provider

    /// Persist the hosted (cloud) provider config into the engine.
    /// An empty API key clears the config.
    public func saveHostedConfig(
        provider: HostedProvider,
        apiKey: String,
        model: String,
        baseURL: String?
    ) async {
        let key = apiKey.trimmingCharacters(in: .whitespacesAndNewlines)
        let modelID = model.trimmingCharacters(in: .whitespacesAndNewlines)
        let base = baseURL?.trimmingCharacters(in: .whitespacesAndNewlines)
        let config: HostedConfig? = key.isEmpty ? nil : HostedConfig(
            provider: provider,
            apiKey: key,
            model: modelID,
            baseURL: (base?.isEmpty ?? true) ? nil : base
        )
        do { try await engine.setHostedConfig(config) } catch {
            errorText = error.localizedDescription
        }
    }

    // MARK: - Subagents

    /// Run one subagent per non-empty goal and publish their summaries.
    public func runAgents(goals: [String]) async {
        let cleaned = goals
            .map { $0.trimmingCharacters(in: .whitespacesAndNewlines) }
            .filter { !$0.isEmpty }
        guard !cleaned.isEmpty, !isRunningAgents else { return }
        isRunningAgents = true
        agentResults = []
        do {
            agentResults = try await engine.spawnSubagents(goals: cleaned)
        } catch {
            errorText = error.localizedDescription
        }
        isRunningAgents = false
    }

    // MARK: - Turn streaming

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
                    handleToolEvent(toolEvent, convID: convID)
                case .turnFinished:
                    activeTool = nil
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
        activeTool = nil
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

    private func handleToolEvent(_ event: ToolEvent, convID: UUID) {
        switch event {
        case .chunk(let data):
            // Chunks look like "web.search: querying…" — split into name + detail.
            activeTool = Self.parseTool(chunk: data)
        case .citation(let citation):
            appendToolNote("\(activeTool?.name ?? "web") source: \(citation.url)", convID: convID)
        case .result(let outputJSON, _):
            appendToolNote("\(activeTool?.name ?? "tool") result: \(outputJSON)", convID: convID)
            activeTool = nil
        case .error(let message, _):
            appendToolNote("\(activeTool?.name ?? "tool") error: \(message)", convID: convID)
            activeTool = nil
        case .approvalRequired(let approvalID, let summary, _):
            // Tool-level gate: surface through the same approval sheet.
            pendingApproval = PendingApproval(
                id: approvalID,
                summary: summary,
                preview: ApprovalPreview.parse(summary: summary)
            )
        case .cancelled:
            activeTool = nil
        }
    }

    private static func parseTool(chunk: String) -> ActiveTool {
        let pieces = chunk.split(separator: ":", maxSplits: 1, omittingEmptySubsequences: false)
        guard pieces.count == 2 else {
            return ActiveTool(name: chunk, detail: "")
        }
        return ActiveTool(
            name: pieces[0].trimmingCharacters(in: .whitespaces),
            detail: pieces[1].trimmingCharacters(in: .whitespaces)
        )
    }

    private func appendToolNote(_ note: String, convID: UUID) {
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
