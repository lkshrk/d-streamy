import SwiftUI

struct WindowPickerView: View {
    @EnvironmentObject var state: AppState

    private var displays: [CaptureTarget] { state.targets.filter { $0.kind == .display } }
    private var windows: [CaptureTarget] { state.targets.filter { $0.kind == .window } }
    private var isDisplay: Bool { state.selectedTarget?.kind == .display }

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            Label("Source", systemImage: isDisplay ? "display" : "macwindow")
                .font(.subheadline.bold())

            HStack(spacing: 6) {
                Menu {
                    if !displays.isEmpty {
                        Section("Displays") {
                            ForEach(displays) { target in
                                Button(target.label) { state.selectTarget(target) }
                            }
                        }
                    }
                    if !windows.isEmpty {
                        Section("Windows") {
                            ForEach(windows) { target in
                                Button(target.label) { state.selectTarget(target) }
                            }
                        }
                    }
                    Divider()
                    Button("Refresh") { Task { await state.refreshTargets() } }
                } label: {
                    Text(state.selectedTarget?.label ?? "Choose Source...")
                        .lineLimit(1)
                        .truncationMode(.tail)
                        .frame(maxWidth: .infinity, alignment: .leading)
                }
                .menuStyle(.button)
                .disabled(state.streamState.canStop)
                .help(state.streamState.canStop ? "Stop the stream to change the source" : "Choose a window or display")

                Button {
                    state.toggleCropOverlay()
                } label: {
                    Image(systemName: state.isCropOverlayVisible ? "crop.rotate" : "crop")
                }
                .buttonStyle(.bordered)
                .disabled(!state.canCropSelectedContent)
                .help(state.canCropSelectedContent
                      ? (state.isCropOverlayVisible ? "Hide crop overlay" : "Set crop region")
                      : "Crop is available for window sharing")
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
    }
}
