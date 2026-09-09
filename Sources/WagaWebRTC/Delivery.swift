import CryptoKit
import Foundation

struct WagaDelivery: Sendable {
    struct Path: Sendable {
        var confirmed = false
        var outstanding = 0
        var window = 32_000
        var lastSent: UInt64 = 0
        var lastReduction: UInt64 = 0
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

    func allows(_ id: String, bytes: Int) -> Bool {
        guard let path = paths[id], path.confirmed else { return true }
        return packets.count < 4096 && order.count - head < 4096
            && path.outstanding + bytes <= path.window
    }

    func preferredPaths(_ active: [WagaPathScore], bytes: Int) -> [WagaPathScore] {
        let available = active.filter { allows($0.id, bytes: bytes) }
        // str0m has already paced this packet. A full delivery window may
        // change its route, but must not discard media or a requested repair.
        return available.isEmpty ? active : available
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
            state.outstanding -= packet.bytes
            state.window = min(512_000, state.window + max(1, 1_200 * packet.bytes / state.window))
            paths[path] = state
        }
        return true
    }
}
