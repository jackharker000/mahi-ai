import AppKit
import MahiKit
import SwiftUI

/// The approval wall: a rich preview of a gated action with allow/deny choices.
/// Every destructive tool call (write / shell / computer-use) routes through here.
struct ApprovalSheet: View {
    @EnvironmentObject private var model: AppModel
    let pending: AppModel.PendingApproval

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            Label("Approval required", systemImage: "exclamationmark.shield.fill")
                .font(.headline)
            Text(headline).foregroundStyle(.secondary)
            preview
            HStack {
                Button("Deny", role: .destructive) { model.resolveApproval(.deny) }
                    .keyboardShortcut(.cancelAction)
                Spacer()
                Button("Allow Once") { model.resolveApproval(.allowOnce) }
                Button("Allow Always") { model.resolveApproval(.allowAlways) }
                    .keyboardShortcut(.defaultAction)
            }
        }
        .padding(20)
        .frame(width: 520)
    }

    private var headline: String {
        switch pending.preview {
        case .command: return "Mahi wants to run a shell command."
        case .diff: return "Mahi wants to apply a code change."
        case .screenshot: return "Mahi wants to act on this screen region."
        case .plain(let text): return text
        }
    }

    @ViewBuilder private var preview: some View {
        switch pending.preview {
        case .command(let command): codeBlock(command)
        case .diff(let diff): codeBlock(diff)
        case .screenshot(let data):
            if let image = NSImage(data: data) {
                Image(nsImage: image)
                    .resizable()
                    .scaledToFit()
                    .frame(maxHeight: 320)
                    .clipShape(RoundedRectangle(cornerRadius: 8))
            } else {
                Text("(unrenderable screenshot)").foregroundStyle(.secondary)
            }
        case .plain:
            EmptyView()
        }
    }

    private func codeBlock(_ text: String) -> some View {
        ScrollView {
            Text(text)
                .font(.system(.body, design: .monospaced))
                .textSelection(.enabled)
                .frame(maxWidth: .infinity, alignment: .leading)
        }
        .frame(maxHeight: 260)
        .padding(10)
        .background(Color(nsColor: .textBackgroundColor))
        .clipShape(RoundedRectangle(cornerRadius: 8))
    }
}
