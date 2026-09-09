import XCTest
@testable import WagaWebRTC

final class DiagnosticTests: XCTestCase {
    func testDisabledEstimatorSnapshot() throws {
        let core = try WagaCore(audio: .opus, video: .h264)
        XCTAssertEqual(core.bweDiagnosticSnapshot(), "bwe=disabled")
    }

    func testEnabledEstimatorSnapshotIsRepeatable() throws {
        let core = try WagaCore(audio: .opus, video: .h264, targetBitrate: 5_000_000)
        let snapshot = try XCTUnwrap(core.bweDiagnosticSnapshot())
        XCTAssertTrue(snapshot.contains("loss_state="))
        XCTAssertTrue(snapshot.contains("hold_observation_ms=None"))
        XCTAssertTrue(snapshot.contains("probe_enabled=false"))
        XCTAssertEqual(core.bweDiagnosticSnapshot(), snapshot)
        XCTAssertNil(core.pollBitrateEstimate())
    }
}
