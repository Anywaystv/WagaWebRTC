import Foundation

private let recoveryMagic: [UInt8] = [0x57, 0x47, 0x52, 0x31]
private let parityType: UInt8 = 1
private let repairType: UInt8 = 2
private let parityGroupSize = 8
private let packetHistorySize = 4096

private struct RecoveryPacketID: Hashable {
    let ssrc: UInt32
    let sequenceNumber: UInt16
}

private struct RecoveryPacket {
    let id: RecoveryPacketID
    let data: Data
}

struct WagaRecovery: Sendable {
    private var history: [RecoveryPacketID: Data] = [:]
    private var historyOrder: [RecoveryPacketID] = []
    private var historyHead = 0
    private var groups: [UInt32: [RecoveryPacket]] = [:]

    mutating func record(_ data: Data) -> Data? {
        guard let id = rtpPacketID(data), history[id] == nil else {
            return nil
        }

        history[id] = data
        historyOrder.append(id)
        trimHistory()

        groups[id.ssrc, default: []].append(.init(id: id, data: data))
        guard groups[id.ssrc]?.count == parityGroupSize else {
            return nil
        }

        let packets = groups.removeValue(forKey: id.ssrc) ?? []
        return makeParity(packets)
    }

    mutating func repairs(for data: Data) -> [Data]? {
        guard hasRecoveryMagic(data) else {
            return nil
        }
        guard data.count >= 12, data[4] == repairType else {
            return []
        }

        let count = Int(data[5])
        guard count > 0, count <= 32, data.count == 12 + count * 2 else {
            return []
        }

        let ssrc = readUInt32(data, at: 8)
        return (0 ..< count).compactMap { index in
            history[.init(ssrc: ssrc, sequenceNumber: readUInt16(data, at: 12 + index * 2))]
        }
    }

    mutating func removeAll() {
        history.removeAll(keepingCapacity: true)
        historyOrder.removeAll(keepingCapacity: true)
        historyHead = 0
        groups.removeAll(keepingCapacity: true)
    }

    private mutating func trimHistory() {
        while history.count > packetHistorySize {
            history.removeValue(forKey: historyOrder[historyHead])
            historyHead += 1
        }
        if historyHead >= packetHistorySize {
            historyOrder.removeFirst(historyHead)
            historyHead = 0
        }
    }
}

private func rtpPacketID(_ data: Data) -> RecoveryPacketID? {
    guard isRtp(data) else {
        return nil
    }
    return .init(
        ssrc: readUInt32(data, at: 8),
        sequenceNumber: readUInt16(data, at: 2)
    )
}

private func makeParity(_ packets: [RecoveryPacket]) -> Data? {
    guard let first = packets.first, packets.count == parityGroupSize,
          let maximumLength = packets.map(\.data.count).max(), maximumLength <= UInt16.max
    else {
        return nil
    }

    var output = Data(recoveryMagic)
    output.append(parityType)
    output.append(UInt8(packets.count))
    output.append(contentsOf: [0, 0])
    appendUInt32(first.id.ssrc, to: &output)
    for packet in packets {
        appendUInt16(packet.id.sequenceNumber, to: &output)
        appendUInt16(UInt16(packet.data.count), to: &output)
    }

    var parity = [UInt8](repeating: 0, count: maximumLength)
    for packet in packets {
        packet.data.withUnsafeBytes { bytes in
            for index in bytes.indices {
                parity[index] ^= bytes[index]
            }
        }
    }
    output.append(contentsOf: parity)
    return output
}

func isRtp(_ data: Data) -> Bool {
    guard data.count >= 12, data[0] & 0xC0 == 0x80 else {
        return false
    }
    return data[1] < 192 || data[1] > 223
}

private func hasRecoveryMagic(_ data: Data) -> Bool {
    data.count >= recoveryMagic.count && data.prefix(recoveryMagic.count).elementsEqual(recoveryMagic)
}

private func readUInt16(_ data: Data, at offset: Int) -> UInt16 {
    UInt16(data[offset]) << 8 | UInt16(data[offset + 1])
}

private func readUInt32(_ data: Data, at offset: Int) -> UInt32 {
    UInt32(data[offset]) << 24 | UInt32(data[offset + 1]) << 16 |
        UInt32(data[offset + 2]) << 8 | UInt32(data[offset + 3])
}

private func appendUInt16(_ value: UInt16, to data: inout Data) {
    data.append(UInt8(value >> 8))
    data.append(UInt8(value & 0xFF))
}

private func appendUInt32(_ value: UInt32, to data: inout Data) {
    data.append(UInt8(value >> 24))
    data.append(UInt8((value >> 16) & 0xFF))
    data.append(UInt8((value >> 8) & 0xFF))
    data.append(UInt8(value & 0xFF))
}
