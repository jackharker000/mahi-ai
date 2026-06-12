import XCTest
@testable import MahiKit

/// Exercises the local-model manager, hosted config, and subagent surface of
/// `PreviewMockEngine`. All waits are bounded polls against the engine's own
/// snapshots (no fixed long sleeps), with timings cranked down for tests.
final class PreviewMockEngineModelTests: XCTestCase {
    /// An engine whose simulated download/activation timers run fast.
    private func makeFastEngine() -> PreviewMockEngine {
        PreviewMockEngine(
            chunkDelay: .milliseconds(1),
            downloadTick: .milliseconds(5),
            activationStep: .milliseconds(10)
        )
    }

    /// Poll `condition` every `interval` up to `attempts` times.
    @discardableResult
    private func poll(
        every interval: Duration = .milliseconds(10),
        attempts: Int = 500,
        until condition: () async throws -> Bool
    ) async rethrows -> Bool {
        for _ in 0 ..< attempts {
            if try await condition() { return true }
            try? await Task.sleep(for: interval)
        }
        return try await condition()
    }

    // MARK: Catalog

    func testCatalogIsSeededAndNotInstalled() async throws {
        let engine = makeFastEngine()
        let catalog = try await engine.modelCatalog()

        XCTAssertFalse(catalog.isEmpty)
        XCTAssertTrue(catalog.contains { $0.id == "qwen2.5-7b-instruct-q4_k_m" })
        XCTAssertTrue(catalog.contains { $0.family == "Llama 3.1" && $0.toolCalling })
        XCTAssertTrue(catalog.contains { $0.id == "gemma-2-9b-q4_k_m" && !$0.toolCalling })
        for model in catalog {
            XCTAssertEqual(model.state, .notInstalled, "\(model.id) should start notInstalled")
            XCTAssertGreaterThan(model.sizeBytes, 0)
            XCTAssertGreaterThan(model.contextWindow, 0)
            XCTAssertEqual(model.quantization, "Q4_K_M")
        }

        let status = try await engine.runtimeStatus()
        XCTAssertEqual(status, .noModel)
    }

    // MARK: Downloads

    func testStartDownloadDrivesModelToInstalled() async throws {
        let engine = makeFastEngine()
        let modelID = "qwen2.5-0.5b-instruct-q4_k_m"

        try await engine.startDownload(modelID: modelID)

        // The state must be observable as .downloading or already .installed.
        let entered = try await poll {
            let model = try await self.model(modelID, in: engine)
            switch model.state {
            case .downloading, .installed: return true
            case .notInstalled, .active: return false
            }
        }
        XCTAssertTrue(entered, "download never became observable")

        let installed = try await poll {
            try await self.model(modelID, in: engine).state == .installed
        }
        XCTAssertTrue(installed, "download never reached .installed")
    }

    func testCancelDownloadRevertsToNotInstalled() async throws {
        // Slow ticks so the download is still in flight when we cancel.
        let engine = PreviewMockEngine(
            chunkDelay: .milliseconds(1),
            downloadTick: .milliseconds(250),
            activationStep: .milliseconds(10)
        )
        let modelID = "llama-3.2-3b-instruct-q4_k_m"

        try await engine.startDownload(modelID: modelID)
        let downloading = try await poll {
            if case .downloading = try await self.model(modelID, in: engine).state {
                return true
            }
            return false
        }
        XCTAssertTrue(downloading)

        try await engine.cancelDownload(modelID: modelID)
        let reverted = try await poll {
            try await self.model(modelID, in: engine).state == .notInstalled
        }
        XCTAssertTrue(reverted, "cancel should revert the model to .notInstalled")
    }

    // MARK: Activation

    func testActivateModelDrivesRuntimeToRunning() async throws {
        let engine = makeFastEngine()
        let modelID = "qwen2.5-0.5b-instruct-q4_k_m"

        try await engine.startDownload(modelID: modelID)
        let installed = try await poll {
            try await self.model(modelID, in: engine).state == .installed
        }
        XCTAssertTrue(installed)

        try await engine.activateModel(modelID: modelID)

        // The runtime must immediately be in a transitioning state.
        let status = try await engine.runtimeStatus()
        XCTAssertTrue(
            status.isTransitioning || status == .running(modelID: modelID),
            "unexpected runtime status after activate: \(status)"
        )

        let running = try await poll {
            try await engine.runtimeStatus() == .running(modelID: modelID)
        }
        XCTAssertTrue(running, "runtime never reached .running")

        let active = try await self.model(modelID, in: engine)
        XCTAssertEqual(active.state, .active)
    }

    func testActivateNotInstalledModelThrows() async throws {
        let engine = makeFastEngine()
        do {
            try await engine.activateModel(modelID: "gemma-2-9b-q4_k_m")
            XCTFail("activating a model that is not installed should throw")
        } catch {
            // expected
        }
    }

    func testDeleteActiveModelResetsRuntime() async throws {
        let engine = makeFastEngine()
        let modelID = "qwen2.5-0.5b-instruct-q4_k_m"

        try await engine.startDownload(modelID: modelID)
        _ = try await poll { try await self.model(modelID, in: engine).state == .installed }
        try await engine.activateModel(modelID: modelID)
        _ = try await poll { try await engine.runtimeStatus() == .running(modelID: modelID) }

        try await engine.deleteModel(modelID: modelID)
        let status = try await engine.runtimeStatus()
        XCTAssertEqual(status, .noModel)
        let deleted = try await self.model(modelID, in: engine)
        XCTAssertEqual(deleted.state, .notInstalled)
    }

    // MARK: Hosted config

    func testSetHostedConfigStoresAndClears() async throws {
        let engine = makeFastEngine()
        let config = HostedConfig(
            provider: .anthropic,
            apiKey: "sk-test",
            model: "claude-fable-5"
        )
        try await engine.setHostedConfig(config)
        XCTAssertEqual(engine.hostedConfig, config)

        try await engine.setHostedConfig(nil)
        XCTAssertNil(engine.hostedConfig)
    }

    // MARK: Subagents

    func testSpawnSubagentsReturnsOneSummaryPerGoal() async throws {
        let engine = makeFastEngine()
        let goals = ["Summarize the inbox", "Draft a standup note", "Find the failing test"]

        let summaries = try await engine.spawnSubagents(goals: goals)

        XCTAssertEqual(summaries.count, goals.count)
        for (goal, summary) in zip(goals, summaries) {
            XCTAssertTrue(summary.contains(goal), "summary should echo its goal: \(summary)")
            XCTAssertTrue(summary.contains("done (mock)"))
        }

        let none = try await engine.spawnSubagents(goals: [])
        XCTAssertTrue(none.isEmpty)
    }

    // MARK: Turn stream still carries tool events

    func testRunTurnEmitsToolPair() async throws {
        let engine = PreviewMockEngine(chunkDelay: .milliseconds(1))
        let conversationID = try await engine.createConversation(mode: .onDevice)
        let handle = try await engine.runTurn(conversationID: conversationID, userText: "hello")

        var sawChunk = false
        var sawResult = false
        var finished = false
        for try await event in handle.events() {
            if case .tool(let toolEvent) = event {
                if case .chunk = toolEvent { sawChunk = true }
                if case .result = toolEvent { sawResult = true }
            }
            if case .turnFinished = event { finished = true }
        }
        XCTAssertTrue(sawChunk, "expected a tool start chunk in the plain turn")
        XCTAssertTrue(sawResult, "expected a tool result in the plain turn")
        XCTAssertTrue(finished)
    }

    // MARK: Helpers

    private func model(
        _ id: String, in engine: PreviewMockEngine
    ) async throws -> CatalogModel {
        let catalog = try await engine.modelCatalog()
        guard let model = catalog.first(where: { $0.id == id }) else {
            throw EngineError.invalidIdentifier(id)
        }
        return model
    }
}
