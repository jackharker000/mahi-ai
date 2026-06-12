import Foundation

/// The four compute modes Mahi can run an inference request in.
///
/// Swift mirror of `mahi_contracts::types::ComputeMode`. Local-first preference order
/// (DECISIONS.md D1): `macLan` > `macRemote` > `hosted`, with `onDevice` always
/// available and the privacy/offline default.
public enum ComputeMode: String, Codable, Hashable, Sendable, CaseIterable, Identifiable {
    /// Small model on the device itself, fully offline.
    case onDevice
    /// A paired Mac reached over the local network.
    case macLan
    /// A paired Mac reached remotely over a secure tunnel.
    case macRemote
    /// A hosted cloud model API.
    case hosted

    public var id: String { rawValue }

    /// Whether this mode requires any network at all.
    public var requiresNetwork: Bool { self != .onDevice }

    /// Short user-facing name, shown in the active-mode badge.
    public var displayName: String {
        switch self {
        case .onDevice: return "On-Device"
        case .macLan: return "Mac (LAN)"
        case .macRemote: return "Mac (Remote)"
        case .hosted: return "Hosted"
        }
    }

    /// SF Symbol used wherever the mode is rendered.
    public var symbolName: String {
        switch self {
        case .onDevice: return "iphone.gen3"
        case .macLan: return "desktopcomputer"
        case .macRemote: return "desktopcomputer.and.arrow.down"
        case .hosted: return "cloud"
        }
    }

    /// One-line explanation used in pickers and the pin menu.
    public var explanation: String {
        switch self {
        case .onDevice: return "Runs fully offline on this machine. Private and free."
        case .macLan: return "Uses this Mac as the brain over the local network."
        case .macRemote: return "Reaches this Mac from anywhere over a secure tunnel."
        case .hosted: return "Uses a hosted cloud model API."
        }
    }
}
