import XCTest
import VideoToolbox
@testable import CaptureLib

final class VideoCodecTests: XCTestCase {
    func testRawValuesMatchIPCStrings() {
        XCTAssertEqual(VideoCodec.h264.rawValue, "H264")
        XCTAssertEqual(VideoCodec.h265.rawValue, "H265")
        XCTAssertEqual(VideoCodec(rawValue: "H265"), .h265)
        XCTAssertNil(VideoCodec(rawValue: "VP9"))
    }

    func testCodecTypes() {
        XCTAssertEqual(VideoCodec.h264.codecType, kCMVideoCodecType_H264)
        XCTAssertEqual(VideoCodec.h265.codecType, kCMVideoCodecType_HEVC)
    }

    func testParameterSetCount() {
        XCTAssertEqual(VideoCodec.h264.parameterSetCount, 2)  // SPS + PPS
        XCTAssertEqual(VideoCodec.h265.parameterSetCount, 3)  // VPS + SPS + PPS
    }
}
