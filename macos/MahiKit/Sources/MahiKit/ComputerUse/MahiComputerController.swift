import AppKit
import ApplicationServices
import CoreGraphics
import Foundation

/// The real macOS computer-use backend: CoreGraphics screen capture, Accessibility
/// tree reads, and CGEvent input synthesis, fronted by the two safety gates the
/// `ComputerControlling` contract requires:
///
///   • `ComputerUseSupervisor.gate()` runs before *every* action, so the user's
///     pause/kill switch takes effect between actions with nothing queued behind it.
///   • `SensitiveScreenGuard.assertSafe(for:)` runs before every capture and every
///     synthesized input, so Mahi never reads or touches login/payment screens.
///
/// Requirements at runtime (the build must NOT be App-Sandboxed — see
/// Mahi.entitlements):
///   • Screen Recording permission (TCC) for `screenshot()`.
///   • Accessibility permission (TCC) for `describeScreen()` and all input
///     synthesis (CGEvents posted to the HID tap are dropped without it).
///
/// All blocking C calls (CGWindowList capture, AX reads, CGEvent posting) run on
/// dedicated dispatch queues, never on the main actor and never on the cooperative
/// thread pool.
public final class MahiComputerController: ComputerControlling, @unchecked Sendable {
    private let supervisor: ComputerUseSupervisor
    private let screenGuard: SensitiveScreenGuard

    /// Serial queues for the blocking C APIs; input is serialized so synthesized
    /// event streams from concurrent tool calls cannot interleave.
    private let captureQueue = DispatchQueue(label: "ai.mahi.computer-use.capture", qos: .userInitiated)
    private let axQueue = DispatchQueue(label: "ai.mahi.computer-use.ax", qos: .userInitiated)
    private let inputQueue = DispatchQueue(label: "ai.mahi.computer-use.input", qos: .userInitiated)

    /// Designated initializer with explicit safety dependencies.
    public init(supervisor: ComputerUseSupervisor, screenGuard: SensitiveScreenGuard = SensitiveScreenGuard()) {
        self.supervisor = supervisor
        self.screenGuard = screenGuard
    }

    /// Convenience for app code: builds a fresh supervisor + guard. Main-actor
    /// because `ComputerUseSupervisor` (an ObservableObject) is main-actor bound.
    @MainActor
    public convenience init() {
        self.init(supervisor: ComputerUseSupervisor(), screenGuard: SensitiveScreenGuard())
    }

    /// Snapshot of the two TCC grants computer use depends on, for settings UI
    /// and onboarding. Does not prompt.
    public static func permissionsStatus() -> (screenRecording: Bool, accessibility: Bool) {
        (CGPreflightScreenCaptureAccess(), AXIsProcessTrusted())
    }

    // MARK: - ComputerControlling

    public func screenshot() async throws -> Screenshot {
        let kind = ComputerUseAction.Kind.screenshot
        try await supervisor.gate()
        try Self.ensureScreenRecordingPermission()
        try await assertScreenSafe(.capture, kind: kind, detail: "Capture the screen")
        let shot = try await runBlocking(on: captureQueue) { try Self.captureMainDisplayBlocking() }
        await record(kind, detail: "Captured \(shot.pixelWidth)×\(shot.pixelHeight) px @\(String(format: "%.1f", shot.scale))x", outcome: .performed)
        return shot
    }

    public func describeScreen() async throws -> [ScreenElement] {
        let kind = ComputerUseAction.Kind.describe
        try await supervisor.gate()
        try Self.ensureAccessibilityTrusted(promptIfNeeded: true)
        try await assertScreenSafe(.capture, kind: kind, detail: "Describe the frontmost app")
        guard let frontmost = NSWorkspace.shared.frontmostApplication else {
            await record(kind, detail: "No frontmost application", outcome: .performed)
            return []
        }
        let pid = frontmost.processIdentifier
        let appName = frontmost.localizedName ?? "the frontmost app"
        let elements = try await runBlocking(on: axQueue) { Self.collectScreenElements(pid: pid) }
        await record(kind, detail: "\(elements.count) elements in \(appName)", outcome: .performed)
        return elements
    }

    public func click(at point: CGPoint, button: MouseButton, clickCount: Int) async throws {
        let target = try Self.validatedPoint(point, label: "click")
        let count = min(max(clickCount, 1), 3)
        let kind = ComputerUseAction.Kind.click(point: target, button: button, count: count)
        let detail = "\(button.rawValue.capitalized) click ×\(count) at \(Self.describePoint(target))"
        try await authorizeInput(kind: kind, detail: detail)
        try await runInput { try Self.synthesizeClick(at: target, button: button, count: count) }
        await record(kind, detail: detail, outcome: .performed)
    }

    public func typeText(_ text: String) async throws {
        let kind = ComputerUseAction.Kind.type(preview: Self.preview(of: text))
        let detail = "Type \(text.count) character\(text.count == 1 ? "" : "s")"
        try await authorizeInput(kind: kind, detail: detail)
        guard !text.isEmpty else {
            await record(kind, detail: detail, outcome: .performed)
            return
        }
        try await runInput { try Self.synthesizeText(text) }
        await record(kind, detail: detail, outcome: .performed)
    }

    public func pressKey(_ combo: KeyCombo) async throws {
        let kind = ComputerUseAction.Kind.key(combo.description)
        let detail = "Press \(combo.description)"
        try await authorizeInput(kind: kind, detail: detail)
        guard let keyCode = Self.keyCodes[combo.key] else {
            throw ComputerUseError.inputSynthesisFailed("unknown key \u{201C}\(combo.key)\u{201D}")
        }
        let flags = Self.eventFlags(for: combo.modifiers)
        try await runInput { try Self.synthesizeKeyPress(keyCode: keyCode, flags: flags) }
        await record(kind, detail: detail, outcome: .performed)
    }

    public func scroll(at point: CGPoint, deltaX: Double, deltaY: Double) async throws {
        let target = try Self.validatedPoint(point, label: "scroll")
        let kind = ComputerUseAction.Kind.scroll(point: target)
        let dx = Self.clampedScrollDelta(deltaX)
        let dy = Self.clampedScrollDelta(deltaY)
        let detail = "Scroll (\(dx), \(dy)) px at \(Self.describePoint(target))"
        try await authorizeInput(kind: kind, detail: detail)
        try await runInput { try Self.synthesizeScroll(at: target, deltaX: dx, deltaY: dy) }
        await record(kind, detail: detail, outcome: .performed)
    }

    public func drag(from: CGPoint, to: CGPoint) async throws {
        let origin = try Self.validatedPoint(from, label: "drag origin")
        let destination = try Self.validatedPoint(to, label: "drag destination")
        let kind = ComputerUseAction.Kind.drag(from: origin, to: destination)
        let detail = "Drag \(Self.describePoint(origin)) → \(Self.describePoint(destination))"
        try await authorizeInput(kind: kind, detail: detail)
        try await runInput { try Self.synthesizeDrag(from: origin, to: destination) }
        await record(kind, detail: detail, outcome: .performed)
    }

    // MARK: - Gating

    /// The common pre-action checks for input synthesis: kill/pause gate, the
    /// Accessibility grant (HID-tap events are silently dropped without it), and
    /// the sensitive-screen hard rule.
    private func authorizeInput(kind: ComputerUseAction.Kind, detail: String) async throws {
        try await supervisor.gate()
        try Self.ensureAccessibilityTrusted(promptIfNeeded: false)
        try await assertScreenSafe(.act, kind: kind, detail: detail)
    }

    /// Runs the sensitive-screen guard; a block is recorded in the supervisor's
    /// action log (the overlay shows it) and rethrown.
    private func assertScreenSafe(
        _ actionClass: SensitiveScreenGuard.ActionClass,
        kind: ComputerUseAction.Kind,
        detail: String
    ) async throws {
        do {
            try await screenGuard.assertSafe(for: actionClass)
        } catch {
            if let computerUseError = error as? ComputerUseError,
               case .sensitiveScreenBlocked(let reason) = computerUseError {
                await record(kind, detail: detail, outcome: .blocked(reason: reason))
            }
            throw error
        }
    }

    private func record(_ kind: ComputerUseAction.Kind, detail: String, outcome: ComputerUseAction.Outcome) async {
        await supervisor.record(ComputerUseAction(kind: kind, detail: detail, outcome: outcome))
    }

    // MARK: - Queue plumbing

    private func runBlocking<T: Sendable>(
        on queue: DispatchQueue,
        _ work: @escaping @Sendable () throws -> T
    ) async throws -> T {
        try await withCheckedThrowingContinuation { (continuation: CheckedContinuation<T, Error>) in
            queue.async {
                continuation.resume(with: Result(catching: work))
            }
        }
    }

    private func runInput(_ work: @escaping @Sendable () throws -> Void) async throws {
        try await runBlocking(on: inputQueue, work)
    }

    // MARK: - Permissions

    private static func ensureScreenRecordingPermission() throws {
        guard !CGPreflightScreenCaptureAccess() else { return }
        // Triggers the system prompt / System Settings deep link the first time;
        // the grant only applies after the user flips the toggle, so fail this call.
        _ = CGRequestScreenCaptureAccess()
        throw ComputerUseError.permissionMissing("Screen Recording")
    }

    private static func ensureAccessibilityTrusted(promptIfNeeded: Bool) throws {
        if AXIsProcessTrusted() { return }
        if promptIfNeeded {
            let options = [kAXTrustedCheckOptionPrompt.takeUnretainedValue() as String: true] as CFDictionary
            if AXIsProcessTrustedWithOptions(options) { return }
        }
        throw ComputerUseError.permissionMissing("Accessibility")
    }

    // MARK: - Screen capture (blocking; runs on captureQueue)

    private static func captureMainDisplayBlocking() throws -> Screenshot {
        if let image = CGWindowListCreateImage(
            .infinite,
            .optionOnScreenOnly,
            kCGNullWindowID,
            [.bestResolution, .boundsIgnoreFraming]
        ) {
            let rep = NSBitmapImageRep(cgImage: image)
            guard let png = rep.representation(using: .png, properties: [:]) else {
                throw ComputerUseError.captureFailed("PNG encoding failed")
            }
            return Screenshot(
                pngData: png,
                pixelWidth: image.width,
                pixelHeight: image.height,
                scale: displayScale(forPixelWidth: image.width)
            )
        }

        // CGWindowListCreateImage can return nil (permission revoked mid-flight,
        // display asleep); fall back to the system capture tool.
        let data = try captureWithScreencaptureTool()
        guard let rep = NSBitmapImageRep(data: data) else {
            throw ComputerUseError.captureFailed("could not decode the fallback PNG")
        }
        return Screenshot(
            pngData: data,
            pixelWidth: rep.pixelsWide,
            pixelHeight: rep.pixelsHigh,
            scale: displayScale(forPixelWidth: rep.pixelsWide)
        )
    }

    private static func captureWithScreencaptureTool() throws -> Data {
        let fileURL = FileManager.default.temporaryDirectory
            .appendingPathComponent("mahi-capture-\(UUID().uuidString).png")
        defer { try? FileManager.default.removeItem(at: fileURL) }

        let process = Process()
        process.executableURL = URL(fileURLWithPath: "/usr/sbin/screencapture")
        process.arguments = ["-x", "-t", "png", fileURL.path]
        process.standardOutput = FileHandle.nullDevice
        process.standardError = FileHandle.nullDevice
        do {
            try process.run()
        } catch {
            throw ComputerUseError.captureFailed("screencapture could not launch: \(error.localizedDescription)")
        }
        process.waitUntilExit()
        guard process.terminationStatus == 0 else {
            throw ComputerUseError.captureFailed("screencapture exited with status \(process.terminationStatus)")
        }
        guard let data = try? Data(contentsOf: fileURL), !data.isEmpty else {
            throw ComputerUseError.captureFailed("screencapture produced no image file")
        }
        return data
    }

    /// Points → pixels factor, derived from the captured width against the main
    /// screen's point width. (With `CGRect.infinite` the capture spans all
    /// displays; on the common single-display setup this equals the backing
    /// scale factor.)
    private static func displayScale(forPixelWidth pixelWidth: Int) -> Double {
        let pointWidth = NSScreen.main.map { Double($0.frame.width) }
            ?? Double(CGDisplayBounds(CGMainDisplayID()).width)
        guard pointWidth > 0, pixelWidth > 0 else { return 1 }
        return Double(pixelWidth) / pointWidth
    }

    // MARK: - Accessibility tree (blocking; runs on axQueue)

    private static let maxTreeDepth = 4
    private static let maxVisitedElements = 200
    private static let maxValueLength = 256

    /// `kAXFrameAttribute` ("AXFrame") is not exported by the HIServices headers
    /// even though virtually every element answers it; ask by literal name and
    /// fall back to position + size, which are public attributes.
    private static let frameAttributeName = "AXFrame"

    private static let actionableRoles: Set<String> = [
        "AXButton", "AXLink", "AXTextField", "AXTextArea",
        "AXMenuItem", "AXCheckBox", "AXRadioButton", "AXPopUpButton",
    ]

    private static func collectScreenElements(pid: pid_t) -> [ScreenElement] {
        let root = AXUIElementCreateApplication(pid)
        var collected: [ScreenElement] = []
        var visited = 0

        func visit(_ element: AXUIElement, path: String, depth: Int) {
            guard visited < maxVisitedElements else { return }
            visited += 1

            let role = copyString(of: element, attribute: kAXRoleAttribute) ?? "AXUnknown"
            if let frame = elementFrame(of: element) {
                collected.append(ScreenElement(
                    id: stableID(forPath: path, role: role),
                    role: role,
                    title: copyString(of: element, attribute: kAXTitleAttribute),
                    value: valueDescription(of: element),
                    frame: frame,
                    isActionable: actionableRoles.contains(role)
                ))
            }

            guard depth < maxTreeDepth else { return }
            for (index, child) in children(of: element).enumerated() {
                guard visited < maxVisitedElements else { return }
                visit(child, path: "\(path)/\(index)", depth: depth + 1)
            }
        }

        visit(root, path: "\(pid)", depth: 0)
        return collected
    }

    private static func copyString(of element: AXUIElement, attribute: String) -> String? {
        var value: CFTypeRef?
        guard AXUIElementCopyAttributeValue(element, attribute as CFString, &value) == .success else {
            return nil
        }
        return value as? String
    }

    private static func valueDescription(of element: AXUIElement) -> String? {
        var raw: CFTypeRef?
        guard AXUIElementCopyAttributeValue(element, kAXValueAttribute as CFString, &raw) == .success,
              let raw else {
            return nil
        }
        let text: String?
        if let string = raw as? String {
            text = string
        } else if let number = raw as? NSNumber {
            text = number.stringValue
        } else {
            text = nil
        }
        guard let text, !text.isEmpty else { return nil }
        guard text.count > maxValueLength else { return text }
        return String(text.prefix(maxValueLength)) + "…"
    }

    private static func children(of element: AXUIElement) -> [AXUIElement] {
        var value: CFTypeRef?
        guard AXUIElementCopyAttributeValue(element, kAXChildrenAttribute as CFString, &value) == .success,
              let value, CFGetTypeID(value) == CFArrayGetTypeID() else {
            return []
        }
        return (value as! [AnyObject]).compactMap { child -> AXUIElement? in
            guard CFGetTypeID(child) == AXUIElementGetTypeID() else { return nil }
            // Safe: the type id was checked above.
            return (child as! AXUIElement)
        }
    }

    private static func elementFrame(of element: AXUIElement) -> CGRect? {
        if let rect = geometryValue(of: element, attribute: frameAttributeName, type: .cgRect, initial: CGRect.zero) {
            return rect
        }
        guard
            let origin = geometryValue(of: element, attribute: kAXPositionAttribute, type: .cgPoint, initial: CGPoint.zero),
            let size = geometryValue(of: element, attribute: kAXSizeAttribute, type: .cgSize, initial: CGSize.zero)
        else {
            return nil
        }
        return CGRect(origin: origin, size: size)
    }

    /// Decodes an `AXValue`-wrapped CG struct (`CGRect`/`CGPoint`/`CGSize`).
    private static func geometryValue<T>(
        of element: AXUIElement,
        attribute: String,
        type: AXValueType,
        initial: T
    ) -> T? {
        var raw: CFTypeRef?
        guard AXUIElementCopyAttributeValue(element, attribute as CFString, &raw) == .success,
              let raw, CFGetTypeID(raw) == AXValueGetTypeID() else {
            return nil
        }
        // Safe: the type id was checked above.
        let axValue = raw as! AXValue
        var result = initial
        guard AXValueGetType(axValue) == type, AXValueGetValue(axValue, type, &result) else {
            return nil
        }
        return result
    }

    /// Deterministic FNV-1a hash of the AX path, stable within a snapshot.
    private static func stableID(forPath path: String, role: String) -> String {
        var hash: UInt64 = 0xcbf2_9ce4_8422_2325
        for byte in "\(path)#\(role)".utf8 {
            hash ^= UInt64(byte)
            hash = hash &* 0x0000_0100_0000_01b3
        }
        return String(format: "ax-%016llx", hash)
    }

    // MARK: - Input synthesis (blocking; runs on inputQueue)

    private static let eventSettleMicroseconds: useconds_t = 10_000
    private static let multiClickGapMicroseconds: useconds_t = 60_000
    private static let dragStepMicroseconds: useconds_t = 8_000
    private static let typeChunkGapMicroseconds: useconds_t = 3_000
    private static let maxTypeChunk = 20
    private static let dragSteps = 12

    private static func mouseEventSpec(
        for button: MouseButton
    ) -> (down: CGEventType, up: CGEventType, cgButton: CGMouseButton) {
        switch button {
        case .left: return (.leftMouseDown, .leftMouseUp, .left)
        case .right: return (.rightMouseDown, .rightMouseUp, .right)
        case .middle: return (.otherMouseDown, .otherMouseUp, .center)
        }
    }

    private static func synthesizeClick(at point: CGPoint, button: MouseButton, count: Int) throws {
        let spec = mouseEventSpec(for: button)
        for clickIndex in 1...count {
            guard
                let down = CGEvent(
                    mouseEventSource: nil, mouseType: spec.down,
                    mouseCursorPosition: point, mouseButton: spec.cgButton
                ),
                let up = CGEvent(
                    mouseEventSource: nil, mouseType: spec.up,
                    mouseCursorPosition: point, mouseButton: spec.cgButton
                )
            else {
                throw ComputerUseError.inputSynthesisFailed("could not create \(button.rawValue) mouse events")
            }
            // AppKit derives NSEvent.clickCount from this field; the second click
            // of a double-click carries state 2, and so on.
            down.setIntegerValueField(.mouseEventClickState, value: Int64(clickIndex))
            up.setIntegerValueField(.mouseEventClickState, value: Int64(clickIndex))
            down.post(tap: .cghidEventTap)
            usleep(eventSettleMicroseconds)
            up.post(tap: .cghidEventTap)
            if clickIndex < count { usleep(multiClickGapMicroseconds) }
        }
    }

    private static func synthesizeDrag(from origin: CGPoint, to destination: CGPoint) throws {
        guard
            let move = CGEvent(
                mouseEventSource: nil, mouseType: .mouseMoved,
                mouseCursorPosition: origin, mouseButton: .left
            ),
            let down = CGEvent(
                mouseEventSource: nil, mouseType: .leftMouseDown,
                mouseCursorPosition: origin, mouseButton: .left
            ),
            let up = CGEvent(
                mouseEventSource: nil, mouseType: .leftMouseUp,
                mouseCursorPosition: destination, mouseButton: .left
            )
        else {
            throw ComputerUseError.inputSynthesisFailed("could not create drag events")
        }
        move.post(tap: .cghidEventTap)
        usleep(eventSettleMicroseconds)
        down.post(tap: .cghidEventTap)
        usleep(dragStepMicroseconds)
        for step in 1...dragSteps {
            let progress = CGFloat(step) / CGFloat(dragSteps)
            let waypoint = CGPoint(
                x: origin.x + (destination.x - origin.x) * progress,
                y: origin.y + (destination.y - origin.y) * progress
            )
            guard let dragged = CGEvent(
                mouseEventSource: nil, mouseType: .leftMouseDragged,
                mouseCursorPosition: waypoint, mouseButton: .left
            ) else {
                throw ComputerUseError.inputSynthesisFailed("could not create drag step event")
            }
            dragged.post(tap: .cghidEventTap)
            usleep(dragStepMicroseconds)
        }
        up.post(tap: .cghidEventTap)
    }

    private static func synthesizeScroll(at point: CGPoint, deltaX: Int32, deltaY: Int32) throws {
        // Park the cursor on the target so the scroll lands in the right view.
        guard let move = CGEvent(
            mouseEventSource: nil, mouseType: .mouseMoved,
            mouseCursorPosition: point, mouseButton: .left
        ) else {
            throw ComputerUseError.inputSynthesisFailed("could not create cursor move event")
        }
        move.post(tap: .cghidEventTap)
        usleep(eventSettleMicroseconds)
        guard let scroll = CGEvent(
            scrollWheelEvent2Source: nil,
            units: .pixel,
            wheelCount: 2,
            wheel1: deltaY,
            wheel2: deltaX,
            wheel3: 0
        ) else {
            throw ComputerUseError.inputSynthesisFailed("could not create scroll event")
        }
        scroll.post(tap: .cghidEventTap)
    }

    private static func synthesizeText(_ text: String) throws {
        let units = Array(text.utf16)
        var start = 0
        while start < units.count {
            var end = min(start + maxTypeChunk, units.count)
            // Never split a surrogate pair across chunks.
            if end < units.count, end - start > 1, UTF16.isLeadSurrogate(units[end - 1]) {
                end -= 1
            }
            try postUnicodeChunk(Array(units[start..<end]))
            start = end
        }
    }

    private static func postUnicodeChunk(_ chunk: [UniChar]) throws {
        guard
            let down = CGEvent(keyboardEventSource: nil, virtualKey: 0, keyDown: true),
            let up = CGEvent(keyboardEventSource: nil, virtualKey: 0, keyDown: false)
        else {
            throw ComputerUseError.inputSynthesisFailed("could not create keyboard events for text")
        }
        chunk.withUnsafeBufferPointer { buffer in
            down.keyboardSetUnicodeString(stringLength: buffer.count, unicodeString: buffer.baseAddress)
            up.keyboardSetUnicodeString(stringLength: buffer.count, unicodeString: buffer.baseAddress)
        }
        down.post(tap: .cghidEventTap)
        up.post(tap: .cghidEventTap)
        usleep(typeChunkGapMicroseconds)
    }

    private static func synthesizeKeyPress(keyCode: CGKeyCode, flags: CGEventFlags) throws {
        guard
            let down = CGEvent(keyboardEventSource: nil, virtualKey: keyCode, keyDown: true),
            let up = CGEvent(keyboardEventSource: nil, virtualKey: keyCode, keyDown: false)
        else {
            throw ComputerUseError.inputSynthesisFailed("could not create keyboard events")
        }
        down.flags = flags
        up.flags = flags
        down.post(tap: .cghidEventTap)
        usleep(eventSettleMicroseconds)
        up.post(tap: .cghidEventTap)
    }

    internal static func eventFlags(for modifiers: Set<KeyCombo.Modifier>) -> CGEventFlags {
        var flags: CGEventFlags = []
        if modifiers.contains(.command) { flags.insert(.maskCommand) }
        if modifiers.contains(.option) { flags.insert(.maskAlternate) }
        if modifiers.contains(.control) { flags.insert(.maskControl) }
        if modifiers.contains(.shift) { flags.insert(.maskShift) }
        if modifiers.contains(.function) { flags.insert(.maskSecondaryFn) }
        return flags
    }

    /// ANSI-layout virtual key codes (Carbon `kVK_*` values, hardcoded so we do
    /// not need to import Carbon). Keys match `KeyCombo.key`: single characters
    /// or lower-cased names. `internal` for the unit tests.
    internal static let keyCodes: [String: CGKeyCode] = [
        // Letters (ANSI)
        "a": 0x00, "b": 0x0B, "c": 0x08, "d": 0x02, "e": 0x0E, "f": 0x03,
        "g": 0x05, "h": 0x04, "i": 0x22, "j": 0x26, "k": 0x28, "l": 0x25,
        "m": 0x2E, "n": 0x2D, "o": 0x1F, "p": 0x23, "q": 0x0C, "r": 0x0F,
        "s": 0x01, "t": 0x11, "u": 0x20, "v": 0x09, "w": 0x0D, "x": 0x07,
        "y": 0x10, "z": 0x06,
        // Digits (ANSI top row)
        "0": 0x1D, "1": 0x12, "2": 0x13, "3": 0x14, "4": 0x15,
        "5": 0x17, "6": 0x16, "7": 0x1A, "8": 0x1C, "9": 0x19,
        // Named keys
        "return": 0x24, "enter": 0x24, "tab": 0x30, "space": 0x31,
        "escape": 0x35, "esc": 0x35,
        "delete": 0x33, "backspace": 0x33, "forwarddelete": 0x75,
        "left": 0x7B, "right": 0x7C, "down": 0x7D, "up": 0x7E,
        "home": 0x73, "end": 0x77, "pageup": 0x74, "pagedown": 0x79,
        // Function row
        "f1": 0x7A, "f2": 0x78, "f3": 0x63, "f4": 0x76, "f5": 0x60, "f6": 0x61,
        "f7": 0x62, "f8": 0x64, "f9": 0x65, "f10": 0x6D, "f11": 0x67, "f12": 0x6F,
        // Punctuation (ANSI), with spelled-out aliases
        "-": 0x1B, "minus": 0x1B, "=": 0x18, "equals": 0x18,
        "[": 0x21, "leftbracket": 0x21, "]": 0x1E, "rightbracket": 0x1E,
        "\\": 0x2A, "backslash": 0x2A, ";": 0x29, "semicolon": 0x29,
        "'": 0x27, "quote": 0x27, ",": 0x2B, "comma": 0x2B,
        ".": 0x2F, "period": 0x2F, "/": 0x2C, "slash": 0x2C,
        "`": 0x32, "grave": 0x32,
    ]

    // MARK: - Small helpers

    private static func validatedPoint(_ point: CGPoint, label: String) throws -> CGPoint {
        guard point.x.isFinite, point.y.isFinite else {
            throw ComputerUseError.inputSynthesisFailed("\(label) coordinates are not finite")
        }
        return point
    }

    private static func describePoint(_ point: CGPoint) -> String {
        String(format: "(%.0f, %.0f)", point.x, point.y)
    }

    private static func clampedScrollDelta(_ delta: Double) -> Int32 {
        guard delta.isFinite else { return 0 }
        return Int32(max(-30_000, min(30_000, delta.rounded())))
    }

    private static func preview(of text: String) -> String {
        text.count > 40 ? String(text.prefix(40)) + "…" : text
    }
}
