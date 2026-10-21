import SwiftUI
import AppKit

@main
struct DStreamyApp: App {
    @NSApplicationDelegateAdaptor(AppDelegate.self) var appDelegate
    @StateObject private var appState: AppState

    init() {
        _appState = StateObject(wrappedValue: AppState())
    }

    var body: some Scene {
        MenuBarExtra {
            PopoverView()
                .environmentObject(appState)
                .frame(width: 300)
        } label: {
            MenuBarLabel(
                isActive: appState.streamState.isActive,
                dotColor: appState.healthLevel.nsColor,
                dotVisible: appState.menuDotVisible
            )
            .onAppear {
                appDelegate.appState = appState
            }
        }
        .menuBarExtraStyle(.window)
    }
}

struct MenuBarLabel: View {
    let isActive: Bool
    let dotColor: NSColor
    let dotVisible: Bool

    var body: some View {
        Image(nsImage: makeStatusIcon(active: isActive))
    }

    private func makeStatusIcon(active: Bool) -> NSImage {
        let size = NSSize(width: 18, height: 18)
        let image = NSImage(size: size, flipped: false) { rect in
            let symbolName = active ? "video.fill" : "video"
            if let symbol = NSImage(systemSymbolName: symbolName, accessibilityDescription: nil) {
                var config = NSImage.SymbolConfiguration(pointSize: 13, weight: .regular)
                if active {
                    // Tint white so it looks correct against dark menu bar when non-template
                    config = config.applying(.init(paletteColors: [.white]))
                }
                let configured = symbol.withSymbolConfiguration(config) ?? symbol
                let symbolSize = configured.size
                let symbolOrigin = NSPoint(
                    x: (rect.width - symbolSize.width) / 2 - 2,
                    y: (rect.height - symbolSize.height) / 2
                )
                configured.draw(in: NSRect(origin: symbolOrigin, size: symbolSize))
            }
            if active && dotVisible {
                let dotSize: CGFloat = 5
                let dotRect = NSRect(x: rect.width - dotSize - 1, y: rect.height - dotSize - 1, width: dotSize, height: dotSize)
                dotColor.setFill()
                NSBezierPath(ovalIn: dotRect).fill()
            }
            return true
        }
        image.isTemplate = !active
        return image
    }
}

class AppDelegate: NSObject, NSApplicationDelegate {
    private var eventMonitor: Any?
    weak var appState: AppState?
    private var isTerminating = false

    func applicationDidFinishLaunching(_ notification: Notification) {
        // Right-click on status bar item → context menu
        eventMonitor = NSEvent.addLocalMonitorForEvents(matching: .rightMouseDown) { event in
            guard let view = event.window?.contentView?.hitTest(event.locationInWindow),
                  String(describing: type(of: view)) == "NSStatusBarButton" else {
                return event
            }

            let menu = NSMenu()
            menu.addItem(withTitle: "Quit D-Streamy", action: #selector(NSApplication.terminate(_:)), keyEquivalent: "q")
            NSMenu.popUpContextMenu(menu, with: event, for: view)
            return nil
        }
    }

    func applicationShouldTerminate(_ sender: NSApplication) -> NSApplication.TerminateReply {
        guard !isTerminating else { return .terminateNow }
        guard let appState else { return .terminateNow }

        isTerminating = true
        Task { @MainActor in
            await appState.shutdownForQuit()
            sender.reply(toApplicationShouldTerminate: true)
        }

        return .terminateLater
    }
}
