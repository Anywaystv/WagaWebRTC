import Foundation

struct WagaPathHealth {
    static let lifetime: UInt64 = 1_500_000_000
    private var lastSuccess: [String: UInt64] = [:]

    mutating func succeeded(_ destination: String, now: UInt64) {
        lastSuccess[destination] = now
    }

    mutating func invalidate(_ destination: String) {
        lastSuccess.removeValue(forKey: destination)
    }

    func isLive(_ destination: String, now: UInt64) -> Bool {
        guard let last = lastSuccess[destination], now >= last else { return false }
        return now - last < Self.lifetime
    }
}
