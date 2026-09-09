import Foundation

/// Applies encoder timing to a bandwidth ceiling; does not estimate network capacity.
public struct WagaBitrateRamp: Sendable {
    private var ceiling: UInt64?
    private var receivedAt: UInt64 = 0
    private var increasedAt: UInt64?
    private var decreasedAt: UInt64?

    public init() {}

    public mutating func observe(ceiling: UInt64, now: UInt64) {
        self.ceiling = ceiling
        receivedAt = now
    }

    /// Times are monotonic nanoseconds. The application supplies its configured step cap.
    public mutating func next(current: UInt64, target: UInt64, maximumIncrease: UInt64,
                              now: UInt64) -> UInt64? {
        guard let ceiling, now >= receivedAt, now - receivedAt <= 1_000_000_000 else { return nil }
        let limit = min(ceiling, target)
        if limit < current {
            if limit > current / 2, let decreasedAt,
               now < decreasedAt || now - decreasedAt < 200_000_000 { return nil }
            decreasedAt = now
            increasedAt = now
            return limit
        }
        guard limit > current else { return nil }
        if let increasedAt, now < increasedAt || now - increasedAt < 200_000_000 { return nil }
        let step = maximumIncrease
        guard step > 0 else { return nil }
        increasedAt = now
        return current + min(step, limit - current)
    }
}
