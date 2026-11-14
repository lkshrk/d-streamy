import SwiftUI

struct SettingsView: View {
    @EnvironmentObject var state: AppState

    private let resolutionPresets = [
        ("720p", 1280, 720),
        ("900p", 1600, 900),
        ("1080p", 1920, 1080),
    ]

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            Label("Settings", systemImage: "gearshape")
                .font(.subheadline.bold())

            row("Resolution") {
                Picker("", selection: resolutionBinding) {
                    ForEach(resolutionPresets, id: \.0) { preset in
                        Text(preset.0).tag(preset.0)
                    }
                }
                .labelsHidden()
                .controlSize(.small)
            }

            row("FPS") {
                Picker("", selection: $state.fps) {
                    Text("15").tag(15)
                    Text("30").tag(30)
                    Text("60").tag(60)
                }
                .labelsHidden()
                .controlSize(.small)
                .pickerStyle(.segmented)
            }

            row("Codec") {
                Picker("", selection: $state.videoCodec) {
                    Text("H.264").tag("H264")
                    Text("H.265").tag("H265")
                }
                .labelsHidden()
                .controlSize(.small)
                .pickerStyle(.segmented)
            }

            row("Bitrate") {
                Slider(value: $state.bitrateMbps, in: 1...15, step: 0.5)
                    .controlSize(.small)
                Text("\(state.bitrateMbps, specifier: "%.1f") Mbps")
                    .font(.caption.monospacedDigit())
                    .frame(width: 55, alignment: .trailing)
            }

            row("Volume") {
                Slider(value: $state.audioGain, in: 0...3, step: 0.1)
                    .controlSize(.small)
                Text("\(Int(state.audioGain * 100))%")
                    .font(.caption.monospacedDigit())
                    .frame(width: 55, alignment: .trailing)
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
    }

    private func row<Content: View>(_ label: String, @ViewBuilder _ content: () -> Content) -> some View {
        HStack {
            Text(label)
                .font(.caption)
                .frame(width: 70, alignment: .leading)
            content()
        }
    }

    private var resolutionBinding: Binding<String> {
        Binding(
            get: {
                resolutionPresets.first { $0.1 == state.maxWidth && $0.2 == state.maxHeight }?.0 ?? "720p"
            },
            set: { name in
                if let preset = resolutionPresets.first(where: { $0.0 == name }) {
                    state.maxWidth = preset.1
                    state.maxHeight = preset.2
                }
            }
        )
    }
}
