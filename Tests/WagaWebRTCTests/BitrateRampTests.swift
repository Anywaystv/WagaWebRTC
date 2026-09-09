import XCTest
@testable import WagaWebRTC

final class BitrateRampTests: XCTestCase {
    func testConfirmedBandwidthRecoversFromFloorWithinFourSeconds() {
        var ramp = WagaBitrateRamp()
        var current: UInt64 = 5_000_000
        ramp.observe(ceiling: 250_000, now: 0)
        current = ramp.next(current: current, target: 5_000_000, maximumIncrease: 250_000, now: 0)!
        XCTAssertEqual(current, 250_000)
        for tick in 1...200 {
            let now = UInt64(tick) * 20_000_000
            if tick % 10 == 0 { ramp.observe(ceiling: 5_000_000, now: now) }
            if let next = ramp.next(current: current, target: 5_000_000, maximumIncrease: 250_000, now: now) {
                XCTAssertLessThanOrEqual(next - current, 250_000)
                XCTAssertLessThanOrEqual(next, 5_000_000)
                current = next
            }
        }
        XCTAssertEqual(current, 5_000_000)
    }

    func testIncreaseCadenceAndFreshness() {
        var ramp = WagaBitrateRamp()
        XCTAssertNil(ramp.next(current: 250_000, target: 5_000_000, maximumIncrease: 250_000, now: 0))
        ramp.observe(ceiling: 5_000_000, now: 0)
        XCTAssertEqual(ramp.next(current: 250_000, target: 5_000_000, maximumIncrease: 250_000, now: 0), 500_000)
        for tick in 1..<10 {
            XCTAssertNil(ramp.next(current: 500_000, target: 5_000_000, maximumIncrease: 250_000, now: UInt64(tick) * 20_000_000))
        }
        XCTAssertEqual(ramp.next(current: 500_000, target: 5_000_000, maximumIncrease: 250_000, now: 200_000_000), 750_000)
        XCTAssertNil(ramp.next(current: 750_000, target: 5_000_000, maximumIncrease: 250_000, now: 1_000_000_001))
    }

    func testCeilingTargetAndStepCap() {
        var ramp = WagaBitrateRamp()
        ramp.observe(ceiling: 5_000_000, now: 0)
        XCTAssertEqual(ramp.next(current: 250_000, target: 5_000_000, maximumIncrease: 50_000, now: 0), 300_000)
        XCTAssertEqual(ramp.next(current: 300_000, target: 310_000, maximumIncrease: 250_000, now: 400_000_000), 310_000)
        ramp.observe(ceiling: 315_000, now: 800_000_000)
        XCTAssertEqual(ramp.next(current: 310_000, target: 5_000_000, maximumIncrease: 250_000, now: 800_000_000), 315_000)
    }

    func testReductionCadenceAndSevereDrop() {
        var ramp = WagaBitrateRamp()
        ramp.observe(ceiling: 4_000_000, now: 0)
        XCTAssertEqual(ramp.next(current: 5_000_000, target: 5_000_000, maximumIncrease: 250_000, now: 0), 4_000_000)
        ramp.observe(ceiling: 3_000_000, now: 20_000_000)
        XCTAssertNil(ramp.next(current: 4_000_000, target: 5_000_000, maximumIncrease: 250_000, now: 20_000_000))
        XCTAssertEqual(ramp.next(current: 4_000_000, target: 5_000_000, maximumIncrease: 250_000, now: 200_000_000), 3_000_000)
        ramp.observe(ceiling: 250_000, now: 220_000_000)
        XCTAssertEqual(ramp.next(current: 3_000_000, target: 5_000_000, maximumIncrease: 250_000, now: 220_000_000), 250_000)
        ramp.observe(ceiling: 5_000_000, now: 240_000_000)
        XCTAssertNil(ramp.next(current: 250_000, target: 5_000_000, maximumIncrease: 250_000, now: 240_000_000))
        XCTAssertNil(ramp.next(current: 250_000, target: 5_000_000, maximumIncrease: 250_000, now: 419_999_999))
        XCTAssertEqual(ramp.next(current: 250_000, target: 5_000_000, maximumIncrease: 250_000, now: 420_000_000), 500_000)
    }
}
