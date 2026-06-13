import Foundation

/// Lifecycle state of one catalog model on this machine.
/// Mirror of the planned `mahi_contracts::models::ModelState` (FFI-friendly:
/// plain associated values, no generics).
public enum ModelState: Hashable, Sendable {
    /// The weights are not on disk.
    case notInstalled
    /// The weights are being fetched. `progress` is 0...1.
    case downloading(progress: Double, bytesDownloaded: Int64)
    /// The weights are on disk and ready to be loaded.
    case installed
    /// The model is loaded into the managed runtime and serving.
    case active
}

/// One entry in the local-model catalog.
/// Mirror of the planned `mahi_contracts::models::CatalogModel`.
public struct CatalogModel: Identifiable, Hashable, Sendable {
    /// Stable catalog identifier, e.g. "qwen2.5-7b-instruct-q4_k_m".
    public let id: String
    public let displayName: String
    /// Model family for grouping/badging, e.g. "Qwen2.5", "Llama 3".
    public let family: String
    /// On-disk size of the weights once downloaded.
    public let sizeBytes: Int64
    /// Quantization label, e.g. "Q4_K_M".
    public let quantization: String
    /// Maximum context window in tokens.
    public let contextWindow: Int
    /// Whether the model reliably emits tool calls.
    public let toolCalling: Bool
    /// One-line, user-facing description.
    public let description: String
    /// Install/download/run state on this machine.
    public var state: ModelState

    public init(
        id: String,
        displayName: String,
        family: String,
        sizeBytes: Int64,
        quantization: String,
        contextWindow: Int,
        toolCalling: Bool,
        description: String,
        state: ModelState = .notInstalled
    ) {
        self.id = id
        self.displayName = displayName
        self.family = family
        self.sizeBytes = sizeBytes
        self.quantization = quantization
        self.contextWindow = contextWindow
        self.toolCalling = toolCalling
        self.description = description
        self.state = state
    }
}

/// State of the managed local llama-server runtime.
/// Mirror of the planned `mahi_contracts::models::RuntimeStatus`.
public enum RuntimeStatus: Hashable, Sendable {
    /// No model is loaded and nothing is starting.
    case noModel
    /// The runtime binary/process is being prepared (before a model loads).
    case preparingRuntime
    /// A model is loading into the runtime. May take on the order of a minute.
    case starting(modelID: String)
    /// The runtime is serving the given model.
    case running(modelID: String)
    /// The runtime failed; `message` is user-presentable.
    case failed(message: String)

    /// The model currently being started or served, if any.
    public var currentModelID: String? {
        switch self {
        case .starting(let id), .running(let id): return id
        case .noModel, .preparingRuntime, .failed: return nil
        }
    }

    /// True while the runtime is between states (UI should poll/spin).
    public var isTransitioning: Bool {
        switch self {
        case .preparingRuntime, .starting: return true
        case .noModel, .running, .failed: return false
        }
    }
}

/// Hosted-model provider kinds.
public enum HostedProvider: String, Codable, Hashable, Sendable, CaseIterable, Identifiable {
    case anthropic
    case openAICompatible

    public var id: String { rawValue }

    public var displayName: String {
        switch self {
        case .anthropic: return "Anthropic"
        case .openAICompatible: return "OpenAI-compatible"
        }
    }
}

/// Configuration for the hosted (cloud) provider.
/// Mirror of the planned `mahi_contracts::compute::HostedConfig`.
public struct HostedConfig: Hashable, Sendable {
    public let provider: HostedProvider
    public let apiKey: String
    /// Provider model id, e.g. "claude-fable-5".
    public let model: String
    /// Optional override base URL (mainly for OpenAI-compatible servers).
    public let baseURL: String?

    public init(
        provider: HostedProvider,
        apiKey: String,
        model: String,
        baseURL: String? = nil
    ) {
        self.provider = provider
        self.apiKey = apiKey
        self.model = model
        self.baseURL = baseURL
    }
}
