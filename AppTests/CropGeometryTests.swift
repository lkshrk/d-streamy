import XCTest
@testable import DStreamyApp

final class CropGeometryTests: XCTestCase {
    func testFlipsToTopLeftAndScalesToSourcePixels() {
        let crop = sourcePixelCrop(
            viewRect: CGRect(x: 100, y: 50, width: 400, height: 300),
            viewSize: CGSize(width: 1000, height: 800),
            sourceWidth: 2000,
            sourceHeight: 1600
        )
        XCTAssertEqual(crop, CropRect(x: 200, y: 900, width: 800, height: 600))
    }

    func testClampsToTheSource() {
        let crop = sourcePixelCrop(
            viewRect: CGRect(x: -10, y: -10, width: 2000, height: 2000),
            viewSize: CGSize(width: 500, height: 400),
            sourceWidth: 1000,
            sourceHeight: 800
        )
        XCTAssertEqual(crop, CropRect(x: 0, y: 0, width: 1000, height: 800))
    }

    func testRejectsAnEmptyCrop() {
        XCTAssertNil(sourcePixelCrop(
            viewRect: CGRect(x: 600, y: 0, width: 10, height: 10),
            viewSize: CGSize(width: 500, height: 400),
            sourceWidth: 1000,
            sourceHeight: 800
        ))
    }
}
