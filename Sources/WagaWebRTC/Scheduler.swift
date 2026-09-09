import Foundation

struct WagaHandoff: Sendable {
    private(set) var primary: String?
    private var changedAt: UInt64?
    private var windowStarted: UInt64?
    private var sentBytes: [String: Int] = [:]
    private var dominant: String?

    mutating func sent(on path: String, bytes: Int, now: UInt64) {
        if primary == nil { primary = path }
        if let start = windowStarted, now >= start, now - start >= 500_000_000 {
            let total = sentBytes.values.reduce(0, +)
            dominant = sentBytes.max(by: { $0.value < $1.value }).flatMap {
                $0.value * 4 >= total * 3 ? $0.key : nil
            }
            sentBytes.removeAll(keepingCapacity: true)
            windowStarted = now
        }
        if windowStarted == nil { windowStarted = now }
        sentBytes[path, default: 0] += bytes
    }

    mutating func update(_ confirmed: [String], now: UInt64) -> Bool {
        guard let dominant, dominant != primary, confirmed.contains(dominant) else { return false }
        if let primary, confirmed.contains(primary), let changedAt {
            guard now >= changedAt, now - changedAt >= 5_000_000_000 else { return false }
        }
        let changed = primary != nil
        primary = dominant
        if changed { changedAt = now }
        return changed
    }

    func probingPath(now: UInt64) -> String? {
        guard let changedAt, now >= changedAt, now - changedAt < 5_000_000_000 else { return nil }
        return primary
    }
}

package struct WagaPathScore: Equatable, Sendable {
    let id: String
    var priority: Double
    var smoothedRttMilliseconds: Double?
    var pendingBytes: Int
    var deliveryLoad: Double?

    package init(id: String, priority: Double, smoothedRttMilliseconds: Double?, pendingBytes: Int) {
        self.id = id
        self.priority = priority
        self.smoothedRttMilliseconds = smoothedRttMilliseconds
        self.pendingBytes = pendingBytes
    }
}

package struct WagaPathScheduler: Sendable {
    private(set) var paths: [WagaPathScore] = []

    package init() {}

    package mutating func replace(_ active: [WagaPathScore]) {
        paths = active
    }

    package func select() -> String? {
        guard let index = paths.indices.min(by: { score(paths[$0]) < score(paths[$1]) }) else {
            return nil
        }
        return paths[index].id
    }

    private func score(_ path: WagaPathScore) -> Double {
        let rttMilliseconds = max(path.smoothedRttMilliseconds ?? 100, 1)
        let load = path.deliveryLoad ?? 0
        // Keep new and receipt-confirmed paths on the same scale. Local send
        // completion alone does not prove that an unconfirmed path delivered data.
        return (load + Double(path.pendingBytes) / 32_000 + rttMilliseconds / 10_000)
            / max(path.priority, 0.01)
    }
}
