import SwiftUI
import AppKit

@main
struct DStreamyApp: App {
    @NSApplicationDelegateAdaptor(AppDelegate.self) var appDelegate
    @StateObject private var appState = AppState()

    var body: some Scene {
        MenuBarExtra {
            PopoverView()
                .environmentObject(appState)
                .frame(width: 300)
        } label: {
            MenuBarLabel(isActive: appState.streamState.isActive)
        }
        .menuBarExtraStyle(.window)
    }
}

struct MenuBarLabel: View {
    let isActive: Bool

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
            if active {
                let dotSize: CGFloat = 5
                let dotRect = NSRect(x: rect.width - dotSize - 1, y: rect.height - dotSize - 1, width: dotSize, height: dotSize)
                NSColor.systemGreen.setFill()
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
}
