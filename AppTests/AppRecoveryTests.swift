import XCTest
@testable import DStreamyApp

final class AppRecoveryTests: XCTestCase {
    private static let replyId = #"id() { printf '%s' "$1" | sed 's/.*"id":"\([0-9]*\)".*/\1/'; }"#

    private static let stoppedEvent = #"{"type":"event","event":{"event":"state","state":{"state":"stopped","reason":{"reason":"requested"}}}}"#

    private static let identifyLimit = "Discord kept ending the session; D-Streamy stopped logging in again to protect the account. Try again later."

    @MainActor
    private func state(_ script: String) throws -> (AppState, DaemonClient) {
        let client = shellDaemon(Self.replyId + "\n" + script)
        try client.launch()
        let state = AppState(bootstrapOnLaunch: false, daemon: client, daemonFactory: {
            XCTFail("the daemon was relaunched")
            return DaemonClient(executableURL: URL(fileURLWithPath: "/usr/bin/false"))
        })
        readyToStart(state)
        return (state, client)
    }

    private static let targetsReply = #"printf '{"type":"result","id":"%s","data":[\#(Fixture.displayTarget)]}\n' "$(id "$line")""#

    @MainActor
    func testRefusedStartKeepsTheDaemon() async throws {
        for code in ["identify_limit", "invalid_params", "unknown_channel", "busy", "gateway", "not_logged_in", "invalid_token"] {
            let (state, client) = try state("""
            read line; \(Self.targetsReply)
            read line; printf '{"type":"error","id":"%s","code":"\(code)","message":"\(Self.identifyLimit)"}\\n' "$(id "$line")"
            read line; \(Self.targetsReply)
            read line; printf '{"type":"result","id":"%s","data":null}\\n' "$(id "$line")"
            printf '%s\\n' '{"type":"event","event":{"event":"state","state":{"state":"streaming"}}}'
            while read rest; do :; done
            """)
            defer { client.terminate() }

            await state.startStream(timeout: .seconds(2))
            XCTAssertTrue(client.isRunning, code)
            XCTAssertEqual(state.streamState, .idle, code)
            let authFailure = code == "not_logged_in" || code == "invalid_token"
            XCTAssertEqual(state.isAuthenticated, !authFailure, code)
            switch code {
            case "not_logged_in":
                XCTAssertNil(state.daemonError, "shown once, as the login error")
                XCTAssertEqual(state.authError, "\(Self.identifyLimit) After logging in from Terminal, open the D-Streamy menu again.")
            case "invalid_token":
                XCTAssertNil(state.daemonError, "shown once, as the login error")
                XCTAssertEqual(state.authError, Self.identifyLimit)
            default:
                XCTAssertEqual(state.daemonError, "Start was rejected: \(Self.identifyLimit)", code)
            }
            if !authFailure {
                await state.startStream(timeout: .seconds(2))
                try await waitFor { state.streamState == .streaming }
                XCTAssertNil(state.daemonError, code)
            }
        }
    }

    @MainActor
    func testAlreadyRunningElsewhereKeepsTheDaemonAndSaysWhatToStop() async throws {
        let (state, client) = try state("""
        read line; \(Self.targetsReply)
        read line; printf '{"type":"error","id":"%s","code":"already_running","message":"another dstreamy session is live (pid 42)"}\\n' "$(id "$line")"
        read line
        case "$line" in
          *'"cmd":"stop"'*) printf '{"type":"error","id":"%s","code":"not_running","message":"no stream is running"}\\n' "$(id "$line")" ;;
        esac
        while read rest; do :; done
        """)
        defer { client.terminate() }

        await state.startStream(timeout: .seconds(2))
        XCTAssertTrue(client.isRunning)
        XCTAssertEqual(state.streamState, .idle)
        XCTAssertEqual(
            state.daemonError,
            "Start was rejected: another dstreamy session is live (pid 42). Stop the other stream on this Discord account (OBS or the dstreamy command) first."
        )
    }

    @MainActor
    func testAlreadyRunningFromALeftoverOwnStreamStopsIt() async throws {
        let (state, client) = try state("""
        read line; \(Self.targetsReply)
        read line; printf '{"type":"error","id":"%s","code":"already_running","message":"a stream is already running"}\\n' "$(id "$line")"
        read line
        printf '%s\\n' '\(Self.stoppedEvent)'
        printf '{"type":"result","id":"%s","data":\(Fixture.stoppedStats)}\\n' "$(id "$line")"
        while read rest; do :; done
        """)
        defer { client.terminate() }

        await state.startStream(timeout: .seconds(2))
        XCTAssertTrue(client.isRunning)
        XCTAssertEqual(state.streamState, .idle)
        XCTAssertEqual(
            state.daemonError,
            "Start was rejected: a stream is already running. A stream left running in dstreamy was stopped. Press Start again."
        )
    }

    @MainActor
    func testAlreadyRunningWhoseOwnStreamDoesNotStopKeepsStopAndLiveUpdates() async throws {
        let (state, client) = try state("""
        read line; \(Self.targetsReply)
        read line; printf '{"type":"error","id":"%s","code":"already_running","message":"a stream is already running"}\\n' "$(id "$line")"
        read line; printf '{"type":"error","id":"%s","code":"busy","message":"stop unavailable"}\\n' "$(id "$line")"
        printf '%s\\n' '{"type":"event","event":{"event":"state","state":{"state":"streaming"}}}'
        read line
        printf '%s\\n' '\(Self.stoppedEvent)'
        printf '{"type":"result","id":"%s","data":\(Fixture.stoppedStats)}\\n' "$(id "$line")"
        while read rest; do :; done
        """)
        defer { client.terminate() }

        await state.startStream(timeout: .seconds(2))
        XCTAssertTrue(client.isRunning)
        XCTAssertTrue(state.streamState.canStop)
        XCTAssertTrue(state.daemonError?.contains("did not confirm stopping it") == true, state.daemonError ?? "")
        XCTAssertFalse(state.daemonError?.contains("other stream") == true)
        try await waitFor { state.streamState == .streaming }

        await state.stopStream(timeout: .seconds(2))
        try await waitFor { state.streamState == .idle }
        XCTAssertTrue(client.isRunning, "a confirmed stop keeps the daemon")
        XCTAssertNil(state.daemonError)
    }

    @MainActor
    func testAlreadyRunningWhoseStopFailsAfterItsStoppedEventEndsTheSession() async throws {
        let (state, client) = try state("""
        read line; \(Self.targetsReply)
        read line; printf '{"type":"error","id":"%s","code":"already_running","message":"a stream is already running"}\\n' "$(id "$line")"
        read line
        printf '%s\\n' '\(Self.stoppedEvent)'
        printf '{"type":"error","id":"%s","code":"busy","message":"stop events still being delivered"}\\n' "$(id "$line")"
        while read rest; do :; done
        """)
        defer { client.terminate() }

        await state.startStream(timeout: .seconds(2))
        XCTAssertTrue(client.isRunning)
        XCTAssertEqual(state.streamState, .idle)
        XCTAssertEqual(
            state.daemonError,
            "Start was rejected: a stream is already running. A stream left running in dstreamy was stopped. Press Start again."
        )
    }

    @MainActor
    func testNotLoggedInGuidanceNamesTheButtonAndTheBundledCLI() {
        XCTAssertEqual(
            AppState.notLoggedInGuidance("not logged in: use Log in, or run `dstreamy login`", cli: "/Apps/D-Streamy.app/Contents/MacOS/dstreamy"),
            "not logged in: use Edit token, or run \"/Apps/D-Streamy.app/Contents/MacOS/dstreamy\" login in Terminal. After logging in from Terminal, open the D-Streamy menu again."
        )
        XCTAssertEqual(
            AppState.notLoggedInGuidance("account \"work\" has no saved token: run `dstreamy login --account work`", cli: "/x/dstreamy"),
            "account \"work\" has no saved token: run \"/x/dstreamy\" login --account work in Terminal. After logging in from Terminal, open the D-Streamy menu again."
        )
    }

    @MainActor
    func testReopeningTheMenuPicksUpACLILogin() async throws {
        let (state, client) = try state("""
        read line; printf '{"type":"result","id":"%s","data":{"logged_in":true}}\\n' "$(id "$line")"
        read line; printf '{"type":"result","id":"%s","data":[{"id":"g","name":"Guild"}]}\\n' "$(id "$line")"
        read line; printf '{"type":"result","id":"%s","data":[{"id":"c","name":"Channel"}]}\\n' "$(id "$line")"
        while read rest; do :; done
        """)
        defer { client.terminate() }
        state.isAuthenticated = false

        await state.recheckLogin()
        XCTAssertTrue(state.isAuthenticated)
        XCTAssertEqual(state.guilds, [GuildInfo(id: "g", name: "Guild")])
        XCTAssertEqual(state.channels, [ChannelInfo(id: "c", name: "Channel")])
    }

    @MainActor
    func testStopDuringAPendingStartEndsQuietlyWithoutRestart() async throws {
        let (state, client) = try state("""
        read line; \(Self.targetsReply)
        read start
        printf '%s\\n' '{"type":"event","event":{"event":"error","fatal":false,"message":"gateway connect failed: dns error"}}'
        read stop
        printf '{"type":"error","id":"%s","code":"not_running","message":"no stream is running"}\\n' "$(id "$stop")"
        printf '{"type":"error","id":"%s","code":"cancelled","message":"Stop was pressed before the stream started"}\\n' "$(id "$start")"
        while read rest; do :; done
        """)
        defer { client.terminate() }

        let start = Task { await state.startStream(timeout: .seconds(2)) }
        try await waitFor { state.connectError == "gateway connect failed: dns error" }
        await state.stopStream(timeout: .seconds(2))
        await start.value
        XCTAssertTrue(client.isRunning, "a cancelled start is the user's own Stop, not a daemon fault")
        XCTAssertEqual(state.streamState, .idle)
        XCTAssertNil(state.daemonError)
        XCTAssertNil(state.connectError)
    }

    @MainActor
    func testShuttingDownRefusalTerminatesAndTheNextStartRelaunches() async throws {
        let next = shellDaemon(Self.replyId + "\n" + """
        read line; printf '{"type":"result","id":"%s","data":{"protocol":1,"version":"test","platform":"macos"}}\\n' "$(id "$line")"
        read line; \(Self.targetsReply)
        read line; printf '{"type":"result","id":"%s","data":null}\\n' "$(id "$line")"
        printf '%s\\n' '{"type":"event","event":{"event":"state","state":{"state":"streaming"}}}'
        while read rest; do :; done
        """)
        defer { next.terminate() }
        let client = shellDaemon(Self.replyId + "\n" + """
        read line; \(Self.targetsReply)
        read line; printf '{"type":"error","id":"%s","code":"shutting_down","message":"the service is shutting down"}\\n' "$(id "$line")"
        while read rest; do :; done
        """)
        try client.launch()
        defer { client.terminate() }
        var launches = 0
        let state = AppState(bootstrapOnLaunch: false, daemon: client, daemonFactory: {
            launches += 1
            return next
        })
        readyToStart(state)

        await state.startStream(timeout: .seconds(2))
        XCTAssertFalse(client.isRunning)
        XCTAssertEqual(state.streamState, .idle)
        XCTAssertEqual(state.daemonError, "Start was rejected: the service is shutting down. dstreamy was terminated before retrying.")

        await state.startStream(timeout: .seconds(2))
        try await waitFor { state.streamState == .streaming }
        XCTAssertEqual(launches, 1)
        XCTAssertTrue(next.isRunning)
    }

    @MainActor
    func testStopBeforeTheStartRequestIsSentEndsTheStart() async throws {
        let (state, client) = try state("""
        read targets
        printf '%s\\n' '{"type":"event","event":{"event":"error","fatal":false,"message":"slow"}}'
        read stop
        printf '{"type":"error","id":"%s","code":"not_running","message":"no stream is running"}\\n' "$(id "$stop")"
        printf '{"type":"result","id":"%s","data":[\(Fixture.displayTarget)]}\\n' "$(id "$targets")"
        while read line; do
          case "$line" in *'"cmd":"start"'*) printf '%s\\n' '{"type":"event","event":{"event":"error","fatal":true,"message":"a Start reached the daemon"}}' ;; esac
        done
        """)
        defer { client.terminate() }

        let start = Task { await state.startStream(timeout: .seconds(2)) }
        try await waitFor { state.connectError == "slow" }
        await state.stopStream(timeout: .seconds(2))
        await start.value
        XCTAssertEqual(state.streamState, .idle)
        XCTAssertTrue(client.isRunning)
        XCTAssertNil(state.daemonError)
    }

    @MainActor
    func testStartThatGoesLiveAfterANotRunningStopIsStopped() async throws {
        let (state, client) = try state("""
        read line; \(Self.targetsReply)
        read start
        printf '%s\\n' '{"type":"event","event":{"event":"error","fatal":false,"message":"connecting"}}'
        read stop
        printf '{"type":"error","id":"%s","code":"not_running","message":"no stream is running"}\\n' "$(id "$stop")"
        printf '{"type":"result","id":"%s","data":null}\\n' "$(id "$start")"
        read line
        case "$line" in
          *'"cmd":"stop"'*)
            printf '%s\\n' '\(Self.stoppedEvent)'
            printf '{"type":"result","id":"%s","data":\(Fixture.stoppedStats)}\\n' "$(id "$line")" ;;
        esac
        while read rest; do :; done
        """)
        defer { client.terminate() }

        let start = Task { await state.startStream(timeout: .seconds(2)) }
        try await waitFor { state.connectError == "connecting" }
        await state.stopStream(timeout: .seconds(2))
        await start.value
        XCTAssertEqual(state.streamState, .idle)
        XCTAssertTrue(client.isRunning, "the late session was stopped, so the daemon is kept")
        XCTAssertNil(state.daemonError)
    }

    @MainActor
    func testStartThatGoesLiveAfterANotRunningStopTerminatesWhenItCannotBeStopped() async throws {
        let (state, client) = try state("""
        read line; \(Self.targetsReply)
        read start
        printf '%s\\n' '{"type":"event","event":{"event":"error","fatal":false,"message":"connecting"}}'
        read stop
        printf '{"type":"error","id":"%s","code":"not_running","message":"no stream is running"}\\n' "$(id "$stop")"
        printf '{"type":"result","id":"%s","data":null}\\n' "$(id "$start")"
        read line; printf '{"type":"error","id":"%s","code":"busy","message":"stop unavailable"}\\n' "$(id "$line")"
        while read rest; do :; done
        """)
        defer { client.terminate() }

        let start = Task { await state.startStream(timeout: .seconds(2)) }
        try await waitFor { state.connectError == "connecting" }
        await state.stopStream(timeout: .seconds(2))
        await start.value
        XCTAssertFalse(client.isRunning)
        XCTAssertEqual(state.streamState, .idle)
    }

    @MainActor
    func testLateSessionThatEndsBeforeItsStopKeepsTheDaemonAndTheReason() async throws {
        let (state, client) = try state("""
        read line; \(Self.targetsReply)
        read start
        printf '%s\\n' '{"type":"event","event":{"event":"error","fatal":false,"message":"connecting"}}'
        read stop
        printf '{"type":"error","id":"%s","code":"not_running","message":"no stream is running"}\\n' "$(id "$stop")"
        printf '{"type":"result","id":"%s","data":null}\\n' "$(id "$start")"
        read line
        printf '%s\\n' '{"type":"event","event":{"event":"state","state":{"state":"stopped","reason":{"reason":"voice_taken_over"}}}}'
        printf '{"type":"error","id":"%s","code":"not_running","message":"no stream is running"}\\n' "$(id "$line")"
        while read rest; do :; done
        """)
        defer { client.terminate() }

        let start = Task { await state.startStream(timeout: .seconds(2)) }
        try await waitFor { state.connectError == "connecting" }
        await state.stopStream(timeout: .seconds(2))
        await start.value
        XCTAssertTrue(client.isRunning, "a session that already ended needs no daemon restart")
        XCTAssertEqual(state.streamState, .error(StopReason.voiceTakenOver.message))
    }

    @MainActor
    func testStartThatGoesLiveDuringASecondStopIsStillStopped() async throws {
        let (state, client) = try state("""
        read line; \(Self.targetsReply)
        read start
        printf '%s\\n' '{"type":"event","event":{"event":"error","fatal":false,"message":"connecting"}}'
        read stop
        printf '{"type":"error","id":"%s","code":"not_running","message":"no stream is running"}\\n' "$(id "$stop")"
        read stop
        printf '{"type":"result","id":"%s","data":null}\\n' "$(id "$start")"
        printf '{"type":"error","id":"%s","code":"not_running","message":"no stream is running"}\\n' "$(id "$stop")"
        read line
        printf '%s\\n' '{"type":"event","event":{"event":"channel_moved","guild_id":"g","channel_id":"moved"}}'
        printf '%s\\n' '\(Self.stoppedEvent)'
        printf '{"type":"result","id":"%s","data":\(Fixture.stoppedStats)}\\n' "$(id "$line")"
        while read rest; do :; done
        """)
        defer { client.terminate() }
        state.channels = [ChannelInfo(id: "moved", name: "Moved")]

        let start = Task { await state.startStream(timeout: .seconds(5)) }
        try await waitFor { state.connectError == "connecting" }
        await state.stopStream(timeout: .seconds(2))
        await state.stopStream(timeout: .seconds(2))
        await start.value
        XCTAssertEqual(state.selectedChannel?.id, "moved", "the late session got its own stop")
        XCTAssertEqual(state.streamState, .idle)
        XCTAssertTrue(client.isRunning)
    }

    @MainActor
    func testStartThatGoesLiveAfterItsStoppedEventIsStoppedByTheStopInFlight() async throws {
        let (state, client) = try state("""
        read line; \(Self.targetsReply)
        read start
        printf '%s\\n' '{"type":"event","event":{"event":"error","fatal":false,"message":"connecting"}}'
        read stop
        printf '%s\\n' '\(Self.stoppedEvent)'
        read gate
        printf '{"type":"result","id":"%s","data":null}\\n' "$(id "$start")"
        printf '{"type":"result","id":"%s","data":{"logged_in":true}}\\n' "$(id "$gate")"
        read gate
        printf '{"type":"error","id":"%s","code":"not_running","message":"no stream is running"}\\n' "$(id "$stop")"
        printf '{"type":"result","id":"%s","data":{"logged_in":true}}\\n' "$(id "$gate")"
        read line
        printf '%s\\n' '{"type":"event","event":{"event":"channel_moved","guild_id":"g","channel_id":"moved"}}'
        printf '{"type":"result","id":"%s","data":\(Fixture.stoppedStats)}\\n' "$(id "$line")"
        while read rest; do :; done
        """)
        defer { client.terminate() }
        state.channels = [ChannelInfo(id: "moved", name: "Moved")]

        let start = Task { await state.startStream(timeout: .seconds(10)) }
        try await waitFor { state.connectError == "connecting" }
        let stop = Task { await state.stopStream(timeout: .seconds(10)) }
        try await waitFor { state.streamState == .idle }
        try await client.request(.loggedIn, as: LoggedInInfo.self, timeout: .seconds(5))
        await start.value
        try await client.request(.loggedIn, as: LoggedInInfo.self, timeout: .seconds(5))
        await stop.value
        XCTAssertEqual(state.selectedChannel?.id, "moved", "the late session got its own stop")
        XCTAssertEqual(state.streamState, .idle)
        XCTAssertTrue(client.isRunning)
        XCTAssertNil(state.daemonError)
    }

    @MainActor
    func testNoLoginRecheckAfterARejectedToken() async throws {
        let (state, client) = try state("""
        read line; printf '{"type":"result","id":"%s","data":{"logged_in":true}}\\n' "$(id "$line")"
        while read rest; do :; done
        """)
        defer { client.terminate() }
        state.isAuthenticated = true
        state.handle(.state(.streaming))
        state.handle(.state(.stopped(.invalidToken)))
        XCTAssertFalse(state.isAuthenticated)

        await state.recheckLogin()
        XCTAssertFalse(state.isAuthenticated, "a rejected token still resolves, so logged_in must not be trusted")
        XCTAssertEqual(state.authError, StopReason.invalidToken.message)
    }

    @MainActor
    func testReconnectingShowsTheLastConnectErrorUntilStreaming() {
        let state = AppState(bootstrapOnLaunch: false)
        state.handle(.state(.streaming))
        state.handle(.error(message: "ignored while streaming", fatal: false))
        XCTAssertNil(state.connectError)
        state.handle(.state(.reconnecting(attempt: 1)))
        state.handle(.error(message: "voice connect timed out", fatal: false))
        XCTAssertEqual(state.connectError, "voice connect timed out")
        state.handle(.state(.streaming))
        XCTAssertNil(state.connectError)
    }

    @MainActor
    func testIdentifyLimitStopShowsItsMessageOnceWithoutStopAdvice() {
        let state = AppState(bootstrapOnLaunch: false)
        state.handle(.state(.streaming))
        state.handle(.error(message: Self.identifyLimit, fatal: true))
        XCTAssertEqual(state.daemonError, "\(Self.identifyLimit) Stop before starting another stream.")
        XCTAssertTrue(state.streamState.canStop)

        state.handle(.state(.stopped(.identifyLimit(retryAfterSecs: nil))))
        XCTAssertEqual(state.streamState, .error(StopReason.identifyLimit(retryAfterSecs: nil).message))
        XCTAssertNil(state.daemonError)
    }

    @MainActor
    func testUnconfirmedStartNoticeClearsOnceTheStartResolves() async throws {
        let (state, client) = try state("""
        read line; \(Self.targetsReply)
        read start
        read release
        printf '%s\\n' '{"type":"event","event":{"event":"state","state":{"state":"streaming"}}}'
        printf '{"type":"result","id":"%s","data":null}\\n' "$(id "$release")"
        read line
        printf '%s\\n' '\(Self.stoppedEvent)'
        printf '{"type":"result","id":"%s","data":\(Fixture.stoppedStats)}\\n' "$(id "$line")"
        while read rest; do :; done
        """)
        defer { client.terminate() }

        await state.startStream(timeout: .milliseconds(30))
        XCTAssertTrue(state.daemonError?.contains("Cancel before retrying") == true)
        _ = try await client.send(.loggedIn, timeout: .seconds(1))
        try await waitFor { state.streamState == .streaming }
        XCTAssertNil(state.daemonError)

        await state.stopStream(timeout: .seconds(2))
        try await waitFor { state.streamState == .idle }
        XCTAssertTrue(client.isRunning, "a start resolved by streaming stops without restarting the daemon")
    }

    @MainActor
    func testTokenOfAnotherUserExplainsHowToSwitch() async throws {
        let (state, client) = try state("""
        read line; printf '{"type":"error","id":"%s","code":"account_mismatch","message":"signed in as b, but account a belongs to a: log in as a, or save b as its own account (Add account… or `dstreamy login` without --account)"}\\n' "$(id "$line")"
        read line; \(Self.targetsReply)
        read line; printf '{"type":"result","id":"%s","data":null}\\n' "$(id "$line")"
        printf '%s\\n' '{"type":"event","event":{"event":"state","state":{"state":"streaming"}}}'
        while read rest; do :; done
        """)
        defer { client.terminate() }

        await state.saveToken("token-of-b")
        XCTAssertTrue(state.isAuthenticated)
        XCTAssertNil(state.daemonError)
        let guidance = try XCTUnwrap(state.authError)
        let cli = DaemonBinary.locate()?.path ?? "dstreamy"
        XCTAssertTrue(guidance.contains("\"\(cli)\" login --token"), guidance)
        XCTAssertTrue(guidance.contains("\"\(cli)\" account use NAME"), guidance)
        XCTAssertFalse(guidance.contains("Add account"), guidance)
        XCTAssertTrue(client.isRunning)

        await state.startStream(timeout: .seconds(2))
        XCTAssertNil(state.authError, "starting with the current login clears the guidance")
        try await waitFor { state.streamState == .streaming }
    }

    @MainActor
    func testLateChannelReplyForAPreviousServerIsDropped() async throws {
        let (state, client) = try state("""
        read first
        printf '%s\\n' '{"type":"event","event":{"event":"state","state":{"state":"connecting"}}}'
        read release
        printf '{"type":"result","id":"%s","data":[{"id":"a1","name":"Old server channel"}]}\\n' "$(id "$first")"
        printf '{"type":"result","id":"%s","data":{"logged_in":true}}\\n' "$(id "$release")"
        while read rest; do :; done
        """)
        defer { client.terminate() }
        state.selectedGuild = GuildInfo(id: "A", name: "Old")

        let fetch = Task { await state.fetchChannels(guildId: "A") }
        try await waitFor { state.streamState == .connecting }
        state.selectedGuild = GuildInfo(id: "B", name: "New")
        _ = try await client.send(.loggedIn, timeout: .seconds(1))
        await fetch.value
        XCTAssertEqual(state.channels, [])
    }

    @MainActor
    func testStaleErrorFromAnEarlierRequestForTheSameServerKeepsTheList() async throws {
        let (state, client) = try state("""
        read first
        printf '%s\\n' '{"type":"event","event":{"event":"state","state":{"state":"connecting"}}}'
        read second
        printf '{"type":"result","id":"%s","data":[{"id":"a2","name":"Fresh channel"}]}\\n' "$(id "$second")"
        read release
        printf '{"type":"error","id":"%s","code":"internal","message":"did not answer channels in time"}\\n' "$(id "$first")"
        printf '{"type":"result","id":"%s","data":{"logged_in":true}}\\n' "$(id "$release")"
        while read rest; do :; done
        """)
        defer { client.terminate() }
        state.selectedGuild = GuildInfo(id: "A", name: "Server")

        let first = Task { await state.fetchChannels(guildId: "A") }
        try await waitFor { state.streamState == .connecting }
        await state.fetchChannels(guildId: "A")
        XCTAssertEqual(state.channels, [ChannelInfo(id: "a2", name: "Fresh channel")])
        _ = try await client.send(.loggedIn, timeout: .seconds(1))
        await first.value
        XCTAssertEqual(state.channels, [ChannelInfo(id: "a2", name: "Fresh channel")])
        XCTAssertNil(state.daemonError)
    }

    @MainActor
    func testRestoredSourceNeedsTheSameWindowTitle() async throws {
        let defaults = UserDefaults.standard
        let keys = ["lastTargetId", "lastTargetApp", "lastTargetTitle"]
        let saved = keys.map { defaults.object(forKey: $0) }
        defer {
            for (key, value) in zip(keys, saved) {
                if let value { defaults.set(value, forKey: key) } else { defaults.removeObject(forKey: key) }
            }
        }
        defaults.set("window:1", forKey: "lastTargetId")
        defaults.set("Safari", forKey: "lastTargetApp")
        defaults.set("Slides", forKey: "lastTargetTitle")

        let (state, client) = try state("""
        read line; printf '{"type":"result","id":"%s","data":[{"id":"window:2","kind":"window","title":"Online banking","app":"Safari","width":800,"height":600}]}\\n' "$(id "$line")"
        read line; printf '{"type":"result","id":"%s","data":[{"id":"window:3","kind":"window","title":"Slides","app":"Safari","width":800,"height":600}]}\\n' "$(id "$line")"
        while read rest; do :; done
        """)
        defer { client.terminate() }
        state.selectedTarget = nil

        await state.refreshTargets()
        XCTAssertNil(state.selectedTarget)

        await state.refreshTargets()
        XCTAssertEqual(state.selectedTarget?.id, "window:3")
    }

    @MainActor
    func testTokenRejectedMidStreamAsksToLogInAgain() {
        let state = authenticatedState()
        state.handle(.state(.streaming))
        state.handle(.error(message: "Discord rejected the token", fatal: true))
        state.handle(.state(.stopped(.invalidToken)))
        XCTAssertEqual(state.streamState, .idle)
        XCTAssertFalse(state.isAuthenticated)
        XCTAssertEqual(state.authError, StopReason.invalidToken.message)
        XCTAssertNil(state.daemonError)
    }

    func testLoginRequestsOutlastTheDaemonsFirstLoginBudget() {
        XCTAssertGreaterThan(AppState.loginRequestTimeout, .seconds(45))
    }
}
