import AppKit
import MahiKit
import SwiftUI

/// The chat transcript + composer, with a toolbar badge showing what is
/// actually serving this chat (the local runtime's model when one is running,
/// otherwise the active compute mode).
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
                backendBadge
            }
        }
    }

    /// "Local · Qwen2.5 7B" while the managed runtime serves a model;
    /// the compute-mode pill otherwise.
    @ViewBuilder private var backendBadge: some View {
        if case .running(let modelID) = model.runtime {
            BadgePill(
                text: "Local · \(model.displayName(forModelID: modelID) ?? modelID)",
                systemImage: "cpu",
                help: "Served by the managed local runtime."
            )
        } else {
            BadgePill(
                text: model.activeMode.displayName,
                systemImage: model.activeMode.symbolName,
                help: model.activeMode.explanation
            )
        }
    }

    private var transcript: some View {
        ScrollViewReader { proxy in
            ScrollView {
                LazyVStack(alignment: .leading, spacing: 12) {
                    ForEach(model.messages) { message in
                        bubble(for: message)
                            .id(message.id)
                    }
                    if model.isStreaming {
                        if let tool = model.activeTool {
                            ToolBubble(name: tool.name, detail: tool.detail, isRunning: true)
                        }
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

    @ViewBuilder private func bubble(for message: Message) -> some View {
        if message.role == .tool {
            let (name, detail) = Self.splitToolNote(message.textContent)
            ToolBubble(name: name, detail: detail, isRunning: false)
        } else {
            MessageBubble(role: message.role, text: message.textContent)
        }
    }

    /// Persisted tool notes look like "web.search result: {…}" — show the first
    /// word as the tool name and the rest as the detail line.
    static func splitToolNote(_ text: String) -> (name: String, detail: String) {
        let pieces = text.split(separator: " ", maxSplits: 1, omittingEmptySubsequences: false)
        guard pieces.count == 2 else { return (text, "") }
        return (String(pieces[0]), String(pieces[1]))
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

/// A compact "Tool" bubble: wrench icon, tool name, detail line, and a subtle
/// spinner while the tool is still running.
private struct ToolBubble: View {
    let name: String
    let detail: String
    let isRunning: Bool

    var body: some View {
        HStack {
            HStack(spacing: 8) {
                Image(systemName: "wrench.and.screwdriver")
                    .font(.caption)
                    .foregroundStyle(.orange)
                VStack(alignment: .leading, spacing: 2) {
                    HStack(spacing: 6) {
                        Text("Tool").font(.caption2).foregroundStyle(.secondary)
                        Text(name)
                            .font(.system(.caption, design: .monospaced).weight(.semibold))
                    }
                    if !detail.isEmpty {
                        Text(detail)
                            .font(.system(.caption, design: .monospaced))
                            .foregroundStyle(.secondary)
                            .lineLimit(3)
                            .textSelection(.enabled)
                    }
                }
                if isRunning {
                    ProgressView().controlSize(.mini)
                }
            }
            .padding(8)
            .background(Color.orange.opacity(0.12))
            .clipShape(RoundedRectangle(cornerRadius: 10))
            Spacer(minLength: 48)
        }
    }
}

/// A small capsule badge for the toolbar.
private struct BadgePill: View {
    let text: String
    let systemImage: String
    let help: String

    var body: some View {
        Label(text, systemImage: systemImage)
            .font(.caption)
            .padding(.horizontal, 8)
            .padding(.vertical, 4)
            .background(Color.secondary.opacity(0.12))
            .clipShape(Capsule())
            .help(help)
    }
}
