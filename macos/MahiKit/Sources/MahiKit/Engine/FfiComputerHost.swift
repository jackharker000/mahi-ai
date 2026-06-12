// Bridges the UniFFI `ComputerUseHost` callback (driven by the Rust agent loop)
// to the async `MahiComputerController`. Only compiled when the Rust core is
// linked (`canImport(Mahi)`).

#if canImport(Mahi)

import CoreGraphics
import Foundation
import Mahi

/// Adapts `MahiComputerController` (async, rich types) to the Rust-facing
/// `ComputerUseHost` (synchronous, plain types). Each Rust call runs the
/// controller's async work and blocks until it finishes — off the main actor,
/// and the controller uses its own dispatch queues, so this never deadlocks.
final class ComputerUseHostBridge: ComputerUseHost, @unchecked Sendable {
    private let controller: any ComputerControlling

    /// Build the real macOS controller. `@MainActor` because the controller's
    /// supervisor/guard are main-actor-bound at construction; the host methods
    /// below are nonisolated and run on Rust's callback threads.
    @MainActor
    init() {
        controller = MahiComputerController()
    }

    /// Inject a controller (tests / custom hosts).
    init(controller: any ComputerControlling) {
        self.controller = controller
    }

    func screenshot() throws -> ScreenshotFfi {
        let shot = try runSync { try await self.controller.screenshot() }
        return ScreenshotFfi(
            png: shot.pngData,
            width: UInt32(max(0, shot.pixelWidth)),
            height: UInt32(max(0, shot.pixelHeight))
        )
    }

    func describeUi() throws -> [UiElementFfi] {
        let elements = try runSync { try await self.controller.describeScreen() }
        return elements.map { element in
            UiElementFfi(
                role: element.role,
                label: element.title ?? element.value,
                x: Int32(element.frame.origin.x),
                y: Int32(element.frame.origin.y),
                width: UInt32(max(0, element.frame.size.width)),
                height: UInt32(max(0, element.frame.size.height)),
                focused: false
            )
        }
    }

    func click(x: Int32, y: Int32, button: String) throws {
        let point = CGPoint(x: Double(x), y: Double(y))
        let mouseButton = Self.button(from: button)
        try runSync {
            try await self.controller.click(at: point, button: mouseButton, clickCount: 1)
        }
    }

    func moveMouse(x: Int32, y: Int32) throws {
        // `ComputerControlling` has no standalone move and the agent's tool set
        // doesn't use one, so this is a no-op.
    }

    func typeText(text: String) throws {
        try runSync { try await self.controller.typeText(text) }
    }

    func scroll(dx: Int32, dy: Int32) throws {
        let point = Self.cursorLocation()
        try runSync {
            try await self.controller.scroll(at: point, deltaX: Double(dx), deltaY: Double(dy))
        }
    }

    func key(combo: String) throws {
        try runSync { try await self.controller.pressKey(KeyCombo(parsing: combo)) }
    }

    func isSensitiveContext() -> Bool {
        // The controller enforces the sensitive-screen guard per action (and
        // throws if blocked), so we don't pre-empt it here.
        false
    }

    // MARK: - async → sync bridge

    @discardableResult
    private func runSync<T>(_ operation: @escaping @Sendable () async throws -> T) throws -> T {
        let semaphore = DispatchSemaphore(value: 0)
        let box = ResultBox<T>()
        Task.detached {
            do {
                box.store(.success(try await operation()))
            } catch {
                box.store(.failure(error))
            }
            semaphore.signal()
        }
        semaphore.wait()
        switch box.take() {
        case .success(let value):
            return value
        case .failure(let error):
            throw MahiError.Engine(error.localizedDescription)
        case .none:
            throw MahiError.Engine("computer-use produced no result")
        }
    }

    private static func button(from raw: String) -> MouseButton {
        switch raw.lowercased() {
        case "right": return .right
        case "middle": return .middle
        default: return .left
        }
    }

    private static func cursorLocation() -> CGPoint {
        CGEvent(source: nil)?.location ?? .zero
    }
}

/// A thread-safe one-shot result box for the async→sync bridge. The lock is
/// only ever held for a trivial synchronous critical section (never across an
/// `await`), so a plain `NSLock` is correct here.
private final class ResultBox<T>: @unchecked Sendable {
    private let lock = NSLock()
    private var value: Result<T, Error>?

    func store(_ result: Result<T, Error>) {
        lock.lock()
        value = result
        lock.unlock()
    }

    func take() -> Result<T, Error>? {
        lock.lock()
        defer { lock.unlock() }
        return value
    }
}

#endif
