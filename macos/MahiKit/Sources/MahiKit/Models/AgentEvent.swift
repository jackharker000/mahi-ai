import Foundation

/// Why a generation stream ended. Mirror of `mahi_contracts::compute::FinishReason`.
public enum FinishReason: String, Codable, Hashable, Sendable {
    case stop
    case toolCall
    case maxTokens
    case cancelled
    case error
}

/// How destructive a gated action is. Mirror of `mahi_contracts::tooling::DestructiveLevel`.
public enum DestructiveLevel: String, Codable, Hashable, Sendable, Comparable {
    case low
    case high
    case critical

    private var rank: Int {
        switch self {
        case .low: return 0
        case .high: return 1
        case .critical: return 2
        }
    }

    public static func < (lhs: DestructiveLevel, rhs: DestructiveLevel) -> Bool {
        lhs.rank < rhs.rank
    }
}

/// A web citation attached to tool output.
public struct Citation: Identifiable, Hashable, Sendable {
    public let id: UUID
    public let url: String
    public let title: String?
    public let excerpt: String?

    public init(id: UUID = UUID(), url: String, title: String? = nil, excerpt: String? = nil) {
        self.id = id
        self.url = url
        self.title = title
        self.excerpt = excerpt
    }
}

/// One event in a tool's lifecycle. Mirror of `mahi_contracts::tooling::ToolEvent`.
public enum ToolEvent: Hashable, Sendable {
    case chunk(data: String)
    case citation(Citation)
    case approvalRequired(approvalID: UUID, summary: String, destructiveLevel: DestructiveLevel)
    /// `outputJSON` is the tool's JSON output, serialized; the UI treats it as opaque text.
    case result(outputJSON: String, truncated: Bool)
    case error(message: String, retryable: Bool)
    case cancelled
}

/// A single event in an agent turn, streamed to the surface.
/// Mirror of `mahi_contracts::agent::AgentEvent`.
public enum AgentEvent: Hashable, Sendable {
    /// A turn has begun.
    case turnStarted(conversationID: UUID, messageID: UUID, mode: ComputeMode)
    /// A chunk of assistant text.
    case textDelta(String)
    /// A tool produced an event.
    case tool(ToolEvent)
    /// The active compute mode changed mid-turn (handoff).
    case modeHandoff(from: ComputeMode, to: ComputeMode)
    /// An approval is required before continuing.
    case approvalRequired(approvalID: UUID, summary: String)
    /// The turn finished.
    case turnFinished(reason: FinishReason)
    /// A non-fatal error occurred.
    case error(message: String)
}
