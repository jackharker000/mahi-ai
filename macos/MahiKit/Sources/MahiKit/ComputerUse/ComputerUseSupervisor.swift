import Foundation

/// One observed computer-use action, rendered live in `ComputerUseOverlay`.
public struct ComputerUseAction: Identifiable, Hashable, Sendable {
    public enum Kind: Hashable, Sendable {
        case screenshot
        case describe
        case click(point: CGPoint, button: MouseButton, count: Int)
        case type(preview: String)
        case key(String)
        case scroll(point: CGPoint)
        case drag(from: CGPoint, to: CGPoint)
    }

    public enum Outcome: Hashable, Sendable {
        case performed
        case blocked(reason: String)
    }

    public let id: UUID
    public let kind: Kind
    public let detail: String
    public let outcome: Outcome
    public let timestamp: Date

    public init(kind: Kind, detail: String, outcome: Outcome, timestamp: Date = .now) {
        self.id = UUID()
        self.kind = kind
        self.detail = detail
        self.outcome = outcome
        self.timestamp = timestamp
    }

    public var symbolName: String {
        switch kind {
        case .screenshot: return "camera.viewfinder"
        case .describe: return "rectangle.and.text.magnifyingglass"
        case .click: return "cursorarrow.click"
        case .type: return "keyboard"
        case .key: return "command"
        case .scroll: return "arrow.up.and.down"
        case .drag: return "hand.draw"
        }
    }
}

/// The user's safety control over computer use: a pause gate, a kill switch, and the
/// live action log. The controller calls `gate()` before *every* action, so pause
/// and kill take effect between actions with nothing queued behind them.
///
/// Lives on the main actor so SwiftUI (`ComputerUseOverlay`, the menu-bar panel)
/// can bind to it directly; controller calls hop on briefly per action.
@MainActor
public final class ComputerUseSupervisor: ObservableObject {
    public enum State: String, Sendable {
        /// No computer-use activity this session.
        case idle
        /// The agent is actively driving the computer.
        case active
        /// The user paused; actions wait until resume or kill.
        case paused
        /// The kill switch was hit; every action throws until `reset()`.
        case killed
    }

    @Published public private(set) var state: State = .idle
    @Published public private(set) var actions: [ComputerUseAction] = []

    private var resumeWaiters: [CheckedContinuation<Void, Never>] = []
    private let maxLogEntries = 500

    public init() {}

    // MARK: Gate (called by the controller before every action)

    /// Returns when it is safe to act. Throws if the kill switch is (or becomes)
    /// engaged; suspends while paused.
    public func gate() async throws {
        while true {
            switch state {
            case .killed:
                throw ComputerUseError.killSwitchEngaged
            case .paused:
                await withCheckedContinuation { resumeWaiters.append($0) }
                // Loop: re-check state — the wake may have been kill, not resume.
            case .idle:
                state = .active
                return
            case .active:
                return
            }
        }
    }

    public func record(_ action: ComputerUseAction) {
        actions.append(action)
        if actions.count > maxLogEntries {
            actions.removeFirst(actions.count - maxLogEntries)
        }
    }

    // MARK: User controls

    public func pause() {
        guard state == .active else { return }
        state = .paused
    }

    public func resume() {
        guard state == .paused else { return }
        state = .active
        wakeWaiters()
    }

    /// The kill switch. Always visible in the overlay and the menu-bar panel;
    /// takes effect before the next action and stays engaged until `reset()`.
    public func kill() {
        state = .killed
        wakeWaiters()
        record(ComputerUseAction(
            kind: .key("KILL"),
            detail: "Kill switch engaged by user",
            outcome: .blocked(reason: "kill switch")
        ))
    }

    /// Re-arm after a kill (e.g. when the user starts a fresh task).
    public func reset() {
        state = .idle
        actions.removeAll()
    }

    public var isPaused: Bool { state == .paused }
    public var isKilled: Bool { state == .killed }
    public var isEngaged: Bool { state != .idle }

    private func wakeWaiters() {
        let waiters = resumeWaiters
        resumeWaiters.removeAll()
        waiters.forEach { $0.resume() }
    }
}
