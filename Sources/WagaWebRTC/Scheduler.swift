import Foundation

package struct WagaPathScore: Equatable, Sendable {
    let id: String
    var priority: Double
    var smoothedRttMilliseconds: Double?
    var pendingBytes: Int

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

    package mutating func select(byteCount: Int) -> String? {
        guard let index = paths.indices.min(by: { score(paths[$0]) < score(paths[$1]) }) else {
            return nil
        }
        paths[index].pendingBytes += byteCount
        return paths[index].id
    }

    private func score(_ path: WagaPathScore) -> Double {
        let rttMilliseconds = path.smoothedRttMilliseconds ?? 100
        return (Double(path.pendingBytes) + rttMilliseconds * 1_500) / max(path.priority, 0.01)
    }
}
