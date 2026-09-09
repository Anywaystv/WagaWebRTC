import Foundation

struct WagaPathHealth {
    static let lifetime: UInt64 = 1_500_000_000
    private var lastSuccess: [String: UInt64] = [:]
    private var everSucceeded = false
    private var available = true

    mutating func succeeded(_ destination: String, now: UInt64) {
        lastSuccess[destination] = now
        everSucceeded = true
    }

    mutating func invalidate(_ destination: String) {
        lastSuccess.removeValue(forKey: destination)
    }

    func isLive(_ destination: String, now: UInt64) -> Bool {
        guard let last = lastSuccess[destination], now >= last else { return false }
        return now - last < Self.lifetime
    }

    mutating func availabilityChange(now: UInt64) -> Bool? {
        guard everSucceeded else { return nil }
        let live = lastSuccess.keys.contains { isLive($0, now: now) }
        guard live != available else { return nil }
        available = live
        return live
    }

    func destination(preferred: String, candidates: [String], now: UInt64) -> String? {
        if isLive(preferred, now: now) { return preferred }
        return candidates.sorted().first { isLive($0, now: now) }
    }
}
