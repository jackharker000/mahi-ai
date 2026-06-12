import AppKit
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
    @State private var permissions = (screenRecording: false, accessibility: false)

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

            Section("Computer use") {
                permissionRow(
                    name: "Screen Recording",
                    granted: permissions.screenRecording,
                    hint: "Lets Mahi see what's on screen."
                )
                permissionRow(
                    name: "Accessibility",
                    granted: permissions.accessibility,
                    hint: "Lets Mahi read the UI and control the Mac."
                )
                HStack(spacing: 8) {
                    Button("Open Privacy Settings") {
                        if let url = URL(
                            string: "x-apple.systempreferences:com.apple.preference.security?Privacy"
                        ) {
                            NSWorkspace.shared.open(url)
                        }
                    }
                    Button("Refresh") {
                        permissions = MahiComputerController.permissionsStatus()
                    }
                }
                Text("Grant these in System Settings → Privacy & Security, then relaunch Mahi. "
                    + "Mahi only acts on the steps you approve, and never on login or payment screens.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
        }
        .formStyle(.grouped)
        .onAppear { permissions = MahiComputerController.permissionsStatus() }
    }

    private func permissionRow(name: String, granted: Bool, hint: String) -> some View {
        LabeledContent {
            Label(
                granted ? "Granted" : "Not granted",
                systemImage: granted ? "checkmark.circle.fill" : "xmark.circle"
            )
            .foregroundStyle(granted ? Color.green : Color.secondary)
        } label: {
            VStack(alignment: .leading, spacing: 2) {
                Text(name)
                Text(hint).font(.caption).foregroundStyle(.secondary)
            }
        }
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
            Text("Local models · tools · agents · computer use")
                .font(.caption)
                .foregroundStyle(.secondary)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }
}
