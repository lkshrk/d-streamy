import XCTest
@testable import DStreamyApp

final class DaemonClientTests: XCTestCase {
    private func shell(_ script: String) -> DaemonClient {
        DaemonClient(executableURL: URL(fileURLWithPath: "/bin/sh"), arguments: ["-c", script])
    }

    func testEncodesCommandsAsSnakeCaseLines() throws {
        let request = DaemonRequest(id: "7", command: .start(StartRequest(
            guildId: "g", channelId: "c", target: "window:42",
            crop: CropRect(x: 1, y: 2, width: 3, height: 4), bitrateKbps: 6000
        )))
        let json = try XCTUnwrap(String(data: DaemonCoding.encoder.encode(request), encoding: .utf8))
        XCTAssertEqual(
            json,
            #"{"bitrate_kbps":6000,"channel_id":"c","cmd":"start","crop":{"height":4,"width":3,"x":1,"y":2},"guild_id":"g","id":"7","target":"window:42"}"#
        )
    }

    func testOutOfOrderRepliesAreMatchedById() async throws {
        let client = shell(#"""
        read a; read b
        printf '%s\n' '{"type":"error","id":"2","code":"not_logged_in","message":"no token"}'
        printf '%s\n' '{"type":"result","id":"1","data":{"logged_in":false}}'
        sleep 5
        """#)
        try client.launch()
        defer { client.terminate() }

        async let first = Result { try await client.request(.loggedIn, as: LoggedInInfo.self, timeout: .seconds(5)) }
        async let second = Result { try await client.request(.loggedIn, as: LoggedInInfo.self, timeout: .seconds(5)) }
        let results = await [first, second]

        XCTAssertEqual(results.compactMap { try? $0.get() }, [LoggedInInfo(loggedIn: false)])
        let errors = results.compactMap { result -> DaemonError? in
            if case .failure(let error) = result { return error as? DaemonError }
            return nil
        }
        XCTAssertEqual(errors, [DaemonError(code: "not_logged_in", message: "no token")])
    }

    func testPendingRequestsFailWhenTheDaemonExits() async throws {
        let client = shell("read line; exit 3")
        try client.launch()
        do {
            try await client.request(.hello, as: HelloInfo.self, timeout: .seconds(5))
            XCTFail("expected the request to fail")
        } catch let error as DaemonClientError {
            XCTAssertEqual(error, .exited(3))
        }
    }
}
