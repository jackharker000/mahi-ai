import Foundation

/// A conversation/session. Mirror of `mahi_contracts::data::Conversation`
/// (the fields the Mac UI renders; sync/persona internals stay behind the FFI).
public struct Conversation: Identifiable, Hashable, Sendable {
    public let id: UUID
    public var title: String?
    public let createdAt: Date
    public var updatedAt: Date
    public let modeAtCreation: ComputeMode

    public init(
        id: UUID = UUID(),
        title: String? = nil,
        createdAt: Date = .now,
        updatedAt: Date = .now,
        modeAtCreation: ComputeMode
    ) {
        self.id = id
        self.title = title
        self.createdAt = createdAt
        self.updatedAt = updatedAt
        self.modeAtCreation = modeAtCreation
    }

    /// Title to render when none has been generated yet.
    public var displayTitle: String {
        if let title, !title.isEmpty { return title }
        return "New Conversation"
    }
}

/// Mirror of `mahi_contracts::data::MessageRole`.
public enum MessageRole: String, Codable, Hashable, Sendable {
    case user
    case assistant
    case tool
    case system
}

/// A unit of message content. Mirror of `mahi_contracts::data::ContentBlock`.
public enum ContentBlock: Hashable, Sendable {
    case text(String)
    case toolCall(callID: String, toolID: String, argsJSON: String)
    case toolResult(callID: String, outputJSON: String)
    case artifactRef(artifactID: UUID)
}

/// One persisted message in a conversation. Mirror of `mahi_contracts::data::Message`.
public struct Message: Identifiable, Hashable, Sendable {
    public let id: UUID
    public let conversationID: UUID
    public let role: MessageRole
    public let content: [ContentBlock]
    public let modelID: String?
    public let mode: ComputeMode
    public let createdAt: Date
    public let sequenceNum: Int64

    public init(
        id: UUID = UUID(),
        conversationID: UUID,
        role: MessageRole,
        content: [ContentBlock],
        modelID: String? = nil,
        mode: ComputeMode,
        createdAt: Date = .now,
        sequenceNum: Int64
    ) {
        self.id = id
        self.conversationID = conversationID
        self.role = role
        self.content = content
        self.modelID = modelID
        self.mode = mode
        self.createdAt = createdAt
        self.sequenceNum = sequenceNum
    }

    /// Concatenate all text blocks into a single string.
    public var textContent: String {
        content.compactMap { block in
            if case .text(let text) = block { return text }
            return nil
        }.joined()
    }
}

/// Mirror of `mahi_contracts::data::TaskStatus`.
public enum TaskStatus: String, Codable, Hashable, Sendable, CaseIterable {
    case pending
    case running
    case awaitingApproval
    case completed
    case cancelled
    case failed

    public var displayName: String {
        switch self {
        case .pending: return "Pending"
        case .running: return "Running"
        case .awaitingApproval: return "Awaiting Approval"
        case .completed: return "Completed"
        case .cancelled: return "Cancelled"
        case .failed: return "Failed"
        }
    }
}

/// A unit of work on the cross-device task queue.
/// Mirror of `mahi_contracts::data::TaskItem` (UI-relevant fields).
public struct TaskItem: Identifiable, Hashable, Sendable {
    public let id: UUID
    public let originDevice: UUID
    public var status: TaskStatus
    public var summary: String
    public let capabilitiesRequired: [String]
    public let createdAt: Date
    public var updatedAt: Date

    public init(
        id: UUID = UUID(),
        originDevice: UUID,
        status: TaskStatus,
        summary: String,
        capabilitiesRequired: [String] = [],
        createdAt: Date = .now,
        updatedAt: Date = .now
    ) {
        self.id = id
        self.originDevice = originDevice
        self.status = status
        self.summary = summary
        self.capabilitiesRequired = capabilitiesRequired
        self.createdAt = createdAt
        self.updatedAt = updatedAt
    }
}
