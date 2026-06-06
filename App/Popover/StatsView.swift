import SwiftUI

struct StatsView: View {
    @EnvironmentObject var state: AppState

    private var health: StreamHealth {
        StreamHealth.compute(history: state.metricsHistory, targetFps: state.fps)
    }
    private var latest: MetricsPayload? { state.metricsHistory.last }

    var body: some View {
        VStack(spacing: 12) {
            // Capture info + uptime
            if !state.captureLabel.isEmpty {
                HStack {
                    Image(systemName: "play.circle.fill")
                        .foregroundStyle(.green)
                    Text(state.captureLabel)
                        .font(.subheadline.bold())
                        .lineLimit(1)
                    Spacer()
                    Text(formatUptime(state.stats.uptime))
                        .font(.system(.caption, design: .monospaced))
                        .foregroundStyle(.secondary)
                }
            }

            // Stability headline: score + sparkline per A/V
            VStack(spacing: 6) {
                HealthRow(label: "Video", score: health.videoScore,
                          level: health.videoLevel, series: health.fpsSeries)
                HealthRow(label: "Audio", score: health.audioScore,
                          level: health.audioLevel, series: health.audioSeries)
            }

            // Live per-interval detail
            LazyVGrid(columns: [
                GridItem(.flexible()),
                GridItem(.flexible()),
            ], spacing: 8) {
                StatCard(label: "FPS",
                         value: "\(latest?.fps ?? state.stats.fps)/\(state.fps)",
                         valueColor: health.videoLevel.color)
                StatCard(label: "Jitter p95/max",
                         value: "\(latest?.gapP95Ms ?? 0)/\(latest?.gapMaxMs ?? 0) ms",
                         valueColor: health.videoLevel.color)
                StatCard(label: "Bitrate", value: formatBitrate(latest?.bitrateKbps ?? state.stats.bitrate))
                StatCard(label: "Dropped", value: "\(state.stats.droppedFrames)",
                         valueColor: (latest?.vDrop ?? 0) > 1 ? .yellow : .primary)
                StatCard(label: "Audio/s", value: "\(latest?.aSent ?? 0)",
                         valueColor: health.audioLevel.color)
                StatCard(label: "Enc Err", value: "\(latest?.aEncErr ?? 0)",
                         valueColor: (latest?.aEncErr ?? 0) > 0 ? .red : .primary)
            }
        }
    }

    private func formatUptime(_ seconds: Int) -> String {
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
