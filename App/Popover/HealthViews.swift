import SwiftUI
import AppKit

extension HealthLevel {
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
