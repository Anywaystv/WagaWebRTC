import Foundation
@testable import WagaWebRTC
import XCTest

final class IceProbeTests: XCTestCase {
    // RFC 5769 section 2.1, public test credentials and request bytes.
    private let password = "VOkJxbRl1RmTxUk/WvJxBt"
    private var request: Data {
        hex("000100582112a442b7e7a701bc34d686fa87dfae" +
            "802200105354554e207465737420636c69656e74" +
            "002400046e0001ff80290008932ff9b151263b36" +
            "000600096576746a3a68367659202020" +
            "000800149aeaa70cbfd8cb56781ef2b5b2d3f249c1b571a2" +
            "80280004e57a3bcf")
    }

    func testIntegrityMatchesPublishedVectorAndRejectsTampering() {
        XCTAssertTrue(validIceIntegrity(request, password: password))
        XCTAssertFalse(validIceIntegrity(request, password: "incorrect"))
        var changed = request
        changed[30] ^= 1
        XCTAssertFalse(validIceIntegrity(changed, password: password))
    }

    func testProbeUsesFreshAuthenticatedTransactionWithoutNomination() throws {
        var template = request
        template.insert(contentsOf: [0, 0x25, 0, 0], at: 20)
        template[3] += 4
        let first = try XCTUnwrap(makeIceProbe(template, password: password))
        let second = try XCTUnwrap(makeIceProbe(template, password: password))
        XCTAssertNotEqual(stunTransaction(first), stunTransaction(second))
        XCTAssertTrue(validIceIntegrity(first, password: password))
        XCTAssertEqual(first[20 ..< 24], Data([0x80, 0x22, 0, 0x10]))
        XCTAssertEqual(first.count, request.count - 8)
    }

    func testMalformedAttributesAreRejected() {
        XCTAssertNil(makeIceProbe(Data([0, 1]), password: password))
        var malformed = request
        malformed[22] = 0xff
        malformed[23] = 0xff
        XCTAssertNil(makeIceProbe(malformed, password: password))
        XCTAssertFalse(validIceIntegrity(malformed, password: password))
    }

    private func hex(_ value: String) -> Data {
        let bytes = Array(value)
        return Data(stride(from: 0, to: bytes.count, by: 2).map {
            UInt8(String(bytes[$0 ... $0 + 1]), radix: 16)!
        })
    }
}
