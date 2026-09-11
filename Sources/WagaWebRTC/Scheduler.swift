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
}

package struct WagaPathScore: Equatable, Sendable {
    let id: String
    var priority: Double
    var smoothedRttMilliseconds: Double?
    var pendingBytes: Int
    var deliveryLoad: Double?
    var deliveryWindow: Int?

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

    package func select(bytes: Int = 1200) -> String? {
        let minimumPriority = paths.map { max($0.priority, 0.01) }.min() ?? 1
        let fastestRtt = paths.filter {
            $0.deliveryWindow != nil && $0.deliveryLoad != nil
        }.compactMap(\.smoothedRttMilliseconds).map { max($0, 1) }.min()
        guard let index = paths.indices.min(by: {
            let lhs = score(paths[$0], bytes: bytes, fastestRtt: fastestRtt, minimumPriority: minimumPriority)
            let rhs = score(paths[$1], bytes: bytes, fastestRtt: fastestRtt, minimumPriority: minimumPriority)
            if lhs == rhs {
                return (paths[$0].smoothedRttMilliseconds ?? 100)
                    < (paths[$1].smoothedRttMilliseconds ?? 100)
            }
            return lhs < rhs
        }) else {
            return nil
        }
        return paths[index].id
    }

    private func score(_ path: WagaPathScore, bytes: Int, fastestRtt: Double?, minimumPriority: Double) -> Double {
        // Priority is relative. Fading raw 10/9 values toward 1 can turn one
        // missed receipt into a ninefold preference for the other path.
        let relativePriority = max(path.priority, 0.01) / minimumPriority
        if let window = path.deliveryWindow {
            // SRTLA ranks window / (in-flight + next packet). Use bytes because
            // RTP audio, video and padding packets have different sizes.
            let window = Double(max(window, 1))
            let stability = min(1, max(0, (window - 16_000) / 16_000))
            let priority = 1 + (relativePriority - 1) * stability
            // Socket-pending media is already in flight in the delivery tracker.
            let load = max(path.deliveryLoad ?? 0, Double(path.pendingBytes) / window)
            var latencyCost = 0.0
            if path.deliveryLoad != nil, let rtt = path.smoothedRttMilliseconds, let fastestRtt {
                // Favor quick delivery while idle. Outstanding traffic already
                // includes RTT, so latency is a floor rather than another charge.
                latencyCost = max(0, 1 - fastestRtt / max(rtt, 1))
            }
            return max(load + Double(max(bytes, 1)) / window, latencyCost) / priority
        }
        let rttMilliseconds = max(path.smoothedRttMilliseconds ?? 100, 1)
        let load = path.deliveryLoad ?? 0
        // Keep new and receipt-confirmed paths on the same scale. Local send
        // completion alone does not prove that an unconfirmed path delivered data.
        return (load + Double(path.pendingBytes) / 32_000 + rttMilliseconds / 10_000)
            / relativePriority
    }
}
