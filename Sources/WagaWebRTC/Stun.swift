import Foundation
import CryptoKit

private let stunMagicCookie = Data([0x21, 0x12, 0xA4, 0x42])

func makeIceProbe(_ template: Data, password: String) -> Data? {
    guard template.count >= 20 else { return nil }
    var request = makeStunBindingRequest()
    var offset = 20
    while offset + 4 <= template.count {
        let type = Int(template[offset]) << 8 | Int(template[offset + 1])
        let length = Int(template[offset + 2]) << 8 | Int(template[offset + 3])
        let end = offset + 4 + ((length + 3) & ~3)
        guard end <= template.count else { return nil }
        if type == 0x0008 { break }
        // Probes check reachability without nominating a different ICE pair.
        if type != 0x0025 && type != 0x8028 {
            request.append(template[offset ..< end])
        }
        offset = end
    }
    let length = request.count - 20 + 24
    guard length <= 65535 else { return nil }
    request[2] = UInt8(length >> 8)
    request[3] = UInt8(length & 255)
    let mac = HMAC<Insecure.SHA1>.authenticationCode(for: request, using: SymmetricKey(data: Data(password.utf8)))
    request.append(contentsOf: [0, 8, 0, 20])
    request.append(contentsOf: mac)
    return request
}

func validIceIntegrity(_ data: Data, password: String) -> Bool {
    guard data.count >= 20 else { return false }
    var offset = 20
    while offset + 4 <= data.count {
        let type = Int(data[offset]) << 8 | Int(data[offset + 1])
        let length = Int(data[offset + 2]) << 8 | Int(data[offset + 3])
        let end = offset + 4 + ((length + 3) & ~3)
        guard end <= data.count else { return false }
        if type == 0x0008 {
            guard length == 20 else { return false }
            var authenticated = Data(data.prefix(offset))
            let bodyLength = end - 20
            authenticated[2] = UInt8(bodyLength >> 8)
            authenticated[3] = UInt8(bodyLength & 255)
            return HMAC<Insecure.SHA1>.isValidAuthenticationCode(
                data[(offset + 4) ..< end], authenticating: authenticated,
                using: SymmetricKey(data: Data(password.utf8))
            )
        }
        offset = end
    }
    return false
}

func isStunSuccess(_ data: Data) -> Bool {
    data.count >= 20 && data[0] == 0x01 && data[1] == 0x01 && data[4 ..< 8] == stunMagicCookie
}

func stunTransaction(_ data: Data) -> Data? {
    guard data.count >= 20, data[4 ..< 8] == stunMagicCookie else { return nil }
    return data[8 ..< 20]
}

func stunEndpoint(_ value: String) -> String? {
    var value = value
    guard !value.hasPrefix("turn:"), !value.hasPrefix("turns:") else {
        return nil
    }
    if value.hasPrefix("stun:") {
        value.removeFirst(5)
    }
    if value.hasPrefix("//") {
        value.removeFirst(2)
    }
    guard !value.isEmpty else { return nil }
    if value.first == "[" || value.contains(":") {
        return value
    }
    return "\(value):3478"
}

func makeStunBindingRequest() -> Data {
    var request = Data([0x00, 0x01, 0x00, 0x00]) + stunMagicCookie
    request.append(contentsOf: (0 ..< 12).map { _ in UInt8.random(in: .min ... .max) })
    return request
}

func xorMappedAddress(_ data: Data) -> String? {
    guard isStunSuccess(data) else { return nil }
    var offset = 20
    while offset + 4 <= data.count {
        let type = UInt16(data[offset]) << 8 | UInt16(data[offset + 1])
        let length = Int(UInt16(data[offset + 2]) << 8 | UInt16(data[offset + 3]))
        let value = offset + 4
        guard value + length <= data.count else { return nil }
        if type == 0x0020, length >= 8 {
            let port = (UInt16(data[value + 2]) << 8 | UInt16(data[value + 3])) ^ 0x2112
            switch data[value + 1] {
            case 0x01 where length >= 8:
                let magic = [UInt8](stunMagicCookie)
                let address = (0 ..< 4).map { String(data[value + 4 + $0] ^ magic[$0]) }.joined(separator: ".")
                return makeSocketAddress(address, port)
            case 0x02 where length >= 20:
                let mask = Array(data[4 ..< 20])
                let bytes = (0 ..< 16).map { data[value + 4 + $0] ^ mask[$0] }
                let address = stride(from: 0, to: 16, by: 2).map {
                    String(format: "%x", UInt16(bytes[$0]) << 8 | UInt16(bytes[$0 + 1]))
                }.joined(separator: ":")
                return makeSocketAddress(address, port)
            default:
                return nil
            }
        }
        offset = value + ((length + 3) & ~3)
    }
    return nil
}
