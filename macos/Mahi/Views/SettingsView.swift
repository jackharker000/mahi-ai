import MahiKit
import SwiftUI

/// App settings: the active compute mode, hosted-provider configuration,
/// and an about pane.
struct SettingsView: View {
    @EnvironmentObject private var model: AppModel

    @AppStorage("hostedProvider") private var hostedProviderRaw: String =
        HostedProvider.anthropic.rawValue
    @AppStorage("hostedModelID") private var hostedModelID: String = "claude-fable-5"
    @AppStorage("hostedBaseURL") private var hostedBaseURL: String = ""
    // TODO: move to Keychain — @AppStorage is plaintext defaults, fine only for dev.
    @AppStorage("hostedAPIKey") private var hostedAPIKey: String = ""

    @State private var didSave = false

    var body: some View {
        TabView {
            general.tabItem { Label("General", systemImage: "gearshape") }
            about.tabItem { Label("About", systemImage: "info.circle") }
        }
        .frame(width: 520, height: 440)
        .padding()
    }

    private var hostedProvider: HostedProvider {
        HostedProvider(rawValue: hostedProviderRaw) ?? .anthropic
    }

    private var general: some View {
        Form {
            Section {
                LabeledContent("Active mode") {
                    Label(model.activeMode.displayName, systemImage: model.activeMode.symbolName)
                }
                Text("On-device runs fully offline and free. Mac (LAN/Remote) and Hosted "
                    + "modes activate once the Rust core is linked and a Mac or API key is "
                    + "available.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }

            Section("Hosted models") {
                Picker("Provider", selection: providerBinding) {
                    ForEach(HostedProvider.allCases) { provider in
                        Text(provider.displayName).tag(provider)
                    }
                }

                SecureField("API key", text: $hostedAPIKey)

                TextField(
                    "Model id",
                    text: $hostedModelID,
                    prompt: Text(hostedProvider == .anthropic ? "claude-fable-5" : "model id")
                )

                if hostedProvider == .openAICompatible {
                    TextField(
                        "Base URL (optional)",
                        text: $hostedBaseURL,
                        prompt: Text("https://my-server.example/v1")
                    )
                }

                HStack(spacing: 8) {
                    Button("Save") {
                        didSave = false
                        let provider = hostedProvider
                        let key = hostedAPIKey
                        let modelID = hostedModelID
                        let base = hostedBaseURL
                        Task {
                            await model.saveHostedConfig(
                                provider: provider,
                                apiKey: key,
                                model: modelID,
                                baseURL: provider == .openAICompatible ? base : nil
                            )
                            didSave = true
                        }
                    }
                    if didSave {
                        Label("Saved", systemImage: "checkmark")
                            .font(.caption)
                            .foregroundStyle(.secondary)
                    }
                }

                Text("Leave the API key empty and press Save to clear the hosted config.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
        }
        .formStyle(.grouped)
    }

    private var providerBinding: Binding<HostedProvider> {
        Binding(
            get: { hostedProvider },
            set: { newValue in
                hostedProviderRaw = newValue.rawValue
                didSave = false
            }
        )
    }

    private var about: some View {
        VStack(spacing: 8) {
            Image(systemName: "sparkles").font(.largeTitle)
            Text("Mahi AI").font(.title2).bold()
            Text("Local-first, multi-surface AI assistant.").foregroundStyle(.secondary)
            Text("Phase 0 · on-device preview").font(.caption).foregroundStyle(.secondary)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }
}
