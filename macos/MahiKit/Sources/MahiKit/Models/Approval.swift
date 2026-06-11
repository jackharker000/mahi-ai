import Foundation

/// The user's answer to an approval request.
public enum ApprovalDecision: String, Hashable, Sendable {
    case allowOnce
    case allowAlways
    case deny

    public var approves: Bool { self != .deny }
}

/// A rich, render-ready preview of the gated action, parsed from the
/// `AgentEvent.approvalRequired` summary. The engine formats summaries as
/// `"<kind>:<payload>"` for structured kinds (see `ApprovalPreview.parse`);
/// anything else renders as plain text.
public enum ApprovalPreview: Hashable, Sendable {
    /// A shell command the agent wants to run.
    case command(String)
    /// A unified diff the coding agent wants to apply.
    case diff(String)
    /// A screenshot of the screen region the agent wants to act on (PNG bytes).
    case screenshot(Data)
    /// Free-form description of the action.
    case plain(String)

    /// Heuristic parser for the approval summary string.
    ///
    /// Recognized structured forms (kept in sync with the engine's approval
    /// summaries as `mahi-tooling` grows them):
    ///   - `command:<shell command>`
    ///   - `diff:<unified diff>`
    ///   - `screenshot:<base64 PNG>`
    public static func parse(summary: String) -> ApprovalPreview {
        if let payload = payload(of: "command", in: summary) {
            return .command(payload)
        }
        if let payload = payload(of: "diff", in: summary) {
            return .diff(payload)
        }
        if let payload = payload(of: "screenshot", in: summary),
           let data = Data(base64Encoded: payload, options: .ignoreUnknownCharacters) {
            return .screenshot(data)
        }
        return .plain(summary)
    }

    private static func payload(of kind: String, in summary: String) -> String? {
        let prefix = kind + ":"
        guard summary.hasPrefix(prefix) else { return nil }
        let payload = String(summary.dropFirst(prefix.count))
            .trimmingCharacters(in: .whitespacesAndNewlines)
        return payload.isEmpty ? nil : payload
    }
}

/// A pending approval surfaced to the user, ready for `ApprovalSheet`.
public struct PendingApproval: Identifiable, Hashable, Sendable {
    public let id: UUID
    public let conversationID: UUID?
    public let summary: String
    public let destructiveLevel: DestructiveLevel
    public let preview: ApprovalPreview
    public let receivedAt: Date

    public init(
        id: UUID,
        conversationID: UUID? = nil,
        summary: String,
        destructiveLevel: DestructiveLevel = .high,
        receivedAt: Date = .now
    ) {
        self.id = id
        self.conversationID = conversationID
        self.summary = summary
        self.destructiveLevel = destructiveLevel
        self.preview = ApprovalPreview.parse(summary: summary)
        self.receivedAt = receivedAt
    }

    /// Critical actions never get a standing "always allow" grant.
    public var allowAlwaysPermitted: Bool { destructiveLevel < .critical }
}
