import XCTest
@testable import DStreamyApp

final class DaemonBinaryTests: XCTestCase {
    private var root: URL!

    override func setUpWithError() throws {
        root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
    }

    override func tearDownWithError() throws {
        try FileManager.default.removeItem(at: root)
    }

    private func makeExecutable(_ relative: String) throws -> URL {
        let url = root.appendingPathComponent(relative)
        try FileManager.default.createDirectory(at: url.deletingLastPathComponent(), withIntermediateDirectories: true)
        FileManager.default.createFile(atPath: url.path, contents: Data("#!/bin/sh\n".utf8), attributes: [.posixPermissions: 0o755])
        return url
    }

    func testPrefersTheBundledBinary() throws {
        let bundled = try makeExecutable("D-Streamy.app/Contents/MacOS/dstreamy")
        _ = try makeExecutable("repo/rust/target/debug/dstreamy")
        let found = DaemonBinary.locate(
            bundleURL: root.appendingPathComponent("D-Streamy.app"),
            environment: ["DSTREAMY_BIN": "/elsewhere/dstreamy"],
            searchFrom: [root.appendingPathComponent("repo/.build/debug")]
        )
        XCTAssertEqual(found?.standardizedFileURL, bundled.standardizedFileURL)
    }

    func testFallsBackToTheEnvironmentThenTheRepository() throws {
        let dev = try makeExecutable("repo/rust/target/debug/dstreamy")
        let bundle = root.appendingPathComponent("D-Streamy.app")
        let start = root.appendingPathComponent("repo/.build/debug")

        XCTAssertEqual(
            DaemonBinary.locate(bundleURL: bundle, environment: ["DSTREAMY_BIN": "/elsewhere/dstreamy"], searchFrom: [start])?.path,
            "/elsewhere/dstreamy"
        )
        XCTAssertEqual(
            DaemonBinary.locate(bundleURL: bundle, environment: [:], searchFrom: [start])?.standardizedFileURL,
            dev.standardizedFileURL
        )
    }
}
