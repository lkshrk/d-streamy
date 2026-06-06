import XCTest
@testable import CaptureLib

final class AnnexBTests: XCTestCase {
    private func convert(_ avcc: [UInt8]) -> [UInt8] {
        var out = Data()
        avcc.withUnsafeBufferPointer { AnnexB.appendNALUs(from: $0, into: &out) }
        return [UInt8](out)
    }

    func testSingleNALU() {
        // length=3, payload 0x41 0x42 0x43
        let avcc: [UInt8] = [0, 0, 0, 3, 0x41, 0x42, 0x43]
        XCTAssertEqual(convert(avcc), [0, 0, 0, 1, 0x41, 0x42, 0x43])
    }

    func testMultipleNALUs() {
        let avcc: [UInt8] = [0, 0, 0, 3, 0x41, 0x42, 0x43, 0, 0, 0, 2, 0x44, 0x45]
        XCTAssertEqual(convert(avcc), [0, 0, 0, 1, 0x41, 0x42, 0x43, 0, 0, 0, 1, 0x44, 0x45])
    }

    func testTruncatedLengthStops() {
        // declares length 5 but only 2 payload bytes follow → drop
        let avcc: [UInt8] = [0, 0, 0, 5, 0x41, 0x42]
        XCTAssertEqual(convert(avcc), [])
    }

    func testPartialSecondNALUKeepsFirst() {
        let avcc: [UInt8] = [0, 0, 0, 2, 0x41, 0x42, 0, 0, 0, 9, 0x44]
        XCTAssertEqual(convert(avcc), [0, 0, 0, 1, 0x41, 0x42])
    }

    func testEmpty() {
        XCTAssertEqual(convert([]), [])
    }

    func testZeroLengthNALUStops() {
        let avcc: [UInt8] = [0, 0, 0, 0, 0x41]
        XCTAssertEqual(convert(avcc), [])
    }
}
