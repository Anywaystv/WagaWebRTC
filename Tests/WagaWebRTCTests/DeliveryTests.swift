import CryptoKit
import Foundation
@testable import WagaWebRTC
import XCTest

final class DeliveryTests: XCTestCase {
    func testReceiptsRequireMatchingPacketAndBothAddresses() {
        var delivery = WagaDelivery()
        let packet = Data(repeating: 42, count: 1200)
        delivery.record(packet, path: "wifi", remote: "server", now: 1)
        let receipt = receiptFor(packet)
        XCTAssertTrue(delivery.receive(receipt, path: "cell", remote: "server", now: 2))
        XCTAssertTrue(delivery.receive(receipt, path: "wifi", remote: "other", now: 2))
        XCTAssertFalse(delivery.paths["wifi"]!.confirmed)
        XCTAssertEqual(delivery.paths["wifi"]?.outstanding, 1200)
        XCTAssertTrue(delivery.receive(receipt, path: "wifi", remote: "server", now: 3))
        XCTAssertTrue(delivery.paths["wifi"]!.confirmed)
        XCTAssertEqual(delivery.paths["wifi"]?.outstanding, 0)
        let window = delivery.paths["wifi"]?.window
        delivery.record(packet, path: "wifi", remote: "server", now: 4)
        XCTAssertTrue(delivery.receive(receipt, path: "wifi", remote: "server", now: 5))
        XCTAssertEqual(delivery.paths["wifi"]?.window, window)
        XCTAssertEqual(delivery.paths["wifi"]?.outstanding, 0)
    }

    func testLossFillsOnlyTheAffectedPathAndExpires() {
        var delivery = WagaDelivery()
        for path in ["wifi", "cell"] {
            let packet = Data(path.utf8)
            delivery.record(packet, path: path, remote: "server", now: 1)
            XCTAssertTrue(delivery.receive(receiptFor(packet), path: path, remote: "server", now: 2))
        }
        for index in 0 ..< 26 {
            let packet = Data(repeating: UInt8(index), count: 1200)
            delivery.record(packet, path: "wifi", remote: "server", now: 3)
        }
        XCTAssertGreaterThan(delivery.paths["wifi"]!.outstanding + 1200, delivery.paths["wifi"]!.window)
        XCTAssertEqual(delivery.paths["cell"]!.outstanding, 0)
        delivery.expire(now: WagaDelivery.lifetime + 4)
        XCTAssertEqual(delivery.paths["wifi"]?.outstanding, 0)
        XCTAssertEqual(delivery.paths["wifi"]?.window, 16_000)
    }

    func testLegacyReceiverNeverNeedsReceiptsAndHistoryIsBounded() {
        var delivery = WagaDelivery()
        for index in 0 ..< 10_000 {
            var value = index.bigEndian
            let packet = withUnsafeBytes(of: &value) { Data($0) }
            delivery.record(packet, path: "wifi", remote: "server", now: 1)
        }
        XCTAssertLessThanOrEqual(delivery.paths["wifi"]!.outstanding, 4096 * MemoryLayout<Int>.size)
        delivery.expire(now: WagaDelivery.lifetime + 1)
        XCTAssertEqual(delivery.paths["wifi"]?.outstanding, 0)
    }

    func testMalformedAndExpiredReceiptsCannotOpenWindow() {
        var delivery = WagaDelivery()
        let packet = Data([1, 2, 3])
        delivery.record(packet, path: "wifi", remote: "server", now: 1)
        var receipt = receiptFor(packet)
        receipt[5] = 9
        XCTAssertTrue(delivery.receive(receipt, path: "wifi", remote: "server", now: 2))
        XCTAssertFalse(delivery.paths["wifi"]!.confirmed)
        XCTAssertTrue(delivery.receive(receiptFor(packet), path: "wifi", remote: "server",
                                       now: WagaDelivery.lifetime + 2))
        XCTAssertFalse(delivery.paths["wifi"]!.confirmed)
    }

    func testRemovedPathCannotAcknowledgeOrChargeItsReplacement() {
        var delivery = WagaDelivery()
        let old = Data([1])
        delivery.record(old, path: "wifi", remote: "server", now: 1)
        delivery.removePath("wifi")
        delivery.record(Data([2]), path: "wifi", remote: "server", now: 100)
        XCTAssertTrue(delivery.receive(receiptFor(old), path: "wifi", remote: "server", now: 101))
        XCTAssertFalse(delivery.paths["wifi"]!.confirmed)
        delivery.expire(now: WagaDelivery.lifetime + 2)
        XCTAssertEqual(delivery.paths["wifi"]?.outstanding, 1)
    }

    func testRevalidationDoesNotRestoreAnUnprovenFullWindow() {
        var delivery = WagaDelivery()
        let confirmed = Data([1])
        delivery.record(confirmed, path: "cell", remote: "server", now: 1)
        _ = delivery.receive(receiptFor(confirmed), path: "cell", remote: "server", now: 2)
        for index in 2...3 {
            delivery.record(Data([UInt8(index)]), path: "cell", remote: "server",
                            now: UInt64(index - 1) * WagaDelivery.lifetime)
            delivery.expire(now: UInt64(index) * WagaDelivery.lifetime)
        }
        let reduced = delivery.paths["cell"]!.window
        XCTAssertLessThan(reduced, 16_000)
        let old = Data([4])
        delivery.record(old, path: "cell", remote: "server", now: 3_000_000_001)
        delivery.revalidatePath("cell", now: 3_100_000_001)
        XCTAssertEqual(delivery.paths["cell"]?.window, reduced)
        XCTAssertEqual(delivery.paths["cell"]?.outstanding, old.count)
        XCTAssertEqual(delivery.paths["cell"]?.receivedPackets, 0)
        _ = delivery.receive(receiptFor(old), path: "cell", remote: "server", now: 3_200_000_001)
        XCTAssertEqual(delivery.paths["cell"]?.outstanding, 0)
        XCTAssertEqual(delivery.paths["cell"]?.receivedPackets, 1)
        let fresh = Data([5])
        delivery.record(fresh, path: "cell", remote: "server", now: 3_300_000_001)
        _ = delivery.receive(receiptFor(fresh), path: "cell", remote: "server", now: 3_400_000_001)
        XCTAssertTrue(delivery.paths["cell"]!.confirmed)
    }

    func testSchedulerPrefersAvailableDeliveryWindow() {
        var wifi = WagaPathScore(id: "wifi", priority: 1, smoothedRttMilliseconds: 5, pendingBytes: 0)
        var cell = WagaPathScore(id: "cell", priority: 1, smoothedRttMilliseconds: 100, pendingBytes: 0)
        wifi.deliveryLoad = 0.9
        cell.deliveryLoad = 0.1
        var scheduler = WagaPathScheduler()
        scheduler.replace([wifi, cell])
        XCTAssertEqual(scheduler.select(), "cell")
        wifi.deliveryLoad = 0
        scheduler.replace([wifi, cell])
        XCTAssertEqual(scheduler.select(), "wifi")
    }

    func testSchedulerUsesRefreshedPendingBytesWithoutMutatingSnapshot() {
        var scheduler = WagaPathScheduler()
        XCTAssertNil(scheduler.select())
        var wifi = WagaPathScore(id: "wifi", priority: 1, smoothedRttMilliseconds: 5, pendingBytes: 0)
        let cell = WagaPathScore(id: "cell", priority: 1, smoothedRttMilliseconds: 5, pendingBytes: 1200)
        scheduler.replace([wifi, cell])
        XCTAssertEqual(scheduler.select(), "wifi")
        XCTAssertEqual(scheduler.paths, [wifi, cell])
        wifi.pendingBytes = 2400
        scheduler.replace([wifi, cell])
        XCTAssertEqual(scheduler.select(), "cell")
    }

    func testReturningWifiCompetesWithConfirmedCellular() {
        var cell = WagaPathScore(id: "cell", priority: 9, smoothedRttMilliseconds: 200, pendingBytes: 0)
        cell.deliveryLoad = 0.1
        let wifi = WagaPathScore(id: "wifi", priority: 10, smoothedRttMilliseconds: 10, pendingBytes: 0)
        var scheduler = WagaPathScheduler()
        scheduler.replace([cell, wifi])
        XCTAssertEqual(scheduler.select(), "wifi")
    }

    func testFastCellularCanBeatWifiAndCongestedWifiYields() {
        var cell = WagaPathScore(id: "cell", priority: 9, smoothedRttMilliseconds: 10, pendingBytes: 0)
        cell.deliveryLoad = 0
        var wifi = WagaPathScore(id: "wifi", priority: 10, smoothedRttMilliseconds: 200, pendingBytes: 0)
        wifi.deliveryLoad = 0
        var scheduler = WagaPathScheduler()
        scheduler.replace([wifi, cell])
        XCTAssertEqual(scheduler.select(), "cell")
        wifi.smoothedRttMilliseconds = 5
        wifi.deliveryLoad = 0.9
        scheduler.replace([wifi, cell])
        XCTAssertEqual(scheduler.select(), "cell")
    }

    func testFasterReturningPathTakesMostTrafficAtLowSendingRate() {
        for fast in ["wifi", "cell"] {
            let slow = fast == "wifi" ? "cell" : "wifi"
            var delivery = WagaDelivery()
            let first = Data([255])
            delivery.record(first, path: slow, remote: "server", now: 1)
            _ = delivery.receive(receiptFor(first), path: slow, remote: "server", now: 2)
            var pending: [(data: Data, path: String, due: UInt64)] = []
            var fastPackets = 0
            for index in 0 ..< 200 {
                let now = UInt64(index + 1) * 40_000_000
                for packet in pending where packet.due <= now {
                    _ = delivery.receive(receiptFor(packet.data), path: packet.path, remote: "server", now: now)
                }
                pending.removeAll { $0.due <= now }
                var scheduler = WagaPathScheduler()
                let paths = [slow, fast].map { id in
                    var score = WagaPathScore(id: id, priority: id == "wifi" ? 10 : 9,
                                              smoothedRttMilliseconds: id == fast ? 20 : 400,
                                              pendingBytes: 0)
                    if let state = delivery.paths[id] {
                        score.deliveryLoad = Double(state.outstanding) / Double(state.window)
                    }
                    score.deliveryWindow = delivery.paths[id]?.window ?? 32_000
                    return score
                }
                scheduler.replace(paths)
                let selected = scheduler.select()!
                if selected == fast { fastPackets += 1 }
                let data = Data(repeating: UInt8(index), count: 1200)
                delivery.record(data, path: selected, remote: "server", now: now)
                pending.append((data, selected, now + (selected == fast ? 20_000_000 : 400_000_000)))
            }
            XCTAssertGreaterThan(fastPackets, 160, "The faster returning \(fast) path should carry most traffic")
        }
    }

    private func receiptFor(_ packet: Data) -> Data {
        var receipt = Data([0x57, 0x47, 0x52, 0x31, 3, 1, 0, 0])
        receipt.append(contentsOf: SHA256.hash(data: packet).prefix(16))
        return receipt
    }

    func testIdleSampleKeepsPrimaryCopyAndCannotBorrowItsReceipt() {
        var delivery = WagaDelivery()
        let first = Data([1])
        delivery.record(first, path: "wifi", remote: "server", now: 1)
        _ = delivery.receive(receiptFor(first), path: "wifi", remote: "server", now: 2)
        var scheduler = WagaPathScheduler()
        scheduler.replace([
            WagaPathScore(id: "wifi", priority: 1, smoothedRttMilliseconds: 10, pendingBytes: 0),
            WagaPathScore(id: "cell", priority: 1, smoothedRttMilliseconds: 400, pendingBytes: 0),
        ])
        let now: UInt64 = 300_000_000
        XCTAssertEqual(delivery.routes(scheduler, bytes: 1200, now: now), ["wifi", "cell"])
        let sample = Data(repeating: 2, count: 1200)
        delivery.record(sample, path: "cell", remote: "server", now: now)
        _ = delivery.receive(receiptFor(sample), path: "wifi", remote: "server", now: now + 1)
        XCTAssertFalse(delivery.paths["cell"]!.confirmed)
        XCTAssertEqual(delivery.paths["cell"]!.outstanding, 1200)
        XCTAssertEqual(delivery.routes(scheduler, bytes: 1200, now: now + 249_000_000), ["wifi"])
        XCTAssertEqual(delivery.routes(scheduler, bytes: 1200, now: now + 250_000_000), ["wifi", "cell"])
        _ = delivery.receive(receiptFor(sample), path: "cell", remote: "server", now: now + 400_000_000)
        XCTAssertTrue(delivery.paths["cell"]!.confirmed)
        XCTAssertEqual(delivery.paths["cell"]!.outstanding, 0)
    }

    func testIdleSamplingDoesNotDuplicateSinglePathOrLegacyTraffic() {
        var delivery = WagaDelivery()
        var scheduler = WagaPathScheduler()
        let wifi = WagaPathScore(id: "wifi", priority: 1, smoothedRttMilliseconds: 10, pendingBytes: 0)
        let cell = WagaPathScore(id: "cell", priority: 1, smoothedRttMilliseconds: 100, pendingBytes: 0)
        XCTAssertEqual(delivery.routes(scheduler, bytes: 1200, now: 1_000_000_000), [])
        scheduler.replace([wifi, cell])
        XCTAssertEqual(delivery.routes(scheduler, bytes: 1200, now: 1_000_000_000), ["wifi"])
        let packet = Data([1])
        delivery.record(packet, path: "wifi", remote: "server", now: 1)
        _ = delivery.receive(receiptFor(packet), path: "wifi", remote: "server", now: 2)
        scheduler.replace([wifi])
        XCTAssertEqual(delivery.routes(scheduler, bytes: 1200, now: 1_000_000_000), ["wifi"])
    }

    func testDeadIdlePathCannotStealMediaFromHealthyPath() {
        for healthy in ["wifi", "cell"] {
            let dead = healthy == "wifi" ? "cell" : "wifi"
            var delivery = WagaDelivery()
            let first = Data([255])
            delivery.record(first, path: healthy, remote: "server", now: 1)
            _ = delivery.receive(receiptFor(first), path: healthy, remote: "server", now: 2)
            var samples = 0
            for index in 0 ..< 200 {
                let now = UInt64(index + 1) * 40_000_000
                delivery.expire(now: now)
                var scheduler = WagaPathScheduler()
                scheduler.replace([healthy, dead].map { id in
                    var score = WagaPathScore(id: id, priority: 1,
                                              smoothedRttMilliseconds: id == healthy ? 10 : 400,
                                              pendingBytes: 0)
                    if let state = delivery.paths[id] {
                        score.deliveryLoad = Double(state.outstanding) / Double(state.window)
                    }
                    score.deliveryWindow = delivery.paths[id]?.window ?? 32_000
                    return score
                })
                let routes = delivery.routes(scheduler, bytes: 1200, now: now)
                XCTAssertEqual(routes.first, healthy)
                if routes.contains(dead) { samples += 1 }
                let data = Data(repeating: UInt8(index), count: 1200)
                delivery.record(data, path: routes.last!, remote: "server", now: now)
                _ = delivery.receive(receiptFor(data), path: healthy, remote: "server", now: now + 10_000_000)
            }
            XCTAssertGreaterThan(samples, 0)
            XCTAssertLessThanOrEqual(samples, 32)
            XCTAssertFalse(delivery.paths[dead]!.confirmed)
        }
    }

    func testConfirmedFasterPathTriggersOneHandoffInEitherDirection() {
        for fast in ["wifi", "cell"] {
            let slow = fast == "wifi" ? "cell" : "wifi"
            var handoff = WagaHandoff()
            handoff.sent(on: slow, bytes: 1200, now: 0)
            for tick in 1...15 {
                handoff.sent(on: fast, bytes: 1200, now: UInt64(tick) * 40_000_000)
            }
            XCTAssertFalse(handoff.update([slow], now: 600_000_000))
            XCTAssertTrue(handoff.update([slow, fast], now: 600_000_000))
            XCTAssertEqual(handoff.primary, fast)
            XCTAssertFalse(handoff.update([slow, fast], now: 600_000_002))
        }
    }

    func testHandoffRejectsBalancedRoutingAndDoesNotFlap() {
        var handoff = WagaHandoff()
        for tick in 0...15 {
            handoff.sent(on: tick % 2 == 0 ? "wifi" : "cell", bytes: 1200, now: UInt64(tick) * 40_000_000)
            XCTAssertFalse(handoff.update(["wifi", "cell"], now: UInt64(tick) * 40_000_000))
        }
        for tick in 16...40 {
            handoff.sent(on: "cell", bytes: 1200, now: UInt64(tick) * 40_000_000)
        }
        XCTAssertTrue(handoff.update(["wifi", "cell"], now: 1_600_000_000))
        for tick in 41...65 {
            handoff.sent(on: "wifi", bytes: 1200, now: UInt64(tick) * 40_000_000)
        }
        XCTAssertFalse(handoff.update(["wifi", "cell"], now: 2_600_000_000))
        XCTAssertTrue(handoff.update(["wifi"], now: 2_600_000_001))
    }

    func testFullWindowStillRoutesMediaAndRepairs() {
        var cell = WagaPathScore(id: "cell", priority: 1, smoothedRttMilliseconds: 450, pendingBytes: 0)
        cell.deliveryWindow = 32_000
        cell.deliveryLoad = 4
        var scheduler = WagaPathScheduler()
        scheduler.replace([cell])
        let delivery = WagaDelivery()
        XCTAssertEqual(delivery.routes(scheduler, bytes: 1200, now: 1), ["cell"])
        let wifi = WagaPathScore(id: "wifi", priority: 1, smoothedRttMilliseconds: 10, pendingBytes: 0)
        scheduler.replace([cell, wifi])
        XCTAssertEqual(delivery.routes(scheduler, bytes: 1200, now: 1), ["wifi"])
        scheduler.replace([])
        XCTAssertTrue(delivery.routes(scheduler, bytes: 1200, now: 1).isEmpty)
    }

    func testReceiptWindowRanksCapacityEvenWhenBothPathsAreIdle() {
        var wifi = WagaPathScore(id: "wifi", priority: 1, smoothedRttMilliseconds: 100, pendingBytes: 0)
        var cell = WagaPathScore(id: "cell", priority: 1, smoothedRttMilliseconds: 10, pendingBytes: 0)
        wifi.deliveryWindow = 64_000
        cell.deliveryWindow = 16_000
        var scheduler = WagaPathScheduler()
        scheduler.replace([cell, wifi])
        XCTAssertEqual(scheduler.select(), "wifi")
        wifi.deliveryLoad = 0.5
        scheduler.replace([cell, wifi])
        XCTAssertEqual(scheduler.select(), "cell")
    }

    func testReducedWindowRemovesPriorityAdvantageUntilRecovery() {
        var wifi = WagaPathScore(id: "wifi", priority: 10, smoothedRttMilliseconds: 10, pendingBytes: 0)
        var cell = WagaPathScore(id: "cell", priority: 1, smoothedRttMilliseconds: 100, pendingBytes: 0)
        wifi.deliveryWindow = 8_000
        cell.deliveryWindow = 32_000
        var scheduler = WagaPathScheduler()
        scheduler.replace([wifi, cell])
        XCTAssertEqual(scheduler.select(), "cell")
        wifi.deliveryWindow = 32_000
        scheduler.replace([wifi, cell])
        XCTAssertEqual(scheduler.select(), "wifi")
    }

    func testStartupWindowReductionDoesNotAmplifyCellularPriority() {
        // Snapshot just before the live startup shifted onto slower cellular.
        for scale in [0.1, 1.0, 10.0] {
            var wifi = WagaPathScore(id: "wifi", priority: 10 * scale,
                                     smoothedRttMilliseconds: 32, pendingBytes: 0)
            wifi.deliveryWindow = 16_527
            wifi.deliveryLoad = 1772.0 / 16_527
            var cell = WagaPathScore(id: "cell", priority: 9 * scale,
                                     smoothedRttMilliseconds: 73, pendingBytes: 0)
            cell.deliveryWindow = 32_299
            cell.deliveryLoad = 12809.0 / 32_299
            var scheduler = WagaPathScheduler()
            scheduler.replace([wifi, cell])
            XCTAssertEqual(scheduler.select(), "wifi", "Priority scale \(scale)")
        }
    }

    func testSocketPendingMediaIsNotChargedTwice() {
        var wifi = WagaPathScore(id: "wifi", priority: 1, smoothedRttMilliseconds: 20, pendingBytes: 12_000)
        var cell = WagaPathScore(id: "cell", priority: 1, smoothedRttMilliseconds: 100, pendingBytes: 0)
        wifi.deliveryWindow = 32_000
        wifi.deliveryLoad = 12_000.0 / 32_000
        cell.deliveryWindow = 32_000
        cell.deliveryLoad = 18_000.0 / 32_000
        var scheduler = WagaPathScheduler()
        scheduler.replace([wifi, cell])
        XCTAssertEqual(scheduler.select(), "wifi")
    }

    func testBusyReceiptWindowGrowsFasterThanIdleSamples() {
        var delivery = WagaDelivery()
        for index in 0 ..< 40 {
            delivery.record(Data(repeating: UInt8(index), count: 1200),
                            path: "busy", remote: "server", now: 1)
        }
        let packet = Data(repeating: 0, count: 1200)
        // Use different ciphertext: identical copies are deliberately tracked once.
        let idle = Data(repeating: 255, count: 1200)
        delivery.record(idle, path: "idle", remote: "server", now: 1)
        _ = delivery.receive(receiptFor(packet), path: "busy", remote: "server", now: 2)
        _ = delivery.receive(receiptFor(idle), path: "idle", remote: "server", now: 2)
        XCTAssertGreaterThan(delivery.paths["busy"]!.window, delivery.paths["idle"]!.window)
        XCTAssertEqual(delivery.paths["idle"]!.window, 32_001)
        let window = delivery.paths["busy"]!.window
        _ = delivery.receive(receiptFor(packet), path: "busy", remote: "server", now: 3)
        XCTAssertEqual(delivery.paths["busy"]!.window, window)
    }
}
