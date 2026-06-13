import MahiKit
import SwiftUI

/// Fan-out subagents: enter one goal per row, run them in parallel, and read
/// back one short summary per goal.
struct AgentsView: View {
    @EnvironmentObject private var model: AppModel
    @State private var goals: [GoalDraft] = [GoalDraft()]

    /// One editable goal row.
    private struct GoalDraft: Identifiable {
        let id = UUID()
        var text: String = ""
    }

    var body: some View {
        Form {
            Section("Goals") {
                ForEach($goals) { $goal in
                    HStack(spacing: 8) {
                        TextField("e.g. Summarize this week's inbox", text: $goal.text)
                            .textFieldStyle(.roundedBorder)
                        Button {
                            remove(goal.id)
                        } label: {
                            Image(systemName: "minus.circle")
                        }
                        .buttonStyle(.plain)
                        .disabled(goals.count == 1)
                        .help("Remove this goal")
                    }
                }
                Button {
                    goals.append(GoalDraft())
                } label: {
                    Label("Add goal", systemImage: "plus.circle")
                }
                .buttonStyle(.plain)
            }

            Section {
                HStack(spacing: 10) {
                    Button("Run agents") {
                        let texts = goals.map(\.text)
                        Task { await model.runAgents(goals: texts) }
                    }
                    .keyboardShortcut(.defaultAction)
                    .disabled(allGoalsEmpty || model.isRunningAgents)

                    if model.isRunningAgents {
                        ProgressView().controlSize(.small)
                        Text("Running \(nonEmptyGoalCount) agent\(nonEmptyGoalCount == 1 ? "" : "s")…")
                            .font(.caption)
                            .foregroundStyle(.secondary)
                    }
                }
            }

            if !model.agentResults.isEmpty {
                Section("Results") {
                    ForEach(Array(model.agentResults.enumerated()), id: \.offset) { _, summary in
                        Label {
                            Text(summary).textSelection(.enabled)
                        } icon: {
                            Image(systemName: "checkmark.circle")
                                .foregroundStyle(.green)
                        }
                    }
                }
            }
        }
        .formStyle(.grouped)
        .navigationTitle("Agents")
    }

    private var allGoalsEmpty: Bool {
        goals.allSatisfy {
            $0.text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
        }
    }

    private var nonEmptyGoalCount: Int {
        goals.filter {
            !$0.text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
        }.count
    }

    private func remove(_ id: GoalDraft.ID) {
        guard goals.count > 1 else { return }
        goals.removeAll { $0.id == id }
    }
}
