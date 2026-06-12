import MahiKit
import SwiftUI

/// App settings: the active compute mode, hosted model id, and an about pane.
struct SettingsView: View {
    @EnvironmentObject private var model: AppModel
    @AppStorage("hostedModelID") private var hostedModelID: String = "claude-fable-5"

    var body: some View {
        TabView {
            general.tabItem { Label("General", systemImage: "gearshape") }
            about.tabItem { Label("About", systemImage: "info.circle") }
        }
        .frame(width: 480, height: 300)
        .padding()
    }

    private var general: some View {
        Form {
            LabeledContent("Active mode") {
                Label(model.activeMode.displayName, systemImage: model.activeMode.symbolName)
            }
            TextField("Hosted model id", text: $hostedModelID)
            Text("On-device runs fully offline and free. Mac (LAN/Remote) and Hosted "
                + "modes activate once the Rust core is linked and a Mac or API key is "
                + "available.")
                .font(.caption)
                .foregroundStyle(.secondary)
        }
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
