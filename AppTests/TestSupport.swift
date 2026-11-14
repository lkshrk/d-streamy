import XCTest
@testable import DStreamyApp

enum Fixture {
    static let displayTarget = #"{"id":"display:1","kind":"display","title":"Display","width":640,"height":480}"#
    static let stoppedStats = #"{"video_frames_sent":10,"video_frames_dropped":0,"audio_frames_sent":50,"bytes_sent":1000,"resumes":0,"fps":30,"last_error":null}"#
}

func shellDaemon(_ script: String) -> DaemonClient {
    DaemonClient(executableURL: URL(fileURLWithPath: "/bin/sh"), arguments: ["-c", script])
}

@MainActor
func authenticatedState() -> AppState {
    let state = AppState(bootstrapOnLaunch: false)
    state.isAuthenticated = true
    return state
}

@MainActor
func readyToStart(_ state: AppState) {
    state.isAuthenticated = true
    state.selectedGuild = GuildInfo(id: "g", name: "Guild")
    state.selectedChannel = ChannelInfo(id: "c", name: "Channel")
    state.selectedTarget = CaptureTarget(id: "display:1", kind: .display, title: "Display", app: nil, width: 640, height: 480)
}

@MainActor
func waitFor(_ condition: () -> Bool, file: StaticString = #filePath, line: UInt = #line) async throws {
    let deadline = ContinuousClock.now + .seconds(5)
    while !condition() {
        guard ContinuousClock.now < deadline else {
            XCTFail("timed out waiting for app state", file: file, line: line)
            return
        }
        try await Task.sleep(for: .milliseconds(10))
    }
}
