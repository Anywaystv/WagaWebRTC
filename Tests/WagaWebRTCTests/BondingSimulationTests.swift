import CryptoKit
import Foundation
@testable import WagaWebRTC
import XCTest

final class BondingSimulationTests: XCTestCase {
    func testFiveMbpsAcrossUnequalPaths() {
        let network = BondingSimulation(wifi: 6_000_000, cellular: 1_000_000)
        network.run(seconds: 8)
        XCTAssertGreaterThan(network.receivedFraction, 0.98)
        XCTAssertGreaterThan(network.links["wifi"]!.delivered, network.links["cell"]!.delivered * 3)
    }

    func testClearlyFasterPathCarriesAlmostAllTrafficWhenItHasCapacity() {
        for fast in ["wifi", "cell"] {
            let network = BondingSimulation(wifi: fast == "wifi" ? 6_000_000 : 1_000_000,
                                            cellular: fast == "cell" ? 6_000_000 : 1_000_000,
                                            cellularRtt: fast == "cell" ? 20_000_000 : 120_000_000,
                                            wifiRtt: fast == "wifi" ? 20_000_000 : 120_000_000)
            network.run(seconds: 8)
            let delivered = network.links.values.reduce(0) { $0 + $1.delivered }
            XCTAssertGreaterThan(network.receivedFraction, 0.98)
            XCTAssertGreaterThan(Double(network.links[fast]!.delivered) / Double(delivered), 0.95,
                                 "The stronger \(fast) path should carry over 95% when it can sustain the stream")
        }
    }

    func testCapacityAggregatesWhenNeitherPathCanCarryFiveMbps() {
        let network = BondingSimulation(wifi: 3_000_000, cellular: 3_000_000)
        network.run(seconds: 12)
        XCTAssertGreaterThan(network.receivedFraction, 0.95)
        for link in network.links.values {
            XCTAssertGreaterThan(link.delivered, 1_000_000)
        }
    }

    func testTrafficMovesToCellularWhenWifiCapacityFalls() {
        let network = BondingSimulation(wifi: 6_000_000, cellular: 6_000_000)
        network.run(seconds: 4)
        network.links["wifi"]!.bitrate = 500_000
        network.run(seconds: 4)
        let received = network.received
        let sent = network.sent
        let wifi = network.links["wifi"]!.delivered
        let cellular = network.links["cell"]!.delivered
        network.run(seconds: 4)
        XCTAssertGreaterThan(Double(network.received - received) / Double(network.sent - sent), 0.95)
        XCTAssertGreaterThan(network.links["cell"]!.delivered - cellular,
                             (network.links["wifi"]!.delivered - wifi) * 4)
    }

    func testHighLatencyCellularStillSuppliesNeededCapacity() {
        let network = BondingSimulation(wifi: 1_000_000, cellular: 6_000_000,
                                        cellularRtt: 300_000_000)
        network.run(seconds: 8)
        let received = network.received
        let sent = network.sent
        let cellular = network.links["cell"]!.delivered
        network.run(seconds: 4)
        XCTAssertGreaterThan(Double(network.received - received) / Double(network.sent - sent), 0.95)
        XCTAssertGreaterThan(network.links["cell"]!.delivered - cellular, 1_800_000)
    }

    func testCellularLossAndWifiRemovalRecoverWithoutResettingDelivery() {
        let network = BondingSimulation(wifi: 6_000_000, cellular: 6_000_000)
        network.run(seconds: 4)
        network.links["cell"]!.dropping = true
        network.run(seconds: 4)
        let before = network.received
        let sent = network.sent
        network.run(seconds: 4)
        XCTAssertGreaterThan(Double(network.received - before) / Double(network.sent - sent), 0.95)

        network.links["cell"]!.dropping = false
        network.links["wifi"]!.active = false
        network.delivery.removePath("wifi")
        network.run(seconds: 4)
        let cellular = network.links["cell"]!.delivered
        network.run(seconds: 4)
        XCTAssertGreaterThan(network.links["cell"]!.delivered - cellular, 2_200_000)

        network.links["wifi"]!.active = true
        network.run(seconds: 4)
        let wifi = network.links["wifi"]!.delivered
        network.run(seconds: 4)
        XCTAssertGreaterThan(network.links["wifi"]!.delivered - wifi, 1_500_000)
    }
}

// Tests packet assignment with finite link queues and delayed delivery receipts.
// The encoder rate is fixed here; this does not simulate WebRTC's shared BWE.
private final class BondingSimulation {
    struct Link {
        var bitrate: UInt64
        let rtt: UInt64
        var active = true
        var dropping = false
        var availableAt: UInt64 = 0
        var delivered = 0
    }

    var links: [String: Link]
    var delivery = WagaDelivery()
    private var pending: [(data: Data, path: String, due: UInt64)] = []
    private var seen = Set<Data>()
    private var now: UInt64 = 1
    private var budget = 0
    private(set) var sent = 0
    private(set) var received = 0

    var receivedFraction: Double { Double(received) / Double(sent) }

    init(wifi: UInt64, cellular: UInt64, cellularRtt: UInt64 = 120_000_000,
         wifiRtt: UInt64 = 20_000_000) {
        links = ["wifi": Link(bitrate: wifi, rtt: wifiRtt),
                 "cell": Link(bitrate: cellular, rtt: cellularRtt)]
    }

    func run(seconds: Int) {
        for _ in 0 ..< seconds * 1000 {
            now += 1_000_000
            for packet in pending where packet.due <= now {
                var receipt = Data([0x57, 0x47, 0x52, 0x31, 3, 1, 0, 0])
                receipt.append(contentsOf: SHA256.hash(data: packet.data).prefix(16))
                _ = delivery.receive(receipt, path: packet.path, remote: "server", now: now)
                links[packet.path]!.delivered += packet.data.count
                if seen.insert(packet.data).inserted { received += packet.data.count }
            }
            pending.removeAll { $0.due <= now }
            delivery.expire(now: now)
            budget += 625 // 5 Mbps at one millisecond per tick.
            while budget >= 1200 {
                budget -= 1200
                var sequence = sent.bigEndian
                var packet = withUnsafeBytes(of: &sequence) { Data($0) }
                packet.append(Data(count: 1200 - packet.count))
                sent += packet.count
                var scheduler = WagaPathScheduler()
                scheduler.replace(["wifi", "cell"].compactMap { id in
                    let link = links[id]!
                    guard link.active else { return nil }
                    var score = WagaPathScore(id: id, priority: id == "wifi" ? 10 : 9,
                                              smoothedRttMilliseconds: Double(link.rtt) / 1_000_000,
                                              pendingBytes: 0)
                    let window = delivery.paths[id]?.window ?? 32_000
                    score.deliveryLoad = Double(delivery.paths[id]?.outstanding ?? 0) / Double(window)
                    score.deliveryWindow = window
                    return score
                })
                let routes = delivery.routes(scheduler, bytes: packet.count, now: now)
                if let tracked = routes.last {
                    delivery.record(packet, path: tracked, remote: "server", now: now)
                }
                for id in routes {
                    var link = links[id]!
                    let departure = max(now, link.availableAt) + UInt64(packet.count) * 8_000_000_000 / link.bitrate
                    guard !link.dropping, departure - now <= 100_000_000 else { continue }
                    link.availableAt = departure
                    links[id] = link
                    pending.append((packet, id, departure + link.rtt))
                }
            }
        }
    }
}
