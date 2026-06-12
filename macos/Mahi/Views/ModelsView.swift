import MahiKit
import SwiftUI

/// The local-model manager: an Ollama-style catalog with per-model download /
/// run / delete controls and a live runtime header.
struct ModelsView: View {
    @EnvironmentObject private var model: AppModel
    /// The model the user most recently asked to run — lets the row show
    /// "Starting…" during `.preparingRuntime`, before the runtime knows an id.
    @State private var pendingActivationID: String?

    var body: some View {
        VStack(spacing: 0) {
            runtimeHeader
            Divider()
            catalogList
        }
        .navigationTitle("Models")
        .task { await model.refreshModels() }
        // An async timer keeps the list live while anything is in flight.
        .task(id: isBusy) {
            guard isBusy else { return }
            while !Task.isCancelled {
                try? await Task.sleep(for: .milliseconds(400))
                if Task.isCancelled { break }
                await model.refreshModels()
            }
        }
        .onChange(of: model.runtime) { newStatus in
            switch newStatus {
            case .running, .failed, .noModel:
                pendingActivationID = nil
            case .preparingRuntime, .starting:
                break
            }
        }
    }

    /// True while any download is in flight or the runtime is between states.
    private var isBusy: Bool {
        if model.runtime.isTransitioning { return true }
        return model.models.contains { catalogModel in
            if case .downloading = catalogModel.state { return true }
            return false
        }
    }

    // MARK: Header

    private var runtimeHeader: some View {
        HStack(spacing: 10) {
            runtimeIndicator
            VStack(alignment: .leading, spacing: 2) {
                Text(model.runtimeHeadline).font(.headline)
                Text("Local llama runtime, managed by Mahi")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
            Spacer()
            Button {
                Task { await model.refreshModels() }
            } label: {
                Image(systemName: "arrow.clockwise")
            }
            .help("Refresh catalog")
        }
        .padding(12)
    }

    @ViewBuilder private var runtimeIndicator: some View {
        switch model.runtime {
        case .running:
            Circle().fill(.green).frame(width: 10, height: 10)
        case .preparingRuntime, .starting:
            ProgressView().controlSize(.small)
        case .failed:
            Circle().fill(.red).frame(width: 10, height: 10)
        case .noModel:
            Circle().fill(.gray.opacity(0.5)).frame(width: 10, height: 10)
        }
    }

    // MARK: Catalog

    private var installed: [CatalogModel] {
        model.models.filter { $0.state == .installed || $0.state == .active }
    }

    private var available: [CatalogModel] {
        model.models.filter { catalogModel in
            switch catalogModel.state {
            case .notInstalled, .downloading: return true
            case .installed, .active: return false
            }
        }
    }

    private var catalogList: some View {
        List {
            Section("Installed") {
                if installed.isEmpty {
                    Text("No models installed yet — download one below.")
                        .foregroundStyle(.secondary)
                } else {
                    ForEach(installed) { catalogModel in
                        row(for: catalogModel)
                    }
                }
            }
            Section("Available") {
                ForEach(available) { catalogModel in
                    row(for: catalogModel)
                }
            }
        }
    }

    private func row(for catalogModel: CatalogModel) -> some View {
        HStack(spacing: 12) {
            VStack(alignment: .leading, spacing: 4) {
                HStack(spacing: 6) {
                    Text(catalogModel.displayName).font(.body.weight(.medium))
                    familyChip(catalogModel.family)
                    if catalogModel.toolCalling {
                        badge("Tools", color: .blue)
                    }
                }
                Text(detailLine(for: catalogModel))
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
            Spacer()
            controls(for: catalogModel)
        }
        .padding(.vertical, 4)
        .contextMenu {
            contextMenuItems(for: catalogModel)
        }
    }

    private func detailLine(for catalogModel: CatalogModel) -> String {
        let size = ByteCountFormatter.string(
            fromByteCount: catalogModel.sizeBytes, countStyle: .file
        )
        let context = "\(catalogModel.contextWindow / 1024)K context"
        return "\(size) · \(catalogModel.quantization) · \(context)"
    }

    private func familyChip(_ family: String) -> some View {
        Text(family)
            .font(.caption2)
            .padding(.horizontal, 6)
            .padding(.vertical, 2)
            .background(Color.secondary.opacity(0.15))
            .clipShape(Capsule())
    }

    private func badge(_ text: String, color: Color) -> some View {
        Text(text)
            .font(.caption2.weight(.semibold))
            .padding(.horizontal, 6)
            .padding(.vertical, 2)
            .background(color.opacity(0.15))
            .foregroundStyle(color)
            .clipShape(Capsule())
    }

    // MARK: Per-state controls

    @ViewBuilder private func controls(for catalogModel: CatalogModel) -> some View {
        switch catalogModel.state {
        case .notInstalled:
            Button("Download") {
                Task { await model.download(modelID: catalogModel.id) }
            }

        case .downloading(let progress, let bytesDownloaded):
            HStack(spacing: 8) {
                Text(downloadCaption(progress: progress, bytes: bytesDownloaded))
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .monospacedDigit()
                ProgressView(value: progress)
                    .progressViewStyle(.circular)
                    .controlSize(.small)
                Button("Cancel") {
                    Task { await model.cancelDownload(modelID: catalogModel.id) }
                }
            }

        case .installed:
            if isStarting(catalogModel) {
                HStack(spacing: 6) {
                    ProgressView().controlSize(.small)
                    Text("Starting…").font(.caption).foregroundStyle(.secondary)
                }
            } else {
                Button("Run") {
                    pendingActivationID = catalogModel.id
                    Task { await model.activate(modelID: catalogModel.id) }
                }
                .disabled(model.runtime.isTransitioning)
            }

        case .active:
            Label("Running", systemImage: "checkmark.circle.fill")
                .font(.caption.weight(.medium))
                .foregroundStyle(.green)
        }
    }

    /// True while the runtime is spinning up specifically for this model.
    private func isStarting(_ catalogModel: CatalogModel) -> Bool {
        guard model.runtime.isTransitioning else { return false }
        if model.runtime.currentModelID == catalogModel.id { return true }
        // `.preparingRuntime` carries no id yet — fall back to the local intent.
        return model.runtime.currentModelID == nil && pendingActivationID == catalogModel.id
    }

    private func downloadCaption(progress: Double, bytes: Int64) -> String {
        let fetched = ByteCountFormatter.string(fromByteCount: bytes, countStyle: .file)
        return "\(Int(progress * 100))% · \(fetched)"
    }

    @ViewBuilder private func contextMenuItems(for catalogModel: CatalogModel) -> some View {
        switch catalogModel.state {
        case .installed, .active:
            Button("Delete from disk", role: .destructive) {
                Task { await model.delete(modelID: catalogModel.id) }
            }
        case .downloading:
            Button("Cancel download") {
                Task { await model.cancelDownload(modelID: catalogModel.id) }
            }
        case .notInstalled:
            Button("Download") {
                Task { await model.download(modelID: catalogModel.id) }
            }
        }
    }
}
