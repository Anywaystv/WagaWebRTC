@testable import WagaWebRTC
import XCTest

final class PathHealthTests: XCTestCase {
    func testAvailabilityTracksRepeatedOutagesWithoutWithdrawingGatheringCandidate() {
        var health = WagaPathHealth()
        XCTAssertNil(health.availabilityChange(now: 0))
        for cycle in 0 ..< 3 {
            let now = UInt64(cycle) * 4_000_000_000
            health.succeeded("public", now: now)
            XCTAssertEqual(health.availabilityChange(now: now), cycle == 0 ? nil : true)
            XCTAssertNil(health.availabilityChange(now: now + 1))
            XCTAssertEqual(health.availabilityChange(now: now + WagaPathHealth.lifetime), false)
            XCTAssertNil(health.availabilityChange(now: now + WagaPathHealth.lifetime + 1))
        }
    }

    func testCellularCanUsePublicDestinationWhenSelectedLanDestinationIsUnreachable() {
        var health = WagaPathHealth()
        health.succeeded("public", now: 0)
        XCTAssertEqual(health.destination(preferred: "lan", candidates: ["lan", "public"], now: 1), "public")
        XCTAssertNil(health.destination(preferred: "lan", candidates: ["public"], now: WagaPathHealth.lifetime))
        health.succeeded("lan", now: WagaPathHealth.lifetime)
        XCTAssertEqual(health.destination(preferred: "lan", candidates: ["public"], now: WagaPathHealth.lifetime), "lan")
        XCTAssertNil(health.availabilityChange(now: WagaPathHealth.lifetime))
    }

    func testSilentWifiExpiresWhileCellularRemainsUsable() {
        var wifi = WagaPathHealth()
        var cellular = WagaPathHealth()
        wifi.succeeded("server", now: 0)
        cellular.succeeded("server", now: 1_000_000_000)
        XCTAssertFalse(wifi.isLive("server", now: 1_500_000_000))
        XCTAssertTrue(cellular.isLive("server", now: 1_500_000_000))
        wifi.succeeded("server", now: 2_000_000_000)
        XCTAssertTrue(wifi.isLive("server", now: 2_000_000_001))
    }

    func testFailureInvalidatesOnlyAffectedDestination() {
        var health = WagaPathHealth()
        health.succeeded("a", now: 0)
        health.succeeded("b", now: 0)
        health.invalidate("a")
        XCTAssertFalse(health.isLive("a", now: 1))
        XCTAssertTrue(health.isLive("b", now: 1))
        XCTAssertFalse(health.isLive("unknown", now: 1))
    }
}
