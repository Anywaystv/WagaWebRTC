@testable import WagaWebRTC
import XCTest

final class AudioCodecTests: XCTestCase {
    func testAacChoiceReachesRustCore() throws {
        let core = try WagaCore(audio: .aac, video: .h264)
        try core.addLocalCandidate("127.0.0.1:41000")
        let offer = try core.createOffer()
        XCTAssertTrue(offer.contains("MPEG4-GENERIC/48000/2"))
        XCTAssertTrue(offer.contains("config=1190"))
        XCTAssertFalse(offer.contains("opus/48000"))
    }

    func testOpusChoiceStillOffersOpus() throws {
        let core = try WagaCore(audio: .opus, video: .h264)
        try core.addLocalCandidate("127.0.0.1:41000")
        let offer = try core.createOffer()
        XCTAssertTrue(offer.contains("opus/48000"))
        XCTAssertFalse(offer.contains("MPEG4-GENERIC"))
    }
}
