import XCTest
@testable import DStreamyApp

/// Drives `DaemonClient` against a real `dstreamy daemon` and the mock; `DSTREAMY_INTEGRATION=1` makes missing binaries fail.
final class DaemonIntegrationTests: XCTestCase {
    private struct MockInfo: Decodable {
        let gatewayUrl: String
        let token: String
        let guildId: String
        let channelId: String
    }

    private final class Recorder<T>: @unchecked Sendable {
        private let lock = NSLock()
        private var items: [T] = []

        func append(_ item: T) { lock.withLock { items.append(item) } }
        var all: [T] { lock.withLock { items } }
    }

    private var mock: Process?
    private var mockInput: Pipe?
    private var configDir: URL?

    override func tearDown() {
        try? mockInput?.fileHandleForWriting.close()
        if mock?.isRunning == true { mock?.terminate() }
        if let configDir { try? FileManager.default.removeItem(at: configDir) }
        super.tearDown()
    }

    private func binary(_ env: String, _ name: String) throws -> URL {
        let environment = ProcessInfo.processInfo.environment
        let url = environment[env].map { URL(fileURLWithPath: $0) }
            ?? DaemonFixtureTests.repoRoot.appendingPathComponent("rust/target/debug/\(name)")
        guard FileManager.default.isExecutableFile(atPath: url.path) else {
            let message = "\(url.path) is missing; run `cargo build -p dstreamy-cli -p dstreamy-mock-discord` in rust/"
            if environment["DSTREAMY_INTEGRATION"] == "1" {
                XCTFail(message)
            }
            throw XCTSkip(message)
        }
        return url
    }

    private func startMock(_ url: URL) throws -> MockInfo {
        let proc = Process()
        proc.executableURL = url
        let input = Pipe()
        let output = Pipe()
        proc.standardInput = input
        proc.standardOutput = output
        try proc.run()
        mock = proc
        mockInput = input

        var line = Data()
        let handle = output.fileHandleForReading
        while true {
            let byte = try XCTUnwrap(handle.readData(ofLength: 1).first, "the mock exited before printing its URL")
            if byte == 0x0A { break }
            line.append(byte)
        }
        return try DaemonCoding.decoder.decode(MockInfo.self, from: line)
    }

    private func waitFor(_ what: String, timeout: Duration = .seconds(30), _ condition: () -> Bool) async throws {
        let deadline = ContinuousClock.now + timeout
        while !condition() {
            guard ContinuousClock.now < deadline else {
                XCTFail("timed out waiting for \(what)")
                return
            }
            try await Task.sleep(for: .milliseconds(50))
        }
    }

    func testHelloGuildsChannelsStartStop() async throws {
        let dstreamy = try binary("DSTREAMY_BIN", "dstreamy")
        let info = try startMock(try binary("DSTREAMY_MOCK_BIN", "mock-discord"))
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("dstreamy-app-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        configDir = dir

        let client = DaemonClient(
            executableURL: dstreamy,
            arguments: ["--config-dir", dir.path, "--gateway-url", info.gatewayUrl, "daemon", "--skip-platform"]
        )
        let events = Recorder<SessionEvent>()
        let exits = Recorder<Int32>()
        client.onEvent = { events.append($0) }
        client.onExit = { exits.append($0) }
        try client.launch()
        defer { client.terminate() }

        let hello = try await client.handshake()
        XCTAssertEqual(hello.protocol, 1)
        XCTAssertEqual(hello.platform, "macos")

        let before: LoggedInInfo = try await client.request(.loggedIn, timeout: .seconds(10))
        XCTAssertFalse(before.loggedIn)
        try await client.request(.setToken(info.token), as: Empty.self, timeout: .seconds(30))
        let after: LoggedInInfo = try await client.request(.loggedIn, timeout: .seconds(10))
        XCTAssertTrue(after.loggedIn)

        let guilds: [GuildInfo] = try await client.request(.guilds, timeout: .seconds(30))
        XCTAssertTrue(guilds.contains { $0.id == info.guildId }, "\(guilds)")
        let channels: [ChannelInfo] = try await client.request(.channels(guildId: info.guildId), timeout: .seconds(30))
        XCTAssertTrue(channels.contains { $0.id == info.channelId }, "\(channels)")
        let targets: [CaptureTarget] = try await client.request(.targets, timeout: .seconds(15))
        let target = try XCTUnwrap(targets.first { $0.id == "test-pattern" }, "\(targets)")

        try await client.request(.start(StartRequest(
            guildId: info.guildId,
            channelId: info.channelId,
            target: target.id,
            codec: "h264",
            fps: 15,
            resolution: "320x180",
            bitrateKbps: 1000
        )), as: Empty.self, timeout: .seconds(60))

        try await waitFor("streaming and a health event with frames sent") {
            let all = events.all
            return all.contains(.state(.streaming)) && all.contains {
                if case .health(let snapshot) = $0 { return snapshot.stats.videoFramesSent > 0 }
                return false
            }
        }

        try await client.request(.update(UpdateRequest(crop: CropRect(x: 0, y: 0, width: 640, height: 360))), as: Empty.self, timeout: .seconds(15))

        let stats: StreamStats = try await client.request(.stop, timeout: .seconds(30))
        XCTAssertGreaterThan(stats.videoFramesSent, 0)
        try await waitFor("the stopped state") { events.all.contains(.state(.stopped(.requested))) }

        try await client.request(.shutdown, as: Empty.self, timeout: .seconds(15))
        try await waitFor("the daemon to exit") { !exits.all.isEmpty }
        XCTAssertEqual(exits.all, [0])
    }
}
