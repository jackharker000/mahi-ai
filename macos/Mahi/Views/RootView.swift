import MahiKit
import SwiftUI

/// Top-level app sections shown in the navigation sidebar.
/// (Settings remains its own `Settings` scene.)
enum AppSection: String, CaseIterable, Identifiable {
    case chat
    case models
    case agents

    var id: String { rawValue }

    var title: String {
        switch self {
        case .chat: return "Chat"
        case .models: return "Models"
        case .agents: return "Agents"
        }
    }

    var symbolName: String {
        switch self {
        case .chat: return "bubble.left.and.bubble.right"
        case .models: return "shippingbox"
        case .agents: return "person.3"
        }
    }
}

/// The app's top-level layout: a section sidebar (Chat / Models / Agents) with
/// the approval sheet and error alert layered on top. The Chat section keeps
/// its own conversation list pane.
struct RootView: View {
    @EnvironmentObject private var model: AppModel
    @State private var section: AppSection? = .chat

    var body: some View {
        NavigationSplitView {
            List(selection: $section) {
                Section("Mahi") {
                    ForEach(AppSection.allCases) { item in
                        Label(item.title, systemImage: item.symbolName)
                            .tag(item)
                    }
                }
            }
            .navigationTitle("Mahi")
            .frame(minWidth: 180)
        } detail: {
            detail
        }
        .task { await model.bootstrap() }
        .sheet(item: $model.pendingApproval) { pending in
            ApprovalSheet(pending: pending).environmentObject(model)
        }
        .alert(
            "Something went wrong",
            isPresented: Binding(
                get: { model.errorText != nil },
                set: { presented in if !presented { model.errorText = nil } }
            )
        ) {
            Button("OK", role: .cancel) { model.errorText = nil }
        } message: {
            Text(model.errorText ?? "")
        }
    }

    @ViewBuilder private var detail: some View {
        switch section ?? .chat {
        case .chat:
            HSplitView {
                ConversationSidebar()
                    .frame(minWidth: 200, idealWidth: 240, maxWidth: 320)
                ChatView()
                    .frame(minWidth: 420, maxWidth: .infinity)
            }
        case .models:
            ModelsView()
        case .agents:
            AgentsView()
        }
    }
}
