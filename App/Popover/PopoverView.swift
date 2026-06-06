import SwiftUI

struct PopoverView: View {
    @EnvironmentObject var state: AppState
    @State private var showTokenInput = false
    @State private var tokenInput = ""
    @State private var showSettings = false

    var body: some View {
        VStack(spacing: 0) {
            // Header with login status
            HStack(spacing: 6) {
                Text("D-Streamy")
                    .font(.headline)
                Spacer()
                if state.token.isEmpty {
                    Image(systemName: "circle.fill")
                        .font(.system(size: 8))
                        .foregroundStyle(.red)
                    Text("Not logged in")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                    Button("Login") { showTokenInput = true }
                        .font(.caption)
                        .buttonStyle(.bordered)
                        .controlSize(.small)
                } else {
                    if state.streamState == .connecting {
                        ProgressView()
                            .controlSize(.mini)
                        Text("Connecting")
                            .font(.caption)
                            .foregroundStyle(.orange)
                    } else if case .reconnecting = state.streamState {
                        ProgressView()
                            .controlSize(.mini)
                        Text("Reconnecting")
                            .font(.caption)
                            .foregroundStyle(.orange)
                    } else if state.streamState.isActive {
                        Image(systemName: "record.circle.fill")
                            .font(.system(size: 8))
                            .foregroundStyle(.green)
                        Text("Live")
                            .font(.caption.bold())
                            .foregroundStyle(.green)
                    }
                    Menu {
                        Button("Logout", role: .destructive) {
                            state.clearToken()
                            showTokenInput = false
                        }
                    } label: {
                        Text(state.username.isEmpty ? "Logged in" : state.username)
                            .font(.caption)
                            .foregroundStyle(.secondary)
                    }
                    .menuStyle(.borderlessButton)
                    .fixedSize()
                }
            }
            .padding(.horizontal, 16)
            .padding(.vertical, 12)

            if showTokenInput {
                VStack(alignment: .leading, spacing: 4) {
                    Text("Paste your Discord token:")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                    HStack(spacing: 4) {
                        SecureField("Token", text: $tokenInput)
                            .textFieldStyle(.roundedBorder)
                            .controlSize(.small)
                        Button("Save") {
                            if !tokenInput.isEmpty {
                                state.saveToken(tokenInput)
                                tokenInput = ""
                                showTokenInput = false
                            }
                        }
                        .buttonStyle(.borderedProminent)
                        .controlSize(.small)
                    }
                    Text("Discord → Network tab → Authorization header")
                        .font(.caption2)
                        .foregroundStyle(.tertiary)
                }
                .padding(.horizontal, 16)
                .padding(.bottom, 8)
            }

            Divider()

            VStack(spacing: 12) {
                if case .error(let msg) = state.streamState {
                    HStack {
                        Image(systemName: "exclamationmark.triangle.fill")
                            .foregroundStyle(.yellow)
                        Text(msg)
                            .font(.caption)
                            .foregroundStyle(.red)
                        Spacer()
                        Button("Dismiss") { state.streamState = .idle }
                            .font(.caption)
                            .buttonStyle(.plain)
                    }
                }

                if state.streamState.isActive {
                    WindowPickerView()
                    StatsView()
                } else if !state.token.isEmpty {
                    WindowPickerView()
                    GuildChannelView()

                    if showSettings {
                        Divider()
                        SettingsView()
                    }
                }
            }
            .padding(12)

            Divider()

            // Footer: Settings + Start/Stop
            HStack(spacing: 12) {
                Button {
                    withAnimation(.easeInOut(duration: 0.15)) { showSettings.toggle() }
                } label: {
                    Image(systemName: "gearshape")
                }
                .buttonStyle(.plain)
                .foregroundStyle(showSettings ? .primary : .secondary)

                Spacer()

                StreamButton()
            }
            .padding(.horizontal, 16)
            .padding(.vertical, 10)
        }
    }

}

struct StreamButton: View {
    @EnvironmentObject var state: AppState

    var body: some View {
        Button {
            Task {
                if state.streamState.canStop {
                    await state.stopStream()
                } else {
                    await state.startStream()
                }
            }
        } label: {
            if state.streamState == .connecting {
                Label("Cancel", systemImage: "stop.fill")
            } else {
                Label(
                    state.streamState.isActive ? "Stop" : "Start",
                    systemImage: state.streamState.isActive ? "stop.fill" : "play.fill"
                )
            }
        }
        .buttonStyle(.borderedProminent)
        .tint(state.streamState.canStop ? .red : .accentColor)
        .disabled(!state.streamState.canStop && !canStart)
        .help(missingInfo)
    }

    private var canStart: Bool {
        state.streamState.canStop ||
        (state.captureFilter != nil && state.selectedGuild != nil && state.selectedChannel != nil)
    }

    private var missingInfo: String {
        if state.streamState.canStop { return "Stop streaming" }
        var missing: [String] = []
        if state.captureFilter == nil { missing.append("source") }
        if state.selectedGuild == nil { missing.append("server") }
        if state.selectedChannel == nil { missing.append("channel") }
        if missing.isEmpty { return "Start streaming" }
        return "Select \(missing.joined(separator: ", ")) to start"
    }
}
