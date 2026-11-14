import XCTest
@testable import DStreamyApp

final class PendingStartTests: XCTestCase {
    @MainActor
    private func state(_ script: String, daemonFactory: (() -> DaemonClient)? = nil) throws -> (AppState, DaemonClient) {
        let client = shellDaemon(script)
        try client.launch()
        let state = AppState(bootstrapOnLaunch: false, daemon: client, daemonFactory: daemonFactory)
        readyToStart(state)
        return (state, client)
    }

    private static let targets = #"{"type":"result","id":"1","data":[\#(Fixture.displayTarget)]}"#

    private final class TerminalGate: @unchecked Sendable {
        private let lock = NSLock()
        private let deliver: @Sendable (SessionEvent) -> Void
        private var pending: [SessionEvent] = []
        private var holding = true

        init(_ deliver: @escaping @Sendable (SessionEvent) -> Void) { self.deliver = deliver }

        func receive(_ event: SessionEvent) {
            let held = lock.withLock {
                if holding, case .state(.stopped(_)) = event {
                    pending.append(event)
                    return true
                }
                return false
            }
            if !held { deliver(event) }
        }

        var count: Int { lock.withLock { pending.count } }

        func release() {
            let events = lock.withLock {
                holding = false
                defer { pending.removeAll() }
                return pending
            }
            for event in events { deliver(event) }
        }
    }

    @MainActor
    private func drainCallbacks() async {
        await withCheckedContinuation { (continuation: CheckedContinuation<Void, Never>) in
            DispatchQueue.main.async { continuation.resume() }
        }
    }

    @MainActor
    func testLocalStartDeadlineRetainsCancelAndRejectsDuplicateStart() async throws {
        let (state, client) = try state("""
        read targets
        printf '%s\\n' '\(Self.targets)'
        read start
        read next
        case "$next" in
          *'"cmd":"stop"'*) printf '%s\\n' '{"type":"error","id":"3","code":"busy","message":"start still pending"}' ;;
          *) printf '%s\\n' '{"type":"event","event":{"event":"error","fatal":true,"message":"duplicate Start reached daemon"}}' ;;
        esac
        while read rest; do :; done
        """)
        defer { client.terminate() }

        await state.startStream(timeout: .milliseconds(30))
        XCTAssertTrue(state.streamState.canStop)
        XCTAssertNotNil(state.daemonError)
        XCTAssertTrue(state.isAuthenticated)
        XCTAssertNil(state.authError)

        await state.startStream(timeout: .milliseconds(30))
        XCTAssertEqual(state.streamState, .connecting)
        await state.stopStream(timeout: .milliseconds(50))
        XCTAssertFalse(client.isRunning)
        XCTAssertEqual(state.streamState, .idle)
        XCTAssertTrue(state.isAuthenticated)
    }

    @MainActor
    func testCanceledStartIgnoresLateReplyAndEventsBeforeRetry() async throws {
        let (state, client) = try state("""
        read targets
        printf '%s\\n' '\(Self.targets)'
        read start
        printf '%s\\n' '{"type":"event","event":{"event":"e2ee","state":"encrypted","dave_protocol_version":1}}'
        read stop
        printf '%s\\n' '{"type":"result","id":"2","data":null}'
        printf '%s\\n' '{"type":"event","event":{"event":"state","state":{"state":"streaming"}}}'
        printf '%s\\n' '{"type":"event","event":{"event":"e2ee","state":"unencrypted","dave_protocol_version":0}}'
        printf '%s\\n' '{"type":"error","id":"3","code":"busy","message":"start pending"}'
        while read rest; do :; done
        """)
        defer { client.terminate() }

        let start = Task { await state.startStream(timeout: .seconds(2)) }
        try await waitFor { state.encryption.state == .encrypted }
        await state.stopStream(timeout: .milliseconds(100))
        await start.value
        XCTAssertFalse(client.isRunning)
        XCTAssertEqual(state.streamState, .idle)
        XCTAssertNil(state.streamStartedAt)
        XCTAssertEqual(state.encryption, EncryptionStatus())
        XCTAssertNil(state.health)
        XCTAssertTrue(state.isAuthenticated)
    }

    @MainActor
    func testLateStartCompletionAfterDeadlineRemainsStoppable() async throws {
        let (state, client) = try state("""
        read targets
        printf '%s\\n' '\(Self.targets)'
        read start
        read release
        printf '%s\\n' '{"type":"result","id":"2","data":null}'
        printf '%s\\n' '{"type":"event","event":{"event":"state","state":{"state":"streaming"}}}'
        printf '%s\\n' '{"type":"result","id":"3","data":null}'
        read stop
        printf '%s\\n' '{"type":"event","event":{"event":"state","state":{"state":"stopped","reason":{"reason":"requested"}}}}'
        printf '%s\\n' '{"type":"result","id":"4","data":\(Fixture.stoppedStats)}'
        while read rest; do :; done
        """)
        defer { client.terminate() }

        await state.startStream(timeout: .milliseconds(30))
        XCTAssertTrue(state.streamState.canStop)
        _ = try await client.send(.loggedIn, timeout: .seconds(1))
        try await waitFor { state.streamState == .streaming }
        XCTAssertTrue(state.streamState.canStop)
        XCTAssertTrue(state.isAuthenticated)
        await state.stopStream(timeout: .seconds(1))
        try await waitFor { state.streamState == .idle }
        XCTAssertTrue(client.isRunning, "streaming resolved the start, so a confirmed stop keeps the daemon")
    }

    @MainActor
    func testIdleDaemonCrashIsReportedWithoutClearingLogin() async throws {
        let (state, client) = try state("read line; exit 7")
        defer { client.terminate() }
        _ = try? await client.send(.loggedIn, timeout: .seconds(1))
        try await waitFor { state.daemonError != nil }
        XCTAssertFalse(state.streamState.canStop)
        XCTAssertTrue(state.isAuthenticated)
        XCTAssertNil(state.authError)
    }

    @MainActor
    func testDaemonCrashAfterStartDeadlineEndsUnresolvedSession() async throws {
        let (state, client) = try state("""
        read targets
        printf '%s\\n' '\(Self.targets)'
        read start
        read release
        exit 9
        """)
        defer { client.terminate() }
        await state.startStream(timeout: .milliseconds(30))
        XCTAssertTrue(state.streamState.canStop)
        _ = try? await client.send(.loggedIn, timeout: .seconds(1))
        try await waitFor { !state.streamState.canStop }
        XCTAssertNotNil(state.daemonError)
        XCTAssertTrue(state.isAuthenticated)
        XCTAssertNil(state.authError)
    }
    @MainActor
    func testStopFailureRetainsControlUntilConfirmedTermination() async throws {
        let (state, client) = try state("""
        read targets
        printf '%s\\n' '\(Self.targets)'
        read start
        printf '%s\\n' '{"type":"result","id":"2","data":null}'
        printf '%s\\n' '{"type":"event","event":{"event":"state","state":{"state":"streaming"}}}'
        read stop
        printf '%s\\n' '{"type":"error","id":"3","code":"busy","message":"stop unavailable"}'
        read stop
        printf '%s\\n' '{"type":"error","id":"4","code":"busy","message":"stop unavailable"}'
        while read rest; do :; done
        """)
        defer { client.terminate() }
        await state.startStream(timeout: .seconds(1))
        try await waitFor { state.streamState == .streaming }
        await state.stopStream(timeout: .milliseconds(100))
        XCTAssertTrue(state.streamState.canStop)
        XCTAssertTrue(client.isRunning)
        XCTAssertNotNil(state.daemonError)
        await state.startStream(timeout: .milliseconds(30))
        XCTAssertTrue(state.streamState.canStop)
        await state.stopStream(timeout: .milliseconds(100))
        XCTAssertFalse(client.isRunning)
        XCTAssertEqual(state.streamState, .idle)
        XCTAssertTrue(state.isAuthenticated)
        XCTAssertNil(state.authError)
    }

    @MainActor
    func testTerminalStateRejectsLateLiveEvents() {
        let state = authenticatedState()
        state.handle(.state(.connecting))
        state.handle(.state(.stopped(.requested)))
        state.handle(.state(.connecting))
        state.handle(.state(.streaming))
        state.handle(.state(.reconnecting(attempt: 1)))
        state.handle(.e2ee(state: .unencrypted, daveProtocolVersion: 0))
        XCTAssertEqual(state.streamState, .idle)
        XCTAssertEqual(state.encryption, EncryptionStatus())
        XCTAssertNil(state.streamStartedAt)
        XCTAssertTrue(state.isAuthenticated)
    }

    @MainActor
    private func assertUnconfirmedStopAcceptsLiveUpdates(_ stopReply: String) async throws {
        let initialHealth = #"{"type":"event","event":{"event":"health","metrics":{"fps":30,"gap_p95_ms":0,"gap_max_ms":0,"gap_trunc":false,"bitrate_kbps":1000,"v_drop":0,"a_enc":0,"a_sent":50,"a_drop_not_ready":0,"a_enc_err":0},"health":{"video_score":100,"audio_score":100,"video_level":"good","audio_level":"good","fps_series":[],"audio_series":[]},"stats":{"video_frames_sent":10,"video_frames_dropped":0,"source_audio_frames_dropped":0,"audio_frames_sent":50,"bytes_sent":1000,"resumes":0,"fps":30,"last_error":null}}}"#
        let updatedHealth = initialHealth
            .replacingOccurrences(of: "\"fps\":30", with: "\"fps\":17")
            .replacingOccurrences(of: "\"source_audio_frames_dropped\":0", with: "\"source_audio_frames_dropped\":4")
        let (state, client) = try state("""
        read targets
        printf '%s\\n' '\(Self.targets)'
        read start
        printf '%s\\n' '{"type":"result","id":"2","data":null}'
        printf '%s\\n' '{"type":"event","event":{"event":"state","state":{"state":"streaming"}}}'
        printf '%s\\n' '{"type":"event","event":{"event":"e2ee","state":"encrypted","dave_protocol_version":1}}'
        printf '%s\\n' '\(initialHealth)'
        read stop
        \(stopReply)
        read release
        printf '%s\\n' '{"type":"event","event":{"event":"e2ee","state":"unencrypted","dave_protocol_version":0}}'
        printf '%s\\n' '\(updatedHealth)'
        printf '%s\\n' '{"type":"result","id":"4","data":null}'
        while read rest; do :; done
        """)
        defer { client.terminate() }
        await state.startStream(timeout: .seconds(1))
        try await waitFor { state.encryption.state == .encrypted && state.health?.metrics.fps == 30 }
        await state.stopStream(timeout: .milliseconds(30))
        XCTAssertTrue(state.streamState.canStop)
        XCTAssertTrue(client.isRunning)
        XCTAssertNotNil(state.daemonError)
        _ = try await client.send(.loggedIn, timeout: .seconds(1))
        try await waitFor { state.encryption.state == .unencrypted && state.health?.metrics.fps == 17 }
        XCTAssertNotNil(state.encryption.warning)
        XCTAssertEqual(state.encryption.daveProtocolVersion, 0)
        XCTAssertEqual(state.health?.stats.sourceAudioFramesDropped, 4)
        XCTAssertTrue(state.streamState.canStop)
        XCTAssertTrue(state.isAuthenticated)
        XCTAssertNil(state.authError)
    }

    @MainActor
    func testFailedEstablishedStopKeepsEncryptionAndHealthUpdates() async throws {
        try await assertUnconfirmedStopAcceptsLiveUpdates(#"printf '%s\n' '{"type":"error","id":"3","code":"busy","message":"stop unavailable"}'"#)
    }

    @MainActor
    func testTimedOutEstablishedStopKeepsEncryptionAndHealthUpdates() async throws {
        try await assertUnconfirmedStopAcceptsLiveUpdates("")
    }

    @MainActor
    func testSuccessfulStopReplyKeepsControlUntilItsTerminalCallbackIsApplied() async throws {
        let (state, client) = try state("""
        read targets
        printf '%s\\n' '\(Self.targets)'
        read start
        printf '%s\\n' '{"type":"result","id":"2","data":null}'
        printf '%s\\n' '{"type":"event","event":{"event":"state","state":{"state":"streaming"}}}'
        read stop
        printf '%s\\n' '{"type":"result","id":"3","data":\(Fixture.stoppedStats)}'
        printf '%s\\n' '{"type":"event","event":{"event":"state","state":{"state":"stopped","reason":{"reason":"requested"}}}}'
        read targets
        printf '%s\\n' '\(Self.targets.replacingOccurrences(of: "\"id\":\"1\"", with: "\"id\":\"4\""))'
        read start
        printf '%s\\n' '{"type":"result","id":"5","data":null}'
        printf '%s\\n' '{"type":"event","event":{"event":"state","state":{"state":"streaming"}}}'
        while read rest; do :; done
        """)
        defer { client.terminate() }
        let deliver = try XCTUnwrap(client.onEvent)
        let gate = TerminalGate(deliver)
        client.onEvent = { gate.receive($0) }
        await state.startStream(timeout: .seconds(1))
        try await waitFor { state.streamState == .streaming }
        await state.stopStream(timeout: .seconds(1))
        try await waitFor { gate.count == 1 }
        XCTAssertTrue(state.streamState.canStop)
        XCTAssertTrue(client.isRunning)
        XCTAssertNotNil(state.daemonError)
        await state.startStream(timeout: .milliseconds(30))
        XCTAssertTrue(state.streamState.canStop)
        gate.release()
        await drainCallbacks()
        XCTAssertEqual(state.streamState, .idle)
        XCTAssertNil(state.daemonError)
        XCTAssertTrue(client.isRunning, "an observed healthy stop does not restart the daemon")
        await state.startStream(timeout: .seconds(1))
        try await waitFor { state.streamState == .streaming }
    }

    @MainActor
    func testRepeatedSuccessfulRepliesWithoutTerminalConfirmationCanTerminateForRecovery() async throws {
        let (state, client) = try state("""
        read targets
        printf '%s\\n' '\(Self.targets)'
        read start
        printf '%s\\n' '{"type":"result","id":"2","data":null}'
        printf '%s\\n' '{"type":"event","event":{"event":"state","state":{"state":"streaming"}}}'
        read stop
        printf '%s\\n' '{"type":"result","id":"3","data":\(Fixture.stoppedStats)}'
        read stop
        printf '%s\\n' '{"type":"result","id":"4","data":\(Fixture.stoppedStats)}'
        while read rest; do :; done
        """)
        defer { client.terminate() }
        await state.startStream(timeout: .seconds(1))
        try await waitFor { state.streamState == .streaming }
        await state.stopStream(timeout: .seconds(1))
        XCTAssertTrue(state.streamState.canStop)
        XCTAssertTrue(client.isRunning)
        await state.stopStream(timeout: .seconds(1))
        XCTAssertFalse(client.isRunning)
        XCTAssertEqual(state.streamState, .idle)
        XCTAssertTrue(state.isAuthenticated)
    }

    @MainActor
    func testStopReplyPreservesAnAlreadyAppliedTerminalError() async throws {
        let (state, client) = try state("""
        read targets
        printf '%s\\n' '\(Self.targets)'
        read start
        printf '%s\\n' '{"type":"result","id":"2","data":null}'
        printf '%s\\n' '{"type":"event","event":{"event":"state","state":{"state":"streaming"}}}'
        read stop
        printf '%s\\n' '{"type":"event","event":{"event":"state","state":{"state":"stopped","reason":{"reason":"e2ee_unavailable"}}}}'
        read release
        printf '%s\\n' '{"type":"result","id":"3","data":\(Fixture.stoppedStats)}'
        printf '%s\\n' '{"type":"result","id":"4","data":null}'
        while read rest; do :; done
        """)
        defer { client.terminate() }
        await state.startStream(timeout: .seconds(1))
        try await waitFor { state.streamState == .streaming }
        let stop = Task { await state.stopStream(timeout: .seconds(2)) }
        let terminal = AppState.StreamState.error(StopReason.e2eeUnavailable.message)
        try await waitFor { state.streamState == terminal }
        _ = try await client.send(.loggedIn, timeout: .seconds(1))
        await stop.value
        XCTAssertEqual(state.streamState, terminal)
        XCTAssertFalse(state.streamState.canStop)
        XCTAssertTrue(client.isRunning)
        XCTAssertTrue(state.isAuthenticated)
    }

    @MainActor
    func testFatalSessionErrorKeepsStopUntilActualTerminalAndThenAllowsRetry() async throws {
        let (state, client) = try state("""
        read targets
        printf '%s\\n' '\(Self.targets)'
        read start
        printf '%s\\n' '{"type":"result","id":"2","data":null}'
        printf '%s\\n' '{"type":"event","event":{"event":"state","state":{"state":"streaming"}}}'
        printf '%s\\n' '{"type":"event","event":{"event":"error","fatal":true,"message":"capture failed"}}'
        printf '%s\\n' '{"type":"event","event":{"event":"state","state":{"state":"stopped","reason":{"reason":"e2ee_unavailable"}}}}'
        read targets
        printf '%s\\n' '\(Self.targets.replacingOccurrences(of: "\"id\":\"1\"", with: "\"id\":\"3\""))'
        read start
        printf '%s\\n' '{"type":"result","id":"4","data":null}'
        printf '%s\\n' '{"type":"event","event":{"event":"state","state":{"state":"streaming"}}}'
        while read rest; do :; done
        """)
        defer { client.terminate() }
        let gate = TerminalGate(try XCTUnwrap(client.onEvent))
        client.onEvent = { gate.receive($0) }
        await state.startStream(timeout: .seconds(1))
        try await waitFor { state.daemonError?.contains("capture failed") == true && gate.count == 1 }
        XCTAssertTrue(state.streamState.canStop)
        XCTAssertTrue(state.isAuthenticated)
        XCTAssertNil(state.authError)
        await state.startStream(timeout: .milliseconds(30))
        gate.release()
        await drainCallbacks()
        XCTAssertEqual(state.streamState, .error(StopReason.e2eeUnavailable.message))
        XCTAssertFalse(state.streamState.canStop)
        XCTAssertNil(state.daemonError, "no Stop advice once the session has ended")
        await state.startStream(timeout: .seconds(1))
        try await waitFor { state.streamState == .streaming }
        XCTAssertTrue(client.isRunning)
    }

    @MainActor
    func testFatalAdmittedStartErrorReconcilesBeforeItsHeldTerminalCallback() async throws {
        let (state, client) = try state("""
        read targets
        printf '%s\\n' '\(Self.targets)'
        read start
        printf '%s\\n' '{"type":"event","event":{"event":"state","state":{"state":"connecting"}}}'
        printf '%s\\n' '{"type":"event","event":{"event":"error","fatal":true,"message":"capture failed during Start"}}'
        printf '%s\\n' '{"type":"event","event":{"event":"state","state":{"state":"stopped","reason":{"reason":"e2ee_unavailable"}}}}'
        read release
        printf '%s\\n' '{"type":"error","id":"2","code":"capture","message":"capture admission failed"}'
        printf '%s\\n' '{"type":"result","id":"3","data":null}'
        while read rest; do :; done
        """)
        defer { client.terminate() }
        let deliver = try XCTUnwrap(client.onEvent)
        let gate = TerminalGate(deliver)
        client.onEvent = { gate.receive($0) }
        let start = Task { await state.startStream(timeout: .seconds(2)) }
        try await waitFor { state.daemonError?.contains("capture failed during Start") == true && gate.count == 1 }
        XCTAssertTrue(state.streamState.canStop)
        XCTAssertTrue(client.isRunning)
        XCTAssertTrue(state.isAuthenticated)
        await state.startStream(timeout: .milliseconds(30))
        _ = try await client.send(.loggedIn, timeout: .seconds(1))
        await start.value
        XCTAssertFalse(client.isRunning)
        XCTAssertEqual(state.streamState, .idle)
        XCTAssertTrue(state.daemonError?.contains("capture admission failed") == true)
        gate.release()
        deliver(.state(.streaming))
        await drainCallbacks()
        XCTAssertEqual(state.streamState, .idle)
        XCTAssertNil(state.streamStartedAt)
        XCTAssertTrue(state.isAuthenticated)
        XCTAssertNil(state.authError)
    }

    @MainActor
    func testCurrentStartTerminalBeforeSuccessReplyRemainsTerminalWithoutRestart() async throws {
        let (state, client) = try state("""
        read targets
        printf '%s\\n' '\(Self.targets)'
        read start
        printf '%s\\n' '{"type":"event","event":{"event":"state","state":{"state":"connecting"}}}'
        printf '%s\\n' '{"type":"event","event":{"event":"state","state":{"state":"streaming"}}}'
        printf '%s\\n' '{"type":"event","event":{"event":"state","state":{"state":"stopped","reason":{"reason":"e2ee_unavailable"}}}}'
        read release
        printf '%s\\n' '{"type":"result","id":"2","data":null}'
        printf '%s\\n' '{"type":"result","id":"3","data":null}'
        while read rest; do :; done
        """)
        defer { client.terminate() }
        let start = Task { await state.startStream(timeout: .seconds(2)) }
        let terminal = AppState.StreamState.error(StopReason.e2eeUnavailable.message)
        try await waitFor { state.streamState == terminal }
        await state.startStream(timeout: .milliseconds(30))
        _ = try await client.send(.loggedIn, timeout: .seconds(1))
        await start.value
        XCTAssertEqual(state.streamState, terminal)
        XCTAssertNil(state.streamStartedAt)
        XCTAssertFalse(state.streamState.canStop)
        XCTAssertTrue(client.isRunning, "the current run's terminal event is authoritative")
        XCTAssertTrue(state.isAuthenticated)
    }

    @MainActor
    func testRejectedStartReconcilesEvenWhenAnOldTerminalCallbackAlreadyApplied() async throws {
        for code in ["capture", "internal"] {
            let next = shellDaemon("""
            read hello
            printf '%s\\n' '{"type":"result","id":"1","data":{"protocol":1,"version":"test","platform":"macos"}}'
            read targets
            printf '%s\\n' '\(Self.targets.replacingOccurrences(of: "\"id\":\"1\"", with: "\"id\":\"2\""))'
            read start
            printf '%s\\n' '{"type":"result","id":"3","data":null}'
            printf '%s\\n' '{"type":"event","event":{"event":"state","state":{"state":"streaming"}}}'
            printf '%s\\n' '{"type":"event","event":{"event":"e2ee","state":"encrypted","dave_protocol_version":1}}'
            while read rest; do :; done
            """)
            defer { next.terminate() }
            let (state, client) = try state("""
            read targets
            printf '%s\\n' '\(Self.targets)'
            read start
            printf '%s\\n' '{"type":"event","event":{"event":"state","state":{"state":"stopped","reason":{"reason":"requested"}}}}'
            read release
            printf '%s\\n' '{"type":"error","id":"2","code":"\(code)","message":"Start rejected while events were pending"}'
            printf '%s\\n' '{"type":"result","id":"3","data":null}'
            while read rest; do :; done
            """, daemonFactory: { next })
            defer { client.terminate() }
            let deliver = try XCTUnwrap(client.onEvent)
            let gate = TerminalGate(deliver)
            client.onEvent = { gate.receive($0) }
            let start = Task { await state.startStream(timeout: .seconds(2)) }
            try await waitFor { gate.count == 1 }
            gate.release()
            await drainCallbacks()
            XCTAssertEqual(state.streamState, .idle)
            await state.startStream(timeout: .milliseconds(30))
            _ = try await client.send(.loggedIn, timeout: .seconds(1))
            await start.value
            XCTAssertFalse(client.isRunning)
            XCTAssertEqual(state.streamState, .idle)
            XCTAssertTrue(state.daemonError?.contains("Start rejected") == true)
            XCTAssertTrue(state.isAuthenticated)
            XCTAssertNil(state.authError)
            await state.startStream(timeout: .seconds(1))
            try await waitFor { state.streamState == .streaming && state.encryption.state == .encrypted }
            deliver(.state(.streaming))
            deliver(.state(.stopped(.requested)))
            deliver(.e2ee(state: .unencrypted, daveProtocolVersion: 0))
            await drainCallbacks()
            XCTAssertEqual(state.streamState, .streaming)
            XCTAssertEqual(state.encryption.state, .encrypted)
            XCTAssertTrue(state.streamState.canStop)
        }
    }

    @MainActor
    func testRefusalAfterAnOldTerminalCallbackKeepsTheDaemonForTheRetry() async throws {
        for code in ["busy", "already_running", "not_logged_in", "invalid_token"] {
            let probe = code == "already_running"
            let stopProbe = probe ? """
            read stop
            printf '%s\\n' '{"type":"error","id":"4","code":"not_running","message":"no stream is running"}'
            """ : ""
            let next = probe ? 5 : 4
            let (state, client) = try state("""
            read targets
            printf '%s\\n' '\(Self.targets)'
            read start
            printf '%s\\n' '{"type":"event","event":{"event":"state","state":{"state":"stopped","reason":{"reason":"requested"}}}}'
            read release
            printf '%s\\n' '{"type":"error","id":"2","code":"\(code)","message":"Start rejected while events were pending"}'
            printf '%s\\n' '{"type":"result","id":"3","data":null}'
            \(stopProbe)
            read targets
            printf '%s\\n' '\(Self.targets.replacingOccurrences(of: "\"id\":\"1\"", with: "\"id\":\"\(next)\""))'
            read start
            printf '%s\\n' '{"type":"result","id":"\(next + 1)","data":null}'
            printf '%s\\n' '{"type":"event","event":{"event":"state","state":{"state":"streaming"}}}'
            while read rest; do :; done
            """, daemonFactory: {
                XCTFail("a refused Start relaunched the daemon")
                return DaemonClient(executableURL: URL(fileURLWithPath: "/usr/bin/false"))
            })
            defer { client.terminate() }
            let gate = TerminalGate(try XCTUnwrap(client.onEvent))
            client.onEvent = { gate.receive($0) }
            let start = Task { await state.startStream(timeout: .seconds(2)) }
            try await waitFor { gate.count == 1 }
            gate.release()
            await drainCallbacks()
            XCTAssertEqual(state.streamState, .idle)
            _ = try await client.send(.loggedIn, timeout: .seconds(1))
            await start.value
            XCTAssertTrue(client.isRunning)
            XCTAssertEqual(state.streamState, .idle)
            let authFailure = code == "not_logged_in" || code == "invalid_token"
            if authFailure {
                XCTAssertNil(state.daemonError, code)
            } else if probe {
                XCTAssertEqual(state.daemonError, "Start was rejected: Start rejected while events were pending. Stop the other stream on this Discord account (OBS or the dstreamy command) first.")
            } else {
                XCTAssertEqual(state.daemonError, "Start was rejected: Start rejected while events were pending.")
            }
            XCTAssertEqual(state.isAuthenticated, !authFailure)
            XCTAssertEqual(state.authError != nil, authFailure)
            if !authFailure {
                await state.startStream(timeout: .seconds(1))
                try await waitFor { state.streamState == .streaming }
            }
        }
    }

}
