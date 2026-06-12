import MahiKit
import SwiftUI

/// The left-hand list of conversations, with a "new conversation" button.
struct ConversationSidebar: View {
    @EnvironmentObject private var model: AppModel

    var body: some View {
        List(selection: selectionBinding) {
            Section("Conversations") {
                ForEach(model.conversations) { conversation in
                    Label(conversation.displayTitle, systemImage: conversation.modeAtCreation.symbolName)
                        .lineLimit(1)
                        .tag(conversation.id)
                }
            }
        }
        .navigationTitle("Mahi")
        .toolbar {
            ToolbarItem {
                Button {
                    Task { await model.newConversation() }
                } label: {
                    Image(systemName: "square.and.pencil")
                }
                .help("New conversation")
            }
        }
    }

    private var selectionBinding: Binding<UUID?> {
        Binding(
            get: { model.selectedID },
            set: { newValue in
                if let id = newValue { Task { await model.select(id) } }
            }
        )
    }
}
