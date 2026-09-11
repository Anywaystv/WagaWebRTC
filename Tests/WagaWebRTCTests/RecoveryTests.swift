import Foundation
@testable import WagaWebRTC
import XCTest

final class RecoveryTests: XCTestCase {
    func testParityCoversEveryEighthPacket() {
        var recovery = WagaRecovery()

        for sequenceNumber in 1 ... 7 {
            XCTAssertNil(recovery.record(rtpPacket(sequenceNumber: UInt16(sequenceNumber))))
        }
        let parity = recovery.record(rtpPacket(sequenceNumber: 8))

        XCTAssertEqual(parity?[0 ..< 4], Data([0x57, 0x47, 0x52, 0x31]))
        XCTAssertEqual(parity?[4], 1)
        XCTAssertEqual(parity?[5], 8)
    }

    func testRepairRequestReadsSharedHistory() {
        var recovery = WagaRecovery()
        let wanted = rtpPacket(sequenceNumber: 4)
        XCTAssertNil(recovery.record(wanted))

        var request = Data([0x57, 0x47, 0x52, 0x31, 2, 1, 0, 0, 1, 2, 3, 4])
        request.append(contentsOf: [0, 4])

        XCTAssertEqual(recovery.repairs(for: request), [wanted])
    }

    func testRepeatedRepairRequestsCannotCreateAnUnpacedBurst() {
        var recovery = WagaRecovery()
        var request = Data([0x57, 0x47, 0x52, 0x31, 2, 32, 0, 0, 1, 2, 3, 4])
        for sequence in 1...32 {
            var packet = rtpPacket(sequenceNumber: UInt16(sequence))
            packet.append(Data(count: 1200 - packet.count))
            _ = recovery.record(packet)
            request.append(contentsOf: [0, UInt8(sequence)])
        }
        XCTAssertLessThanOrEqual(recovery.repairs(for: request)?.count ?? 0, 1)
        XCTAssertEqual(recovery.repairs(for: request), [])
    }

    func testRepairWaitsForReorderingAndRetainsTransportSequence() {
        var recovery = WagaRecovery()
        let packet = rtpPacket(sequenceNumber: 4)
        _ = recovery.record(packet, transportSequence: 123, now: 1)
        let request = Data([0x57, 0x47, 0x52, 0x31, 2, 1, 0, 0, 1, 2, 3, 4, 0, 4])
        XCTAssertEqual(recovery.repairs(for: request, now: 80_000_001, minimumAge: 100_000_000), [])
        XCTAssertEqual(recovery.repairs(for: request, now: 120_000_001, minimumAge: 100_000_000), [packet])
        XCTAssertEqual(recovery.transportSequence(for: packet), 123)
        XCTAssertEqual(recovery.repairs(for: request, now: 180_000_001, minimumAge: 100_000_000), [])
    }

    func testRepairBandwidthRequiresNewPrimaryTraffic() throws {
        var recovery = WagaRecovery()
        var packet = rtpPacket(sequenceNumber: 4)
        packet.append(Data(count: 1200 - packet.count))
        _ = recovery.record(packet, now: 1)
        let request = Data([0x57, 0x47, 0x52, 0x31, 2, 1, 0, 0, 1, 2, 3, 4, 0, 4])
        let firstRepair = try XCTUnwrap(recovery.repairs(for: request, now: 200_000_001))
        XCTAssertEqual(firstRepair, [packet])
        XCTAssertEqual(recovery.repairs(for: request, now: 1_000_000_001), [])
        for sequence in 5...20 {
            var next = packet
            next[3] = UInt8(sequence)
            _ = recovery.record(next, now: 1_000_000_001)
        }
        let secondRepair = try XCTUnwrap(recovery.repairs(for: request, now: 1_200_000_001))
        XCTAssertEqual(secondRepair, [packet])
        XCTAssertEqual((firstRepair + secondRepair).reduce(0) { $0 + $1.count }, 2400)
    }

    func testRtcpIsNotScheduledAsMedia() {
        var packet = rtpPacket(sequenceNumber: 1)
        packet[1] = 200

        XCTAssertFalse(isRtp(packet))
    }

    private func rtpPacket(sequenceNumber: UInt16) -> Data {
        Data([
            0x80, 111,
            UInt8(sequenceNumber >> 8), UInt8(sequenceNumber & 0xFF),
            0, 0, 0, 1,
            1, 2, 3, 4,
            UInt8(sequenceNumber & 0xFF), 0xAA,
        ])
    }
}
