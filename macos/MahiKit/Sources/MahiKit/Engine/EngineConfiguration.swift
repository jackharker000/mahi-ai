import Foundation

/// Everything needed to assemble the engine: store location, providers, routing pin.
/// Fed to `MahiEngineBuilder` by `FfiEngine`, or used as preview seed data by the mock.
public struct EngineConfiguration: Sendable {
    /// Directory holding the encrypted local store (SQLite + SQLCipher behind the FFI).
    public var dataDirectory: URL
    /// Stable identifier for this device (DataStore `device_id`).
    public var deviceID: UUID
    /// Hosted provider credentials; `nil` disables the hosted (mode D) provider.
    public var hostedAPIKey: String?
    /// Hosted model identifier, e.g. "claude-sonnet-4-5".
    public var hostedModelID: String?
    /// On-device model identifier to load at startup, if any.
    public var onDeviceModelID: String?
    /// Optional user pin; the router never silently overrides it (DECISIONS.md D1).
    public var pinnedMode: ComputeMode?

    public init(
        dataDirectory: URL,
        deviceID: UUID,
        hostedAPIKey: String? = nil,
        hostedModelID: String? = nil,
        onDeviceModelID: String? = nil,
        pinnedMode: ComputeMode? = nil
    ) {
        self.dataDirectory = dataDirectory
        self.deviceID = deviceID
        self.hostedAPIKey = hostedAPIKey
        self.hostedModelID = hostedModelID
        self.onDeviceModelID = onDeviceModelID
        self.pinnedMode = pinnedMode
    }

    /// Default configuration rooted in `~/Library/Application Support/Mahi`.
    /// The device id is minted once and persisted alongside the store.
    public static func standard() -> EngineConfiguration {
        let appSupport = FileManager.default.urls(
            for: .applicationSupportDirectory, in: .userDomainMask
        ).first ?? FileManager.default.temporaryDirectory
        let root = appSupport.appendingPathComponent("Mahi", isDirectory: true)
        try? FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        return EngineConfiguration(dataDirectory: root, deviceID: persistentDeviceID(in: root))
    }

    private static func persistentDeviceID(in directory: URL) -> UUID {
        let marker = directory.appendingPathComponent("device-id")
        if let raw = try? String(contentsOf: marker, encoding: .utf8),
           let existing = UUID(uuidString: raw.trimmingCharacters(in: .whitespacesAndNewlines)) {
            return existing
        }
        let fresh = UUID()
        try? fresh.uuidString.write(to: marker, atomically: true, encoding: .utf8)
        return fresh
    }
}

/// Builds the best available engine: the real Rust core when its bindings are
/// linked, otherwise the preview mock (so the app always runs).
public enum EngineFactory {
    public struct Result {
        public let engine: any MahiEngineProtocol
        /// False when running on `PreviewMockEngine`; the UI shows a banner.
        public let isLive: Bool
    }

    public static func makeEngine(
        configuration: EngineConfiguration,
        computerController: any ComputerControlling
    ) -> Result {
        #if canImport(Mahi)
        do {
            let engine = try FfiEngine(
                configuration: configuration,
                computerController: computerController
            )
            return Result(engine: engine, isLive: true)
        } catch {
            // Fall through to the mock; the UI surfaces the failure.
            return Result(engine: PreviewMockEngine(), isLive: false)
        }
        #else
        return Result(engine: PreviewMockEngine(), isLive: false)
        #endif
    }
}
