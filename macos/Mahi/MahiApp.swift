import AppKit
import MahiKit
import SwiftUI

/// The Mahi macOS app: a chat window, a menu-bar quick-access item, and Settings.
///
/// The app runs on the in-process `PreviewMockEngine` until the Rust core has
/// been linked (run `macos/scripts/build-xcframework.sh`), at which point the
/// `FfiEngine` path lights up. See `AppModel` for the wiring.
@main
struct MahiApp: App {
    @StateObject private var model = AppModel()

    var body: some Scene {
        WindowGroup {
            RootView()
                .environmentObject(model)
                .frame(minWidth: 900, minHeight: 560)
        }
        .commands {
            CommandGroup(replacing: .newItem) {
                Button("New Conversation") { Task { await model.newConversation() } }
                    .keyboardShortcut("n", modifiers: .command)
            }
        }

        MenuBarExtra("Mahi", systemImage: "sparkles") {
            Button("Open Mahi") { NSApp.activate(ignoringOtherApps: true) }
                .keyboardShortcut("o")
            Button("New Conversation") { Task { await model.newConversation() } }
            Divider()
            Button("Quit Mahi") { NSApp.terminate(nil) }
                .keyboardShortcut("q")
        }

        Settings {
            SettingsView().environmentObject(model)
        }
    }
}
