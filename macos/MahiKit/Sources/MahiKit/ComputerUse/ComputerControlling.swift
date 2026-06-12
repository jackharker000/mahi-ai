// NOTE: `ComputerControlling` is the Swift-native shape of the FFI-exported
// `ComputerController` callback interface (docs/backend/phase-0/03-engine-facade.md,
// docs/backend/domains/02-tooling-integrations.md §3). The UniFFI-generated protocol
// (`import Mahi`, produced by `macos/scripts/build-xcframework.sh`) is bridged to this
// one by `FfiComputerControllerAdapter` in Engine/FfiEngine.swift, so all real
// screen-capture / Accessibility / CGEvent code lives here, independent of the
// generated bindings.
//
// The protocol carries two equivalent surfaces:
//
//   • Rich primitives (`screenshot() -> Screenshot`, `click(at:button:clickCount:)`,
//     `pressKey(_ combo: KeyCombo)`, …) — what `MahiComputerController` implements
//     and what Swift-side callers use.
//
//   • A simple-typed mirror (`screenshotPNG() -> Data`, `describeUI() ->
//     [UIElementInfo]`, `click(x:y:button:)`, `pressKey(combo: String)`, …) made of
//     `Double`/`String`/`Data` only, matching the operations the Rust core defines in
//     crates/mahi-tooling/src/computer.rs. These are defaulted in a protocol
//     extension to forward to the rich primitives, so the UniFFI
//     foreign-implemented-trait bridge can call them without knowing any
//     CoreGraphics types.

import CoreGraphics
import Foundation

/// A captured screenshot, as the engine's vision models consume it.
public struct Screenshot: Sendable {
    public let pngData: Data
    public let pixelWidth: Int
    public let pixelHeight: Int
    /// Backing-store scale (points → pixels), needed to map model coordinates back.
    public let scale: Double
    public let capturedAt: Date

    public init(pngData: Data, pixelWidth: Int, pixelHeight: Int, scale: Double, capturedAt: Date = .now) {
        self.pngData = pngData
        self.pixelWidth = pixelWidth
        self.pixelHeight = pixelHeight
        self.scale = scale
        self.capturedAt = capturedAt
    }
}

/// One UI element discovered via the Accessibility API, in screen points.
public struct ScreenElement: Identifiable, Hashable, Sendable {
    /// Stable-ish identifier for this snapshot (AX path hash).
    public let id: String
    /// AX role, e.g. "AXButton", "AXTextField", "AXLink".
    public let role: String
    public let title: String?
    public let value: String?
    public let frame: CGRect
    /// Whether the element looks like a click/type target.
    public let isActionable: Bool

    public init(id: String, role: String, title: String?, value: String?, frame: CGRect, isActionable: Bool) {
        self.id = id
        self.role = role
        self.title = title
        self.value = value
        self.frame = frame
        self.isActionable = isActionable
    }
}

public enum MouseButton: String, Hashable, Sendable {
    case left
    case right
    case middle
}

/// A parsed keyboard shortcut, e.g. "cmd+shift+p" or "return".
public struct KeyCombo: Hashable, Sendable, CustomStringConvertible {
    public enum Modifier: String, Hashable, Sendable, CaseIterable {
        case command
        case option
        case control
        case shift
        case function
    }

    public let modifiers: Set<Modifier>
    /// Lower-cased key name: a single character ("p", "1") or a named key
    /// ("return", "escape", "tab", "space", "delete", "up", "down", "left", "right").
    public let key: String

    public init(modifiers: Set<Modifier> = [], key: String) {
        self.modifiers = modifiers
        self.key = key.lowercased()
    }

    /// Parse the engine's wire form, e.g. "cmd+shift+p".
    public init(parsing raw: String) {
        var modifiers: Set<Modifier> = []
        var key = ""
        for part in raw.split(separator: "+").map({ $0.trimmingCharacters(in: .whitespaces).lowercased() }) {
            switch part {
            case "cmd", "command", "meta": modifiers.insert(.command)
            case "opt", "option", "alt": modifiers.insert(.option)
            case "ctrl", "control": modifiers.insert(.control)
            case "shift": modifiers.insert(.shift)
            case "fn", "function": modifiers.insert(.function)
            default: key = part
            }
        }
        self.init(modifiers: modifiers, key: key)
    }

    public var description: String {
        let names = Modifier.allCases.filter(modifiers.contains).map { mod -> String in
            switch mod {
            case .command: return "⌘"
            case .option: return "⌥"
            case .control: return "⌃"
            case .shift: return "⇧"
            case .function: return "fn"
            }
        }
        return (names + [key.uppercased()]).joined()
    }
}

/// Errors thrown by the computer-use surface; each maps to a tool error the agent sees.
public enum ComputerUseError: LocalizedError, Sendable {
    /// The user hit the kill switch; the whole turn must stop.
    case killSwitchEngaged
    /// The frontmost screen looks like a login/payment/secure context (hard rule).
    case sensitiveScreenBlocked(reason: String)
    /// macOS TCC permission (Screen Recording / Accessibility) is missing.
    case permissionMissing(String)
    /// ScreenCaptureKit could not produce a capture.
    case captureFailed(String)
    /// An event could not be synthesized (bad key name, CGEvent failure, …).
    case inputSynthesisFailed(String)

    public var errorDescription: String? {
        switch self {
        case .killSwitchEngaged:
            return "Computer use was stopped by the kill switch."
        case .sensitiveScreenBlocked(let reason):
            return "Blocked: the current screen looks sensitive (\(reason)). "
                + "Mahi never acts on login or payment screens."
        case .permissionMissing(let which):
            return "Missing macOS permission: \(which). Grant it in System Settings "
                + "→ Privacy & Security."
        case .captureFailed(let detail):
            return "Screen capture failed: \(detail)"
        case .inputSynthesisFailed(let detail):
            return "Could not synthesize input: \(detail)"
        }
    }
}

/// The platform hook the engine drives for computer use.
///
/// Every method must be safe to call from any thread/task, must consult the
/// `ComputerUseSupervisor` gate (pause/kill) before acting, and must refuse to act
/// on sensitive screens (`SensitiveScreenGuard`).
public protocol ComputerControlling: AnyObject, Sendable {
    /// Capture the main display.
    func screenshot() async throws -> Screenshot
    /// Describe the frontmost app's actionable UI elements via Accessibility.
    func describeScreen() async throws -> [ScreenElement]
    /// Click at a point in global screen coordinates (points, origin top-left).
    func click(at point: CGPoint, button: MouseButton, clickCount: Int) async throws
    /// Type literal text into the focused element.
    func typeText(_ text: String) async throws
    /// Press a keyboard shortcut.
    func pressKey(_ combo: KeyCombo) async throws
    /// Scroll by a delta at a point (pixels; positive deltaY scrolls content up).
    func scroll(at point: CGPoint, deltaX: Double, deltaY: Double) async throws
    /// Drag from one point to another with the left button.
    func drag(from: CGPoint, to: CGPoint) async throws
}
