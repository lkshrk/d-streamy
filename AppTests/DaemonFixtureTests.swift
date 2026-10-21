import XCTest
@testable import DStreamyApp

/// Decodes the golden protocol fixtures shared with `rust/crates/dstreamy-cli/tests/daemon.rs`.
final class DaemonFixtureTests: XCTestCase {
    static let repoRoot = URL(fileURLWithPath: #filePath).deletingLastPathComponent().deletingLastPathComponent()
    static let fixtureDir = repoRoot.appendingPathComponent("rust/crates/dstreamy-cli/tests/fixtures/daemon")

    private struct Scenario {
        let name: String
        let input: [Data]
        let output: [Data]
    }

    private func scenarios() throws -> [Scenario] {
        let files = try FileManager.default.contentsOfDirectory(atPath: Self.fixtureDir.path)
        let names = files.filter { $0.hasSuffix(".in.ndjson") }.map { String($0.dropLast(".in.ndjson".count)) }.sorted()
        XCTAssertFalse(names.isEmpty, "no fixtures in \(Self.fixtureDir.path)")
        return try names.map { name in
            Scenario(
                name: name,
                input: try lines(of: "\(name).in.ndjson"),
                output: try lines(of: "\(name).out.ndjson")
            )
        }
    }

    private func lines(of file: String) throws -> [Data] {
        let text = try String(contentsOf: Self.fixtureDir.appendingPathComponent(file), encoding: .utf8)
        return text.split(whereSeparator: \.isNewline).map { Data($0.utf8) }
    }

    private func plainJSON(_ data: Data) throws -> JSONValue {
        try JSONDecoder().decode(JSONValue.self, from: data)
    }

    func testRequestsDecodeAndReencodeIdentically() throws {
        for scenario in try scenarios() {
            let replies = try scenario.output.map { try DaemonCoding.decoder.decode(DaemonMessage.self, from: $0) }
            var replyCodes: [String: String] = [:]
            for case .error(let id?, let code, _) in replies { replyCodes[id] = code }

            for line in scenario.input {
                let text = String(decoding: line, as: UTF8.self)
                guard let request = try? DaemonCoding.decoder.decode(DaemonRequest.self, from: line) else {
                    let id = (try? plainJSON(line)).flatMap { value -> String? in
                        if case .object(let o) = value, case .string(let id)? = o["id"] { return id }
                        return nil
                    }
                    let code = id.flatMap { replyCodes[$0] } ?? "malformed"
                    XCTAssertTrue(
                        ["malformed", "unknown_cmd", "invalid_params"].contains(code),
                        "\(scenario.name): the app cannot decode \(text), but the daemon answered \(code)"
                    )
                    continue
                }
                let reencoded = try DaemonCoding.encoder.encode(request)
                XCTAssertEqual(try plainJSON(reencoded), try plainJSON(line), "\(scenario.name): \(text)")
            }
        }
    }

    func testRepliesAndEventsDecodeIntoTheAppModels() throws {
        var eventKinds = Set<String>()
        for scenario in try scenarios() {
            var commands: [String: DaemonCommand] = [:]
            for line in scenario.input {
                if let request = try? DaemonCoding.decoder.decode(DaemonRequest.self, from: line) {
                    commands[request.id] = request.command
                }
            }

            for line in scenario.output {
                let text = String(decoding: line, as: UTF8.self)
                let message = try DaemonCoding.decoder.decode(DaemonMessage.self, from: line)
                switch message {
                case .result(let id, let data):
                    let command = try XCTUnwrap(id.flatMap { commands[$0] }, "\(scenario.name): no request for \(text)")
                    XCTAssertNoThrow(try decodeReply(data, for: command), "\(scenario.name): \(text)")
                case .error(_, let code, let message):
                    XCTAssertFalse(code.isEmpty)
                    XCTAssertFalse(message.isEmpty)
                case .event(let event):
                    eventKinds.insert(kind(of: event))
                }
            }
        }
        XCTAssertTrue(eventKinds.isSuperset(of: ["state", "health"]), "events seen: \(eventKinds)")
    }

    func testSessionFixtureDrivesTheExpectedValues() throws {
        let output = try lines(of: "session.out.ndjson").map { try DaemonCoding.decoder.decode(DaemonMessage.self, from: $0) }
        var states: [SessionState] = []
        for case .event(.state(let state)) in output { states.append(state) }
        XCTAssertEqual(states, [.connecting, .streaming, .stopped(.requested)])

        func result(_ id: String) throws -> JSONValue {
            for case .result(id, let data) in output { return data }
            XCTFail("no result \(id)")
            return .null
        }
        let targets: [CaptureTarget] = try result("7").decode()
        XCTAssertEqual(targets.map(\.id), ["test-pattern"])
        XCTAssertEqual(targets.first?.kind, .display)
        XCTAssertEqual(try result("2").decode(as: LoggedInInfo.self), LoggedInInfo(loggedIn: false))
        XCTAssertEqual(try result("1").decode(as: HelloInfo.self).protocol, daemonProtocolVersion)
    }

    private func decodeReply(_ data: JSONValue, for command: DaemonCommand) throws {
        switch command {
        case .hello: _ = try data.decode(as: HelloInfo.self)
        case .loggedIn: _ = try data.decode(as: LoggedInInfo.self)
        case .guilds: _ = try data.decode(as: [GuildInfo].self)
        case .channels: _ = try data.decode(as: [ChannelInfo].self)
        case .targets: _ = try data.decode(as: [CaptureTarget].self)
        case .stop: _ = try data.decode(as: StreamStats.self)
        case .setToken, .start, .update, .shutdown: _ = try data.decode(as: Empty.self)
        }
    }

    private func kind(of event: SessionEvent) -> String {
        switch event {
        case .state: return "state"
        case .health: return "health"
        case .channelMoved: return "channel_moved"
        case .resumed: return "resumed"
        case .error: return "error"
        }
    }
}
