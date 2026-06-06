import SwiftUI
import AppKit
import CaptureLib

enum HealthLevel {
    case good, warn, bad

    var color: Color {
        switch self {
        case .good: return .green
        case .warn: return .yellow
        case .bad: return .red
        }
    }

    var nsColor: NSColor {
        switch self {
        case .good: return .systemGreen
        case .warn: return .systemYellow
        case .bad: return .systemRed
        }
    }

    init(_ core: StreamHealthLevel) {
        switch core {
        case .good: self = .good
        case .warn: self = .warn
        case .bad: self = .bad
        }
    }
}

/// Derived stream-stability summary computed from the rolling metrics history.
struct StreamHealth {
    let videoScore: Int
    let audioScore: Int
    let videoLevel: HealthLevel
    let audioLevel: HealthLevel
    let fpsSeries: [Double]
    let audioSeries: [Double]

    /// Worst of the two streams — drives the header live indicator.
    var overallLevel: HealthLevel {
        if videoLevel == .bad || audioLevel == .bad { return .bad }
        if videoLevel == .warn || audioLevel == .warn { return .warn }
        return .good
    }

    private static let sparkCount = 40
    private static let recentWindow = 10

    static func compute(history: [MetricsPayload], targetFps: Int) -> StreamHealth {
        guard !history.isEmpty else {
            return StreamHealth(
                videoScore: 100, audioScore: 100,
                videoLevel: .good, audioLevel: .good,
                fpsSeries: [], audioSeries: []
            )
        }

        func sample(_ m: MetricsPayload) -> StreamHealthSample {
            StreamHealthSample(
                fps: m.fps, gapP95Ms: m.gapP95Ms, vDrop: m.vDrop,
                aSent: m.aSent, aDropNotReady: m.aDropNotReady, aEncErr: m.aEncErr
            )
        }

        let videoVals = history.map { StreamHealthScoring.video(sample($0), targetFps: targetFps) }
        let audioVals = history.map { StreamHealthScoring.audio(sample($0)) }

        func avg(_ xs: ArraySlice<Double>) -> Double {
            xs.isEmpty ? 1.0 : xs.reduce(0, +) / Double(xs.count)
        }

        let videoSession = avg(videoVals[...])
        let audioSession = avg(audioVals[...])
        let videoRecent = avg(videoVals.suffix(recentWindow)[...])
        let audioRecent = avg(audioVals.suffix(recentWindow)[...])

        return StreamHealth(
            videoScore: Int((videoSession * 100).rounded()),
            audioScore: Int((audioSession * 100).rounded()),
            videoLevel: HealthLevel(StreamHealthLevel(score: videoRecent)),
            audioLevel: HealthLevel(StreamHealthLevel(score: audioRecent)),
            fpsSeries: history.suffix(sparkCount).map { Double($0.fps) },
            audioSeries: history.suffix(sparkCount).map { Double($0.aSent) }
        )
    }
}

struct Sparkline: View {
    let values: [Double]
    let tint: Color

    var body: some View {
        GeometryReader { geo in
            if values.count >= 2 {
                let maxV = values.max() ?? 1
                let minV = values.min() ?? 0
                let range = max(maxV - minV, 0.0001)
                Path { p in
                    for (i, v) in values.enumerated() {
                        let x = geo.size.width * Double(i) / Double(values.count - 1)
                        let y = geo.size.height * (1 - (v - minV) / range)
                        let pt = CGPoint(x: x, y: y)
                        if i == 0 { p.move(to: pt) } else { p.addLine(to: pt) }
                    }
                }
                .stroke(tint, style: StrokeStyle(lineWidth: 1.5, lineCap: .round, lineJoin: .round))
            }
        }
    }
}

struct HealthRow: View {
    let label: String
    let score: Int
    let level: HealthLevel
    let series: [Double]

    var body: some View {
        HStack(spacing: 8) {
            Circle()
                .fill(level.color)
                .frame(width: 8, height: 8)
            Text(label)
                .font(.caption.bold())
                .frame(width: 42, alignment: .leading)
            Text("\(score)%")
                .font(.system(.caption, design: .monospaced))
                .foregroundStyle(.secondary)
                .frame(width: 40, alignment: .trailing)
            Sparkline(values: series, tint: level.color)
                .frame(height: 16)
        }
    }
}
