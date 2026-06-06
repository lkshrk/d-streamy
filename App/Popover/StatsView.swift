import SwiftUI

struct StatsView: View {
    @EnvironmentObject var state: AppState

    var body: some View {
        VStack(spacing: 12) {
            // Capture info
            if !state.captureLabel.isEmpty {
                HStack {
                    Image(systemName: "play.circle.fill")
                        .foregroundStyle(.green)
                    Text(state.captureLabel)
                        .font(.subheadline.bold())
                        .lineLimit(1)
                    Spacer()
                }
            }

            // Stats grid
            LazyVGrid(columns: [
                GridItem(.flexible()),
                GridItem(.flexible()),
            ], spacing: 8) {
                StatCard(label: "Uptime", value: formatUptime(state.stats.uptime))
                StatCard(label: "FPS", value: "\(state.stats.fps)")
                StatCard(label: "Bitrate", value: "\(state.stats.bitrate) kbps")
                StatCard(label: "Dropped", value: "\(state.stats.droppedFrames)")
                StatCard(label: "Audio Sent", value: "\(state.stats.audioFramesSent ?? 0)")
                StatCard(label: "Audio Enc", value: "\(state.stats.audioFramesEncoded ?? 0)")
            }
        }
    }

    private func formatUptime(_ seconds: Int) -> String {
        let h = seconds / 3600
        let m = (seconds % 3600) / 60
        let s = seconds % 60
        return String(format: "%02d:%02d:%02d", h, m, s)
    }
}

struct StatCard: View {
    let label: String
    let value: String

    var body: some View {
        VStack(spacing: 2) {
            Text(value)
                .font(.system(.title3, design: .monospaced).bold())
            Text(label)
                .font(.caption2)
                .foregroundStyle(.secondary)
        }
        .frame(maxWidth: .infinity)
        .padding(.vertical, 8)
        .background(.quaternary.opacity(0.5), in: RoundedRectangle(cornerRadius: 8))
    }
}
