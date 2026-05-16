import SwiftUI
import ScreenCaptureKit
import os

private let log = Logger(subsystem: "me.harke.d-streamy", category: "picker")

struct WindowPickerView: View {
    @EnvironmentObject var state: AppState

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            Label("Window", systemImage: "macwindow")
                .font(.subheadline.bold())

            HStack {
                Button {
                    ContentPickerController.shared.showPicker(state: state)
                } label: {
                    HStack {
                        Text(state.captureLabel.isEmpty ? "Choose Window..." : state.captureLabel)
                            .lineLimit(1)
                            .truncationMode(.tail)
                        Spacer()
                        Image(systemName: "rectangle.inset.filled.and.person.filled")
                            .foregroundStyle(.secondary)
                    }
                    .frame(maxWidth: .infinity)
                    .padding(.horizontal, 8)
                    .padding(.vertical, 5)
                }
                .buttonStyle(.bordered)
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
            config.allowedPickerModes = [.singleWindow]
            picker.defaultConfiguration = config
            isSetUp = true
        }
        picker.isActive = true
        picker.present()
    }

    // MARK: - SCContentSharingPickerObserver

    func contentSharingPicker(
        _ picker: SCContentSharingPicker,
        didUpdateWith filter: SCContentFilter,
        for stream: SCStream?
    ) {
        Task { @MainActor in
            guard let state = self.appState else { return }
            state.captureFilter = filter

            // Extract label from filter and persist window identity
            if #available(macOS 15.2, *),
               let window = filter.includedWindows.first {
                let app = window.owningApplication?.applicationName ?? ""
                let title = window.title ?? ""
                state.captureLabel = app.isEmpty ? title : "\(app) — \(title)"

                // Save for auto-restore on next launch
                let bundleId = window.owningApplication?.bundleIdentifier ?? ""
                UserDefaults.standard.set(bundleId, forKey: "lastWindowBundleId")
                UserDefaults.standard.set(title, forKey: "lastWindowTitle")
                UserDefaults.standard.set(app, forKey: "lastWindowApp")
                log.info("saved window: \(bundleId) / \(title)")
            } else {
                state.captureLabel = "Selected content"
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
