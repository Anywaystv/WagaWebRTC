import XCTest
@testable import WagaWebRTC

final class BitrateRampTests: XCTestCase {
    func testIncreaseCadenceAndFreshness() {
        var ramp = WagaBitrateRamp()
        XCTAssertNil(ramp.next(current: 250_000, target: 5_000_000, maximumIncrease: 250_000, now: 0))
        ramp.observe(ceiling: 5_000_000, now: 0)
        XCTAssertEqual(ramp.next(current: 250_000, target: 5_000_000, maximumIncrease: 250_000, now: 0), 358_333)
        for tick in 1..<20 {
            XCTAssertNil(ramp.next(current: 358_333, target: 5_000_000, maximumIncrease: 250_000, now: UInt64(tick) * 20_000_000))
        }
        XCTAssertEqual(ramp.next(current: 358_333, target: 5_000_000, maximumIncrease: 250_000, now: 400_000_000), 470_277)
        XCTAssertNil(ramp.next(current: 470_277, target: 5_000_000, maximumIncrease: 250_000, now: 1_000_000_001))
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
        XCTAssertNotNil(ramp.next(current: 250_000, target: 5_000_000, maximumIncrease: 250_000, now: 620_000_000))
    }
}
