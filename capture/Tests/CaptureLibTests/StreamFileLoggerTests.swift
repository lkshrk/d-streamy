import XCTest
@testable import CaptureLib

final class StreamFileLoggerTests: XCTestCase {
    func testWriteCreatesDirectoryAndAppendsSanitizedLine() throws {
        let directory = FileManager.default.temporaryDirectory
            .appendingPathComponent(UUID().uuidString, isDirectory: true)
        let fileURL = directory.appendingPathComponent("stream.log")
        let logger = StreamFileLogger(fileURL: fileURL)

        logger.write(component: "app", "first\nline")
        logger.write(component: "daemon", "second")

        let contents = try String(contentsOf: fileURL, encoding: .utf8)
        XCTAssertTrue(contents.contains("[app] first\\nline"))
        XCTAssertTrue(contents.contains("[daemon] second"))
        XCTAssertEqual(contents.split(separator: "\n").count, 2)
    }
}
