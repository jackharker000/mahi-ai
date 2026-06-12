import MahiKit
import SwiftUI

/// The app's top-level layout: conversation sidebar + chat detail, with the
/// approval sheet and error alert layered on top.
struct RootView: View {
    @EnvironmentObject private var model: AppModel

    var body: some View {
        NavigationSplitView {
            ConversationSidebar()
                .frame(minWidth: 220)
        } detail: {
            ChatView()
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
}
