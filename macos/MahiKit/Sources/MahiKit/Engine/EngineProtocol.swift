import Foundation

/// Errors surfaced by the engine seam to the UI layer.
public enum EngineError: LocalizedError, Sendable {
    /// The Rust core is not linked into this build (run scripts/build-xcframework.sh).
    case engineUnavailable
    case invalidIdentifier(String)
    case cancelled
    case engine(message: String)

    public var errorDescription: String? {
        switch self {
        case .engineUnavailable:
            return "The Mahi engine is not built into this binary. "
                + "Run macos/scripts/build-xcframework.sh and rebuild."
        case .invalidIdentifier(let raw):
            return "The engine returned a malformed identifier: \(raw)"
        case .cancelled:
            return "The operation was cancelled."
        case .engine(let message):
            return message
        }
    }
}

/// A handle on one in-flight agent turn.
///
/// Mirror of the FFI `TurnHandle` (docs/backend/phase-0/03-engine-facade.md):
/// `poll_batch(max) -> [AgentEventFfi]` + `cancel()`. `pollBatch` suspends until at
/// least one event is available and returns an empty array exactly once, after the
/// turn's event stream has ended.
public protocol TurnHandleProtocol: AnyObject, Sendable {
    func pollBatch(maxEvents: UInt32) async throws -> [AgentEvent]
    func cancel()
}

public extension TurnHandleProtocol {
    /// Adapt the poll-based FFI handle to an idiomatic `AsyncSequence`.
    ///
    /// The stream finishes after `turnFinished` (or an empty batch / thrown error),
    /// and cancelling the consuming task cancels the underlying turn.
    func events(batchSize: UInt32 = 64) -> AsyncThrowingStream<AgentEvent, Error> {
        AsyncThrowingStream { continuation in
            let pump = Task {
                do {
                    pumping: while !Task.isCancelled {
                        let batch = try await self.pollBatch(maxEvents: batchSize)
                        if batch.isEmpty { break }
                        for event in batch {
                            continuation.yield(event)
                            if case .turnFinished = event { break pumping }
                        }
                    }
                    continuation.finish()
                } catch {
                    continuation.finish(throwing: error)
                }
            }
            continuation.onTermination = { termination in
                pump.cancel()
                if case .cancelled = termination {
                    self.cancel()
                }
            }
        }
    }
}

/// The engine seam the rest of MahiKit (and the app) programs against.
///
/// Mirror of the `MahiEngine` facade (docs/backend/phase-0/03-engine-facade.md).
/// Implementations:
///   - `FfiEngine` — wraps the UniFFI-generated `Mahi.MahiEngine` (the real core).
///   - `PreviewMockEngine` — in-memory scripted engine for SwiftUI previews, tests,
///     and running the app before the Rust core has been built.
public protocol MahiEngineProtocol: AnyObject, Sendable {
    func createConversation(mode: ComputeMode) async throws -> UUID
    func listConversations(limit: Int) async throws -> [Conversation]
    func history(conversationID: UUID) async throws -> [Message]

    /// Run one agent turn. The user message and the final assistant message are
    /// persisted by the core; the caller streams events off the returned handle.
    func runTurn(conversationID: UUID, userText: String) async throws -> any TurnHandleProtocol

    /// Respond to a pending approval (id from `AgentEvent.approvalRequired`).
    func resolveApproval(approvalID: UUID, approved: Bool) async throws

    // Local model management (Ollama-style, app-managed llama runtime)
    func modelCatalog() async throws -> [CatalogModel]
    func startDownload(modelID: String) async throws
    func cancelDownload(modelID: String) async throws
    func deleteModel(modelID: String) async throws

    /// Load `modelID` into the managed runtime with a context window of
    /// `contextTokens` tokens (the engine clamps to the model's maximum).
    /// Larger windows use more memory and slow generation down.
    func activateModel(modelID: String, contextTokens: Int) async throws
    func runtimeStatus() async throws -> RuntimeStatus

    /// Resize the context window used for subsequent turns without reloading
    /// the model. Hosted providers honor very large windows (up to ~1M tokens).
    func setContextWindow(contextTokens: Int) async throws

    // Hosted (cloud) provider configuration; nil clears it.
    func setHostedConfig(_ config: HostedConfig?) async throws

    // Agent control (the "/goal" and "/compact" commands).

    /// Set (or replace) the standing goal that steers the agent across every
    /// turn of `conversationID`.
    func setGoal(conversationID: UUID, goal: String) async throws

    /// Summarize-and-prune the conversation's context; returns the summary
    /// the agent will carry forward.
    func compact(conversationID: UUID) async throws -> String

    // Parallel subagents: returns one short result summary per goal.
    func spawnSubagents(goals: [String]) async throws -> [String]
}
