import CWagaWebRTC
import Foundation

public enum WagaCodec: Int32, Sendable {
    case none = 0
    case h264 = 1
    case h265 = 2
    case opus = 3
    case aac = 4
}

public enum WagaTransportMode: Sendable {
    case standard
    case bonded
}

public struct WagaConnectionPriorities: Sendable {
    public let wifi: Double
    public let cellular: Double
    public let wiredEthernet: Double

    public init(wifi: Double = 1, cellular: Double = 0.9, wiredEthernet: Double = 1) {
        self.wifi = max(wifi, 0.01)
        self.cellular = max(cellular, 0.01)
        self.wiredEthernet = max(wiredEthernet, 0.01)
    }
}

public enum WagaPeerEvent: Sendable {
    case connected
    case disconnected
    case closed
    case keyframeRequest
}

public struct WagaDatagram: Sendable {
    public let source: String
    public let destination: String
    public let data: Data
}

public struct WagaMediaFrame: Sendable {
    public let codec: WagaCodec
    public let mediaTime: UInt64
    public let clockRate: UInt32
    public let ntpMicroseconds: UInt64?
    public let senderMediaTime: UInt64
    public let data: Data
}

public struct WagaCoreError: Error, CustomStringConvertible, Sendable {
    public let description: String
}

final class WagaCore: @unchecked Sendable {
    private let peer: OpaquePointer

    init(audio: WagaCodec, video: WagaCodec, targetBitrate: UInt64? = nil) throws {
        let peer = if let targetBitrate {
            waga_peer_create_with_bwe(
                audio.rawValue,
                video.rawValue,
                targetBitrate,
                targetBitrate
            )
        } else {
            waga_peer_create(audio.rawValue, video.rawValue)
        }
        guard let peer else {
            throw WagaCoreError(description: "Could not create str0m peer")
        }
        self.peer = peer
    }

    init(receiver: Void) throws {
        guard let peer = waga_receiver_create() else {
            throw WagaCoreError(description: "Could not create str0m receiver")
        }
        self.peer = peer
    }

    deinit {
        waga_peer_destroy(peer)
    }

    func addLocalCandidate(_ address: String) throws {
        try check(address.withCString { waga_peer_add_local_candidate(peer, $0) })
    }

    func removeLocalCandidate(_ address: String) throws {
        try check(address.withCString { waga_peer_remove_local_candidate(peer, $0) })
    }

    func addServerReflexiveCandidate(_ address: String, base: String) throws {
        let result = address.withCString { address in
            base.withCString { base in
                waga_peer_add_server_reflexive_candidate(peer, address, base)
            }
        }
        try check(result)
    }

    func createOffer() throws -> String {
        guard let value = waga_peer_create_offer(peer) else {
            throw lastError()
        }
        defer { waga_string_destroy(value) }
        return String(cString: value)
    }

    func acceptAnswer(_ sdp: String) throws {
        try check(sdp.withCString { waga_peer_accept_answer(peer, $0) })
    }

    func createReceiveOffer() throws -> String {
        try takeString(waga_peer_create_receive_offer(peer))
    }

    func acceptOffer(_ sdp: String) throws -> String {
        try sdp.withCString { try takeString(waga_peer_accept_offer(peer, $0)) }
    }

    func receive(source: String, destination: String, data: Data) throws {
        let result = source.withCString { source in
            destination.withCString { destination in
                data.withUnsafeBytes { bytes in
                    waga_peer_receive(
                        peer,
                        source,
                        destination,
                        bytes.bindMemory(to: UInt8.self).baseAddress,
                        bytes.count
                    )
                }
            }
        }
        try check(result)
    }

    func handleTimeout() throws {
        try check(waga_peer_handle_timeout(peer))
    }

    func timeoutMilliseconds() -> UInt64 {
        waga_peer_timeout_millis(peer)
    }

    func send(codec: WagaCodec, mediaTime: UInt64, data: Data) throws {
        let result = data.withUnsafeBytes { bytes in
            waga_peer_send(
                peer,
                codec.rawValue,
                mediaTime,
                bytes.bindMemory(to: UInt8.self).baseAddress,
                bytes.count
            )
        }
        try check(result)
    }

    func setDesiredBitrate(_ bitrate: UInt64) throws {
        try check(waga_peer_set_desired_bitrate(peer, bitrate))
    }

    func pollTransmit() -> WagaDatagram? {
        var output = WagaTransmit()
        guard waga_peer_poll_transmit(peer, &output),
              let source = output.source,
              let destination = output.destination,
              let data = output.data
        else {
            return nil
        }
        return WagaDatagram(
            source: String(cString: source),
            destination: String(cString: destination),
            data: Data(bytes: data, count: output.length)
        )
    }

    func pollEvent() -> WagaPeerEvent? {
        switch waga_peer_poll_event(peer) {
        case 1: .connected
        case 2: .disconnected
        case 3: .closed
        case 4: .keyframeRequest
        default: nil
        }
    }

    func pollMedia() -> WagaMediaFrame? {
        var output = WagaMedia()
        guard waga_peer_poll_media(peer, &output),
              let codec = WagaCodec(rawValue: output.codec),
              let data = output.data
        else {
            return nil
        }
        return WagaMediaFrame(
            codec: codec,
            mediaTime: output.media_time,
            clockRate: output.clock_rate,
            ntpMicroseconds: output.ntp_micros == .max ? nil : output.ntp_micros,
            senderMediaTime: output.sender_media_time,
            data: Data(bytes: data, count: output.length)
        )
    }

    func pollBitrateEstimate() -> UInt64? {
        var estimate: UInt64 = 0
        return waga_peer_poll_bitrate_estimate(peer, &estimate) ? estimate : nil
    }

    private func takeString(_ value: UnsafeMutablePointer<CChar>?) throws -> String {
        guard let value else { throw lastError() }
        defer { waga_string_destroy(value) }
        return String(cString: value)
    }

    private func check(_ result: Bool) throws {
        if !result {
            throw lastError()
        }
    }

    private func lastError() -> WagaCoreError {
        let message = waga_peer_last_error(peer).map(String.init(cString:)) ?? "Unknown str0m error"
        return WagaCoreError(description: message)
    }
}
