import XCTest
@testable import MahiKit

final class MahiKitTests: XCTestCase {
    func testApprovalPreviewParsing() {
        XCTAssertEqual(ApprovalPreview.parse(summary: "command:ls -la"), .command("ls -la"))
        XCTAssertEqual(ApprovalPreview.parse(summary: "just text"), .plain("just text"))
    }

    func testComputeModeDisplay() {
        XCTAssertEqual(ComputeMode.onDevice.displayName, "On-Device")
        XCTAssertTrue(ComputeMode.hosted.requiresNetwork)
        XCTAssertFalse(ComputeMode.onDevice.requiresNetwork)
    }

    func testPreviewMockEngineRunsATurn() async throws {
        let engine = PreviewMockEngine()
        let id = try await engine.createConversation(mode: .onDevice)
        let handle = try await engine.runTurn(conversationID: id, userText: "hi")
        var sawText = false
        var finished = false
        for try await event in handle.events() {
            switch event {
            case .textDelta: sawText = true
            case .turnFinished: finished = true
            default: break
            }
        }
        XCTAssertTrue(sawText)
        XCTAssertTrue(finished)
    }
}
