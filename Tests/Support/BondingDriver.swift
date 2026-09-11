import CryptoKit
import Foundation

@main
struct BondingDriver {
    static func main() {
        var delivery = WagaDelivery()
        var recovery = WagaRecovery()
        var ramp = WagaBitrateRamp()
        var rtt = ["wifi": 30.0, "cell": 80.0]
        while let line = readLine() {
            let fields = line.split(separator: " ")
            var result = "ok"
            switch fields[0] {
            case "reset":
                delivery = WagaDelivery()
                recovery = WagaRecovery()
                ramp = WagaBitrateRamp()
                rtt = ["wifi": 30.0, "cell": 80.0]
            case "rtt":
                let path = String(fields[1])
                rtt[path] = rtt[path]! * 0.8 + Double(fields[2])! * 0.2
            case "send":
                let now = UInt64(fields[1])!
                let data = decode(fields[2])
                delivery.expire(now: now)
                var scheduler = WagaPathScheduler()
                scheduler.replace(["wifi", "cell"].map { id in
                    var score = WagaPathScore(id: id, priority: id == "wifi" ? 10 : 9,
                                              smoothedRttMilliseconds: rtt[id],
                                              pendingBytes: 0)
                    let window = delivery.paths[id]?.window ?? 32_000
                    score.deliveryLoad = Double(delivery.paths[id]?.outstanding ?? 0) / Double(window)
                    score.deliveryWindow = window
                    return score
                })
                let routes = delivery.routes(scheduler, bytes: data.count, now: now)
                if let tracked = routes.last { delivery.record(data, path: tracked, remote: "server", now: now) }
                result = routes.joined(separator: ",")
                if let parity = recovery.record(data) {
                    scheduler.replace(scheduler.paths.map { previous in
                        var score = previous
                        let window = delivery.paths[score.id]?.window ?? 32_000
                        score.deliveryLoad = Double(delivery.paths[score.id]?.outstanding ?? 0) / Double(window)
                        score.deliveryWindow = window
                        return score
                    })
                    if let path = scheduler.select(bytes: parity.count) {
                        result += " \(path) \(encode(parity))"
                    }
                }
            case "ack":
                let data = decode(fields[3])
                var receipt = Data([0x57, 0x47, 0x52, 0x31, 3, 1, 0, 0])
                receipt.append(contentsOf: SHA256.hash(data: data).prefix(16))
                _ = delivery.receive(receipt, path: String(fields[2]), remote: "server", now: UInt64(fields[1])!)
            case "rate":
                let now = UInt64(fields[1])!
                if let estimate = UInt64(fields[2]) {
                    let budget = estimate * 85 / 100
                    ramp.observe(ceiling: max(250_000, budget > 160_000 ? budget - 160_000 : 0), now: now)
                }
                let current = UInt64(fields[3])!
                result = String(ramp.next(current: current, target: 5_000_000,
                                          maximumIncrease: 250_000, now: now) ?? current)
            default:
                fatalError("Unknown simulation command")
            }
            FileHandle.standardOutput.write(Data((result + "\n").utf8))
        }
    }

    static func decode(_ text: Substring) -> Data {
        let bytes = Array(text.utf8)
        func digit(_ byte: UInt8) -> UInt8 { byte <= 57 ? byte - 48 : byte - 87 }
        return Data(stride(from: 0, to: bytes.count, by: 2).map { digit(bytes[$0]) * 16 + digit(bytes[$0 + 1]) })
    }

    static func encode(_ data: Data) -> String {
        let digits = Array("0123456789abcdef".utf8)
        return String(decoding: data.flatMap { [digits[Int($0 >> 4)], digits[Int($0 & 15)]] }, as: UTF8.self)
    }
}
