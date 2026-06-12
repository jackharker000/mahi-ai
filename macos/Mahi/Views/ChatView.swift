import AppKit
import MahiKit
import SwiftUI

/// The chat transcript + composer, with the active-compute-mode badge.
struct ChatView: View {
    @EnvironmentObject private var model: AppModel
    private let streamingAnchor = "streaming-bubble"

    var body: some View {
        VStack(spacing: 0) {
            transcript
            Divider()
            composer
        }
        .navigationTitle("Chat")
        .toolbar {
            ToolbarItem(placement: .automatic) {
                ModeBadge(mode: model.activeMode)
            }
        }
    }

    private var transcript: some View {
        ScrollViewReader { proxy in
            ScrollView {
                LazyVStack(alignment: .leading, spacing: 12) {
                    ForEach(model.messages) { message in
                        MessageBubble(role: message.role, text: message.textContent)
                            .id(message.id)
                    }
                    if model.isStreaming {
                        MessageBubble(
                            role: .assistant,
                            text: model.streamingText.isEmpty ? "…" : model.streamingText
                        )
                        .id(streamingAnchor)
                    }
                }
                .padding()
            }
            .onChange(of: model.messages.count) { _ in
                withAnimation { proxy.scrollTo(model.messages.last?.id, anchor: .bottom) }
            }
            .onChange(of: model.streamingText) { _ in
                proxy.scrollTo(streamingAnchor, anchor: .bottom)
            }
        }
    }

    private var composer: some View {
        HStack(alignment: .bottom, spacing: 8) {
            TextField("Message Mahi…", text: $model.composer, axis: .vertical)
                .textFieldStyle(.plain)
                .lineLimit(1 ... 6)
                .padding(8)
                .background(Color(nsColor: .textBackgroundColor))
                .clipShape(RoundedRectangle(cornerRadius: 10))
                .onSubmit { model.send() }

            if model.isStreaming {
                Button(role: .destructive) { model.cancel() } label: {
                    Image(systemName: "stop.circle.fill").font(.title2)
                }
                .buttonStyle(.plain)
                .help("Stop")
            } else {
                Button { model.send() } label: {
                    Image(systemName: "arrow.up.circle.fill").font(.title2)
                }
                .buttonStyle(.plain)
                .disabled(model.composer.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
                .help("Send")
            }
        }
        .padding(12)
    }
}

/// One message rendered as a left/right bubble.
private struct MessageBubble: View {
    let role: MessageRole
    let text: String

    var body: some View {
        HStack {
            if role == .user { Spacer(minLength: 48) }
            VStack(alignment: .leading, spacing: 4) {
                Text(roleLabel).font(.caption2).foregroundStyle(.secondary)
                Text(text.isEmpty ? " " : text)
                    .textSelection(.enabled)
                    .frame(maxWidth: .infinity, alignment: .leading)
            }
            .padding(10)
            .background(background)
            .clipShape(RoundedRectangle(cornerRadius: 12))
            if role != .user { Spacer(minLength: 48) }
        }
    }

    private var roleLabel: String {
        switch role {
        case .user: return "You"
        case .assistant: return "Mahi"
        case .tool: return "Tool"
        case .system: return "System"
        }
    }

    private var background: Color {
        switch role {
        case .user: return Color.accentColor.opacity(0.18)
        case .assistant: return Color(nsColor: .controlBackgroundColor)
        case .tool: return Color.orange.opacity(0.12)
        case .system: return Color.gray.opacity(0.12)
        }
    }
}

/// The active-compute-mode pill shown in the toolbar.
private struct ModeBadge: View {
    let mode: ComputeMode

    var body: some View {
        Label(mode.displayName, systemImage: mode.symbolName)
            .font(.caption)
            .padding(.horizontal, 8)
            .padding(.vertical, 4)
            .background(Color.secondary.opacity(0.12))
            .clipShape(Capsule())
            .help(mode.explanation)
    }
}
