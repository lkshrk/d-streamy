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

    private let rect = CGRect(x: 100, y: 100, width: 400, height: 300)
    private let bounds = CGSize(width: 1000, height: 800)

    func testTheVisualTopStripMovesTheTopEdge() {
        let handle = CropHandle.onEdge(at: CGPoint(x: 300, y: 395), of: rect, width: 10)
        XCTAssertEqual(handle, .midXMaxY)
        XCTAssertEqual(handle?.resize(rect, dx: 0, dy: 20, minSize: 100, bounds: bounds), CGRect(x: 100, y: 100, width: 400, height: 320))
    }

    func testTheVisualBottomStripMovesTheBottomEdge() {
        let handle = CropHandle.onEdge(at: CGPoint(x: 300, y: 105), of: rect, width: 10)
        XCTAssertEqual(handle, .midXMinY)
        XCTAssertEqual(handle?.resize(rect, dx: 0, dy: -20, minSize: 100, bounds: bounds), CGRect(x: 100, y: 80, width: 400, height: 320))
    }

    func testCornerStripsMapToTheirCorners() {
        XCTAssertEqual(CropHandle.onEdge(at: CGPoint(x: 105, y: 395), of: rect, width: 10), .minXMaxY)
        XCTAssertEqual(CropHandle.onEdge(at: CGPoint(x: 495, y: 395), of: rect, width: 10), .maxXMaxY)
        XCTAssertEqual(CropHandle.onEdge(at: CGPoint(x: 105, y: 105), of: rect, width: 10), .minXMinY)
        XCTAssertEqual(CropHandle.onEdge(at: CGPoint(x: 495, y: 105), of: rect, width: 10), .maxXMinY)
        XCTAssertEqual(CropHandle.onEdge(at: CGPoint(x: 105, y: 250), of: rect, width: 10), .minXMidY)
        XCTAssertEqual(CropHandle.onEdge(at: CGPoint(x: 495, y: 250), of: rect, width: 10), .maxXMidY)
        XCTAssertNil(CropHandle.onEdge(at: CGPoint(x: 300, y: 250), of: rect, width: 10))
        XCTAssertNil(CropHandle.onEdge(at: CGPoint(x: 300, y: 405), of: rect, width: 10))
    }

    func testEveryHandleOwnsTheStripAtItsPointAndMovesThatPoint() {
        func step(_ value: CGFloat, _ mid: CGFloat) -> CGFloat { value < mid ? 1 : value > mid ? -1 : 0 }
        for handle in CropHandle.allCases {
            let point = handle.point(in: rect)
            let inside = CGPoint(x: point.x + step(point.x, rect.midX), y: point.y + step(point.y, rect.midY))
            XCTAssertEqual(CropHandle.onEdge(at: inside, of: rect, width: 10), handle, "\(handle)")

            let moved = handle.point(in: handle.resize(rect, dx: 10, dy: 10, minSize: 100, bounds: bounds))
            let expected = CGPoint(x: point.x + (point.x == rect.midX ? 0 : 10), y: point.y + (point.y == rect.midY ? 0 : 10))
            XCTAssertEqual(moved, expected, "\(handle)")
        }
    }
}
