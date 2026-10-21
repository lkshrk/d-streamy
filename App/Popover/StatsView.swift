import SwiftUI

struct StatsView: View {
    @EnvironmentObject var state: AppState

    var body: some View {
        let snapshot = state.health
        let health = snapshot?.health
        let metrics = snapshot?.metrics
        let videoLevel = health?.videoLevel ?? .good
        let audioLevel = health?.audioLevel ?? .good

        VStack(spacing: 12) {
            if let target = state.selectedTarget {
                HStack {
                    Image(systemName: "play.circle.fill")
                        .foregroundStyle(.green)
                    Text(target.label)
                        .font(.subheadline.bold())
                        .lineLimit(1)
                    Spacer()
                    Text(formatUptime(state.streamStartedAt))
                        .font(.system(.caption, design: .monospaced))
                        .foregroundStyle(.secondary)
                }
            }

            VStack(spacing: 6) {
                HealthRow(label: "Video", score: health?.videoScore ?? 100,
                          level: videoLevel, series: health?.fpsSeries ?? [])
                HealthRow(label: "Audio", score: health?.audioScore ?? 100,
                          level: audioLevel, series: health?.audioSeries ?? [])
            }

            LazyVGrid(columns: [
                GridItem(.flexible()),
                GridItem(.flexible()),
            ], spacing: 8) {
                StatCard(label: "FPS",
                         value: "\(metrics?.fps ?? 0)/\(state.fps)",
                         valueColor: videoLevel.color)
                StatCard(label: "Jitter p95/max",
                         value: "\(metrics?.gapP95Ms ?? 0)/\(metrics?.gapMaxMs ?? 0) ms",
                         valueColor: videoLevel.color)
                StatCard(label: "Bitrate", value: formatBitrate(metrics?.bitrateKbps ?? 0))
                StatCard(label: "Dropped", value: "\(snapshot?.stats.videoFramesDropped ?? 0)",
                         valueColor: (metrics?.vDrop ?? 0) > 1 ? .yellow : .primary)
                StatCard(label: "Audio/s", value: "\(metrics?.aSent ?? 0)",
                         valueColor: audioLevel.color)
                StatCard(label: "Enc Err", value: "\(metrics?.aEncErr ?? 0)",
                         valueColor: (metrics?.aEncErr ?? 0) > 0 ? .red : .primary)
            }
        }
    }

    private func formatUptime(_ start: Date?) -> String {
        let seconds = start.map { max(0, Int(Date().timeIntervalSince($0))) } ?? 0
        let h = seconds / 3600
        let m = (seconds % 3600) / 60
        let s = seconds % 60
        return String(format: "%02d:%02d:%02d", h, m, s)
    }

    private func formatBitrate(_ kbps: Int) -> String {
        kbps >= 1000
            ? String(format: "%.1f Mbps", Double(kbps) / 1000)
            : "\(kbps) kbps"
    }
}

struct StatCard: View {
    let label: String
    let value: String
    var valueColor: Color = .primary

    var body: some View {
        VStack(spacing: 2) {
            Text(value)
                .font(.system(.title3, design: .monospaced).bold())
                .foregroundStyle(valueColor)
                .lineLimit(1)
                .minimumScaleFactor(0.7)
            Text(label)
                .font(.caption2)
                .foregroundStyle(.secondary)
        }
        .frame(maxWidth: .infinity)
        .padding(.vertical, 8)
        .background(.quaternary.opacity(0.5), in: RoundedRectangle(cornerRadius: 8))
    }
}
