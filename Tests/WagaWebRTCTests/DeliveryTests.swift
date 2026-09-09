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
            XCTAssertTrue(delivery.allows("wifi", bytes: packet.count))
            delivery.record(packet, path: "wifi", remote: "server", now: 3)
        }
        XCTAssertFalse(delivery.allows("wifi", bytes: 1200))
        XCTAssertTrue(delivery.allows("cell", bytes: 1200))
        delivery.expire(now: WagaDelivery.lifetime + 4)
        XCTAssertEqual(delivery.paths["wifi"]?.outstanding, 0)
        XCTAssertEqual(delivery.paths["wifi"]?.window, 16_000)
        XCTAssertTrue(delivery.allows("wifi", bytes: 1200))
    }

    func testLegacyReceiverNeverNeedsReceiptsAndHistoryIsBounded() {
        var delivery = WagaDelivery()
        for index in 0 ..< 10_000 {
            var value = index.bigEndian
            let packet = withUnsafeBytes(of: &value) { Data($0) }
            delivery.record(packet, path: "wifi", remote: "server", now: 1)
        }
        XCTAssertTrue(delivery.allows("wifi", bytes: 1200))
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

    private func receiptFor(_ packet: Data) -> Data {
        var receipt = Data([0x57, 0x47, 0x52, 0x31, 3, 1, 0, 0])
        receipt.append(contentsOf: SHA256.hash(data: packet).prefix(16))
        return receipt
    }

    func testFullCellularWindowStillRoutesMediaAndRepairs() {
        var delivery = WagaDelivery()
        let first = Data([99])
        delivery.record(first, path: "cell", remote: "server", now: 1)
        XCTAssertTrue(delivery.receive(receiptFor(first), path: "cell", remote: "server", now: 2))
        let cell = WagaPathScore(id: "cell", priority: 1, smoothedRttMilliseconds: 450, pendingBytes: 0)
        var scheduler = WagaPathScheduler()
        for index in 0 ..< 200 {
            let packet = Data(repeating: UInt8(index), count: 1200)
            scheduler.replace(delivery.preferredPaths([cell], bytes: packet.count))
            XCTAssertEqual(scheduler.select(), "cell")
            delivery.record(packet, path: "cell", remote: "server", now: UInt64(index + 3))
        }
        XCTAssertFalse(delivery.allows("cell", bytes: 1200))
        XCTAssertEqual(delivery.preferredPaths([cell], bytes: 1200).map(\.id), ["cell"])
        let wifi = WagaPathScore(id: "wifi", priority: 1, smoothedRttMilliseconds: 10, pendingBytes: 0)
        XCTAssertEqual(delivery.preferredPaths([cell, wifi], bytes: 1200).map(\.id), ["wifi"])
        XCTAssertTrue(delivery.preferredPaths([], bytes: 1200).isEmpty)
    }
}
