import AppKit
import ApplicationServices
import Foundation

/// Hard-rule enforcement for computer use: Mahi never acts on (or captures) screens
/// that look like login, payment, or other secure contexts. This is the Swift-side
/// half of the "approval-wall" defense described in
/// docs/backend/domains/02-tooling-integrations.md — detection runs *before* every
/// action, on-device, and a block is non-overridable from the agent's side.
///
/// Signals checked, cheapest first:
///   1. Frontmost app bundle id against a denylist (password managers, Keychain).
///   2. The focused AX element: secure text fields (`AXSecureTextField` subrole).
///   3. Frontmost window title / focused web area URL against keyword patterns.
public final class SensitiveScreenGuard: @unchecked Sendable {
    /// What the agent is trying to do; reads are blocked too (screenshots of a
    /// password field leak just as badly as typing into one).
    public enum ActionClass: Sendable {
        case capture
        case act
    }

    public struct Verdict: Sendable {
        public let allowed: Bool
        public let reason: String?

        public static let allow = Verdict(allowed: true, reason: nil)
        public static func block(_ reason: String) -> Verdict {
            Verdict(allowed: false, reason: reason)
        }
    }

    /// Bundle ids whose windows are always off-limits.
    private let deniedBundleIDs: Set<String> = [
        "com.apple.keychainaccess",
        "com.apple.Passwords",
        "com.1password.1password",
        "com.agilebits.onepassword7",
        "com.bitwarden.desktop",
        "com.lastpass.LastPass",
        "org.keepassxc.keepassxc",
    ]

    /// Lower-cased substrings that mark a window title or URL as sensitive.
    private let sensitiveKeywords: [String] = [
        "password", "passcode", "passphrase",
        "sign in", "sign-in", "signin", "log in", "log-in", "login",
        "two-factor", "2fa", "one-time code", "verification code", "authenticator",
        "checkout", "payment", "billing", "credit card", "card number", "cvv", "cvc",
        "bank", "wire transfer", "iban", "routing number",
    ]

    /// Dedicated serial queue for the blocking AX C calls.
    private let axQueue = DispatchQueue(label: "ai.mahi.sensitive-screen-guard", qos: .userInitiated)

    public init() {}

    /// Throws `ComputerUseError.sensitiveScreenBlocked` unless the frontmost
    /// context is safe for `action`.
    public func assertSafe(for action: ActionClass) async throws {
        let verdict = await evaluateFrontmost(for: action)
        if !verdict.allowed {
            throw ComputerUseError.sensitiveScreenBlocked(
                reason: verdict.reason ?? "sensitive screen"
            )
        }
    }

    /// Evaluate the frontmost app/window/focused element.
    public func evaluateFrontmost(for action: ActionClass) async -> Verdict {
        let frontmost = NSWorkspace.shared.frontmostApplication
        if let bundleID = frontmost?.bundleIdentifier,
           deniedBundleIDs.contains(bundleID) {
            return .block("the frontmost app (\(bundleID)) handles credentials")
        }

        guard let pid = frontmost?.processIdentifier else {
            return .allow
        }

        return await withCheckedContinuation { continuation in
            axQueue.async { [sensitiveKeywords] in
                continuation.resume(
                    returning: Self.evaluateViaAccessibility(
                        pid: pid, keywords: sensitiveKeywords
                    )
                )
            }
        }
    }

    // MARK: - AX checks (blocking; runs on axQueue)

    private static func evaluateViaAccessibility(
        pid: pid_t, keywords: [String]
    ) -> Verdict {
        // Without Accessibility trust we cannot inspect — fail closed for safety:
        // the controller will separately raise a permissionMissing error with
        // guidance, so the user sees why nothing happens.
        guard AXIsProcessTrusted() else {
            return .block("Accessibility permission is missing, so the screen cannot be verified safe")
        }

        let app = AXUIElementCreateApplication(pid)

        // 1. Focused element: secure text field?
        if let focused = copyElement(of: app, attribute: kAXFocusedUIElementAttribute) {
            let subrole = copyString(of: focused, attribute: kAXSubroleAttribute)
            if subrole == "AXSecureTextField" {
                return .block("a secure (password) text field has keyboard focus")
            }
        }

        // 2. Focused window title.
        if let window = copyElement(of: app, attribute: kAXFocusedWindowAttribute),
           let title = copyString(of: window, attribute: kAXTitleAttribute)?.lowercased() {
            if let hit = keywords.first(where: { title.contains($0) }) {
                return .block("the active window title mentions \u{201C}\(hit)\u{201D}")
            }
        }

        // 3. Focused browser document URL (AXDocument on the focused window, where
        //    exposed — Safari and Chromium-family browsers publish it).
        if let window = copyElement(of: app, attribute: kAXFocusedWindowAttribute),
           let document = copyString(of: window, attribute: kAXDocumentAttribute)?.lowercased() {
            if let hit = keywords.first(where: { document.contains($0) }) {
                return .block("the page URL mentions \u{201C}\(hit)\u{201D}")
            }
        }

        return .allow
    }

    private static func copyElement(of element: AXUIElement, attribute: String) -> AXUIElement? {
        var value: CFTypeRef?
        let result = AXUIElementCopyAttributeValue(element, attribute as CFString, &value)
        guard result == .success, let value, CFGetTypeID(value) == AXUIElementGetTypeID() else {
            return nil
        }
        // Safe: the type id was checked above.
        return (value as! AXUIElement)
    }

    private static func copyString(of element: AXUIElement, attribute: String) -> String? {
        var value: CFTypeRef?
        let result = AXUIElementCopyAttributeValue(element, attribute as CFString, &value)
        guard result == .success else { return nil }
        return value as? String
    }
}
