import SwiftUI
import ScreenCaptureKit
import AppKit
import os

private let log = Logger(subsystem: "me.harke.d-streamy", category: "picker")

struct WindowPickerView: View {
    @EnvironmentObject var state: AppState

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            Label("Source", systemImage: state.captureKind == .display ? "display" : "macwindow")
                .font(.subheadline.bold())

            HStack(spacing: 6) {
                Button {
                    ContentPickerController.shared.showPicker(state: state)
                } label: {
                    HStack {
                        Text(state.captureLabel.isEmpty ? "Choose Source..." : state.captureLabel)
                            .lineLimit(1)
                            .truncationMode(.tail)
                        Spacer()
                        Image(systemName: state.captureKind == .display ? "display" : "rectangle.inset.filled.and.person.filled")
                            .foregroundStyle(.secondary)
                    }
                    .frame(maxWidth: .infinity)
                    .padding(.horizontal, 8)
                }
                .buttonStyle(.bordered)

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

/// Singleton that manages SCContentSharingPicker and bridges to AppState.
final class ContentPickerController: NSObject, SCContentSharingPickerObserver {
    static let shared = ContentPickerController()

    private weak var appState: AppState?
    private var isSetUp = false

    private override init() { super.init() }

    func showPicker(state: AppState) {
        self.appState = state

        let picker = SCContentSharingPicker.shared
        if !isSetUp {
            picker.add(self)
            var config = SCContentSharingPickerConfiguration()
            config.allowedPickerModes = [.singleWindow, .singleDisplay]
            config.allowsChangingSelectedContent = true
            picker.defaultConfiguration = config
            isSetUp = true
        }

        let menuWindow = NSApp.keyWindow
        menuWindow?.orderOut(nil)

        DispatchQueue.main.asyncAfter(deadline: .now() + 0.15) {
            picker.isActive = true
            picker.present()
        }
    }

    // MARK: - SCContentSharingPickerObserver

    func contentSharingPicker(
        _ picker: SCContentSharingPicker,
        didUpdateWith filter: SCContentFilter,
        for stream: SCStream?
    ) {
        Task { @MainActor in
            guard let state = self.appState else { return }

            // Hide crop overlay when window changes
            if state.isCropOverlayVisible {
                state.cropOverlayController.hide()
                state.isCropOverlayVisible = false
            }

            state.captureFilter = filter

            // Extract label from filter and persist window identity
            if #available(macOS 15.2, *),
               let window = filter.includedWindows.first {
                state.captureKind = .window
                let app = window.owningApplication?.applicationName ?? ""
                let title = window.title ?? ""
                state.captureLabel = app.isEmpty ? title : "\(app) — \(title)"

                // Save for auto-restore on next launch
                let bundleId = window.owningApplication?.bundleIdentifier ?? ""
                let frame = window.frame
                UserDefaults.standard.set(bundleId, forKey: "lastWindowBundleId")
                UserDefaults.standard.set(title, forKey: "lastWindowTitle")
                UserDefaults.standard.set(app, forKey: "lastWindowApp")
                UserDefaults.standard.set([frame.origin.x, frame.origin.y, frame.width, frame.height],
                                          forKey: "lastWindowFrame")
                UserDefaults.standard.set(CaptureKind.window.rawValue, forKey: "lastCaptureKind")
                log.info("saved window: \(bundleId) / \(title)")
            } else if #available(macOS 15.2, *),
                      let display = filter.includedDisplays.first {
                state.captureKind = .display
                state.captureLabel = displayCaptureLabel(for: display, filter: filter)
                state.cropRect = nil
                UserDefaults.standard.set(CaptureKind.display.rawValue, forKey: "lastCaptureKind")
                UserDefaults.standard.set(Int(display.displayID), forKey: "lastDisplayID")
                log.info("saved display: \(display.displayID)")
            } else {
                state.captureLabel = "Selected content"
                state.captureKind = .window
            }

            // Load saved crop for this window (or nil if none)
            state.loadCropForCurrentWindow()

            // If streaming, switch capture to new window live
            if state.streamState.isActive {
                Task { await state.switchCaptureSource() }
            }
        }
    }

    func contentSharingPicker(
        _ picker: SCContentSharingPicker,
        didCancelFor stream: SCStream?
    ) {
        // User dismissed without selecting
    }

    func contentSharingPickerStartDidFailWithError(_ error: Error) {
        log.error("SCContentSharingPicker failed: \(error.localizedDescription)")
    }
}
