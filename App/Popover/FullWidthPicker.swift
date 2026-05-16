import SwiftUI
import AppKit

/// NSPopUpButton wrapper that expands to fill available width.
/// Native SwiftUI Picker (NSPopUpButton) has defaultHigh content hugging —
/// .frame(maxWidth: .infinity) cannot override AppKit layout priorities.
struct FullWidthPicker<Item: Hashable>: NSViewRepresentable {
    let items: [Item]
    let placeholder: String
    let title: (Item) -> String
    @Binding var selection: Item?
    var isEnabled: Bool = true

    func makeNSView(context: Context) -> NSPopUpButton {
        let button = NSPopUpButton(frame: .zero, pullsDown: false)
        button.setContentHuggingPriority(.defaultLow, for: .horizontal)
        button.setContentCompressionResistancePriority(.defaultLow, for: .horizontal)
        button.target = context.coordinator
        button.action = #selector(Coordinator.selectionChanged(_:))
        return button
    }

    func updateNSView(_ button: NSPopUpButton, context: Context) {
        // Keep coordinator in sync with latest struct values
        context.coordinator.parent = self

        button.isEnabled = isEnabled

        // Rebuild items
        button.removeAllItems()
        button.addItem(withTitle: placeholder)
        button.menu?.items.first?.representedObject = nil

        for (i, item) in items.enumerated() {
            button.addItem(withTitle: title(item))
            button.menu?.items[i + 1].representedObject = i
        }

        // Sync selection
        if let sel = selection, let idx = items.firstIndex(of: sel) {
            button.selectItem(at: idx + 1)
        } else {
            button.selectItem(at: 0)
        }
    }

    func makeCoordinator() -> Coordinator { Coordinator(parent: self) }

    class Coordinator: NSObject {
        var parent: FullWidthPicker
        init(parent: FullWidthPicker) { self.parent = parent }

        @objc func selectionChanged(_ sender: NSPopUpButton) {
            let idx = sender.indexOfSelectedItem
            let itemIdx = idx - 1  // offset for placeholder at 0
            if idx <= 0 || itemIdx >= parent.items.count {
                parent.selection = nil
            } else {
                parent.selection = parent.items[itemIdx]
            }
        }
    }
}
