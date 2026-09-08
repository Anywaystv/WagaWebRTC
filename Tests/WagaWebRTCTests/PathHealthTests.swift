@testable import WagaWebRTC
import XCTest

final class PathHealthTests: XCTestCase {
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
