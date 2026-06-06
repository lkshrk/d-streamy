@testable import CaptureLib
import XCTest

final class CaptureDimensionsTests: XCTestCase {
    func testUncroppedWindowCapturesScaledOutputSize() {
        let dimensions = CaptureDimensions.resolve(
            sourceWidth: 2560,
            sourceHeight: 1440,
            maxWidth: 1280,
            maxHeight: 720,
            crop: nil
        )

        XCTAssertEqual(dimensions.streamWidth, 1280)
        XCTAssertEqual(dimensions.streamHeight, 720)
        XCTAssertEqual(dimensions.outputWidth, 1280)
        XCTAssertEqual(dimensions.outputHeight, 720)
    }

    func testCropUsesFullSourceCaptureAndCropSizedOutput() {
        let crop = CropFilter(x: 10, y: 20, width: 800, height: 600)

        let dimensions = CaptureDimensions.resolve(
            sourceWidth: 2560,
            sourceHeight: 1440,
            maxWidth: 1280,
            maxHeight: 720,
            crop: crop
        )

        XCTAssertEqual(dimensions.streamWidth, 2560)
        XCTAssertEqual(dimensions.streamHeight, 1440)
        XCTAssertEqual(dimensions.outputWidth, 800)
        XCTAssertEqual(dimensions.outputHeight, 600)
    }

    func testMissingSourceFallsBackToConfiguredMaximums() {
        let dimensions = CaptureDimensions.resolve(
            sourceWidth: 0,
            sourceHeight: 0,
            maxWidth: 1280,
            maxHeight: 720,
            crop: nil
        )

        XCTAssertEqual(dimensions.streamWidth, 1280)
        XCTAssertEqual(dimensions.streamHeight, 720)
        XCTAssertEqual(dimensions.outputWidth, 1280)
        XCTAssertEqual(dimensions.outputHeight, 720)
    }
}
