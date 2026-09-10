import CryptoKit
import Foundation

struct WagaDelivery: Sendable {
    struct Path: Sendable {
        var confirmed = false
        var outstanding = 0
        var window = 32_000
        var lastSent: UInt64 = 0
        var lastReduction: UInt64 = 0
        var receivedPackets = 0
        var lastReceived: UInt64 = 0
    }

    private struct Packet: Sendable {
        let path: String
        let remote: String
        let bytes: Int
        let sent: UInt64
    }

    private(set) var paths: [String: Path] = [:]
    private var packets: [Data: Packet] = [:]
    private var order: [(Data, UInt64)] = []
    private var head = 0
    private var recent = Set<Data>()
    private var recentOrder: [Data] = []
    private var recentIndex = 0
    static let lifetime: UInt64 = 1_000_000_000

    mutating func removePath(_ id: String) {
        paths.removeValue(forKey: id)
        packets = packets.filter { $0.value.path != id }
    }

    mutating func revalidatePath(_ id: String, now: UInt64) {
        expire(now: now)
        // Reachability does not clear packets still in flight or establish a
        // new capacity allowance. Require fresh receipts for handoff detection.
        paths[id]?.receivedPackets = 0
        paths[id]?.lastReceived = 0
    }

    mutating func expire(now: UInt64) {
        while head < order.count {
            let (token, sent) = order[head]
            guard now >= sent, now - sent >= Self.lifetime else { break }
            head += 1
            guard let packet = packets[token], packet.sent == sent else { continue }
            packets.removeValue(forKey: token)
            paths[packet.path]?.outstanding -= packet.bytes
            if var path = paths[packet.path], path.confirmed,
               now - path.lastReduction >= Self.lifetime {
                path.window = max(4_800, path.window / 2)
                path.lastReduction = now
                paths[packet.path] = path
            }
        }
        if head >= 4096 {
            order.removeFirst(head)
            head = 0
        }
    }

    func routes(_ scheduler: WagaPathScheduler, bytes: Int, now: UInt64) -> [String] {
        // Windows rank paths; they must not block packets already paced by str0m.
        guard let primary = scheduler.select(bytes: bytes) else { return [] }
        guard paths.values.contains(where: { $0.confirmed }),
              let sample = scheduler.paths.filter({
                  $0.id != primary && now >= (paths[$0.id]?.lastSent ?? 0)
                      && now - (paths[$0.id]?.lastSent ?? 0) >= 250_000_000
              }).min(by: {
                  (paths[$0.id]?.lastSent ?? 0) < (paths[$1.id]?.lastSent ?? 0)
              }) else { return [primary] }
        return [primary, sample.id]
    }

    mutating func record(_ data: Data, path id: String, remote: String, now: UInt64) {
        expire(now: now)
        paths[id]?.lastSent = now
        let token = Data(SHA256.hash(data: data).prefix(16))
        // Retransmitting identical ciphertext cannot produce a distinct receipt.
        guard !recent.contains(token), packets.count < 4096, order.count - head < 4096 else { return }
        if paths[id] == nil {
            guard paths.count < 256 else { return }
            paths[id] = Path()
        }
        packets[token] = Packet(path: id, remote: remote, bytes: data.count, sent: now)
        if recentOrder.count == 4096 {
            recent.remove(recentOrder[recentIndex])
            recentOrder[recentIndex] = token
            recentIndex = (recentIndex + 1) % 4096
        } else {
            recentOrder.append(token)
        }
        recent.insert(token)
        order.append((token, now))
        paths[id]?.outstanding += data.count
        paths[id]?.lastSent = now
    }

    mutating func receive(_ data: Data, path: String, remote: String, now: UInt64) -> Bool {
        guard data.count >= 5, data.prefix(4) == Data("WGR1".utf8), data[4] == 3 else { return false }
        expire(now: now)
        guard data.count >= 8, data[6] == 0, data[7] == 0,
              data[5] > 0, data[5] <= 8, data.count == 8 + Int(data[5]) * 16 else { return true }
        for index in 0 ..< Int(data[5]) {
            let offset = 8 + index * 16
            let token = data.subdata(in: offset ..< offset + 16)
            guard let packet = packets[token], packet.path == path, packet.remote == remote else { continue }
            packets.removeValue(forKey: token)
            guard var state = paths[path] else { continue }
            state.confirmed = true
            state.receivedPackets = min(state.receivedPackets + 1, 3)
            state.lastReceived = now
            state.outstanding -= packet.bytes
            // As in SRTLA, successful delivery grows a busy path faster than
            // an idle one. A trickle of idle samples must not inflate its weight.
            let increment = state.outstanding > state.window ? 30 : 1
            state.window = min(512_000, state.window + max(1, packet.bytes * increment / 1000))
            paths[path] = state
        }
        return true
    }
}
