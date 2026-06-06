import XCTest
@testable import CaptureLib

final class CaptureMetricsTests: XCTestCase {
    func testCountersAccumulate() {
        let m = CaptureMetrics()
        m.recordDelivered()
        m.recordDelivered()
        m.recordEncodeSubmit()
        m.recordEncodeDrop()
        m.recordAudioBuffer()
        m.recordAudioConvFail()

        let s = m.snapshot()
        XCTAssertEqual(s.delivered, 2)
        XCTAssertEqual(s.encSubmit, 1)
        XCTAssertEqual(s.encDrop, 1)
        XCTAssertEqual(s.audioBuf, 1)
        XCTAssertEqual(s.audioConvFail, 1)
    }

    func testSnapshotResets() {
        let m = CaptureMetrics()
        m.recordDelivered()
        _ = m.snapshot()
        let s = m.snapshot()
        XCTAssertEqual(s.delivered, 0)
    }
}
