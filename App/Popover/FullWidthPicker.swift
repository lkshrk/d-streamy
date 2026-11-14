import SwiftUI
import AppKit

/// NSPopUpButton that fills its width; SwiftUI's Picker hugs its content and ignores `.frame(maxWidth:)`.
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
        context.coordinator.parent = self

        button.isEnabled = isEnabled
        let selected = selection.flatMap { items.firstIndex(of: $0) }
        fullWidthPickerRebuild(button, placeholder: placeholder, titles: items.map(title), selected: selected)
    }

    func makeCoordinator() -> Coordinator { Coordinator(parent: self) }

    class Coordinator: NSObject {
        var parent: FullWidthPicker
        init(parent: FullWidthPicker) { self.parent = parent }

        @objc func selectionChanged(_ sender: NSPopUpButton) {
            if let index = sender.selectedItem?.representedObject as? Int, parent.items.indices.contains(index) {
                parent.selection = parent.items[index]
            } else {
                parent.selection = nil
            }
        }
    }
}

/// Adds items through `NSMenu.addItem`: `NSPopUpButton.addItem(withTitle:)` drops an existing item with the same title.
@MainActor
func fullWidthPickerRebuild(_ button: NSPopUpButton, placeholder: String, titles: [String], selected: Int?) {
    let menu = button.menu ?? NSMenu()
    menu.removeAllItems()
    menu.addItem(NSMenuItem(title: placeholder, action: nil, keyEquivalent: ""))
    for (index, title) in titles.enumerated() {
        let item = NSMenuItem(title: title, action: nil, keyEquivalent: "")
        item.representedObject = index
        menu.addItem(item)
    }
    if button.menu !== menu { button.menu = menu }
    if let selected, titles.indices.contains(selected) {
        button.selectItem(at: selected + 1)
    } else {
        button.selectItem(at: 0)
    }
}
