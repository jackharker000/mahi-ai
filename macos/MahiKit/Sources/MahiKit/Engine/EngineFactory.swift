import Foundation

/// Picks the best available engine: the real Rust core when its artifacts were
/// linked into the build, otherwise the in-process preview engine.
public enum EngineFactory {
    /// `~/Library/Application Support/Mahi` — the app's encrypted store.
    public static func defaultDataDirectory() -> URL {
        let base = FileManager.default.urls(
            for: .applicationSupportDirectory, in: .userDomainMask
        ).first ?? FileManager.default.temporaryDirectory
        return base.appendingPathComponent("Mahi", isDirectory: true)
    }

    /// The engine the app should run on.
    public static func makeDefault() -> any MahiEngineProtocol {
        #if canImport(Mahi)
        if let engine = try? FfiBufferedEngine(dataDirectory: defaultDataDirectory()) {
            return engine
        }
        #endif
        return PreviewMockEngine(seeded: true)
    }

    /// Whether the real Rust core is linked into this binary.
    public static var isRealEngineAvailable: Bool {
        #if canImport(Mahi)
        return true
        #else
        return false
        #endif
    }
}
