import AppKit
import SwiftUI
import XCTest
@testable import DStreamyApp

private struct Room: Hashable {
    let id: Int
    let name: String
}

final class FullWidthPickerTests: XCTestCase {
    @MainActor
    func testDuplicateTitlesAndPlaceholderTitleKeepEveryItem() {
        let button = NSPopUpButton(frame: .zero, pullsDown: false)
        let titles = ["General", "Lounge", "General", "Select channel..."]
        fullWidthPickerRebuild(button, placeholder: "Select channel...", titles: titles, selected: 2)

        XCTAssertEqual(button.numberOfItems, titles.count + 1)
        XCTAssertNil(button.item(at: 0)?.representedObject)
        for index in titles.indices {
            XCTAssertEqual(button.item(at: index + 1)?.title, titles[index])
            XCTAssertEqual(button.item(at: index + 1)?.representedObject as? Int, index)
        }
        XCTAssertEqual(button.selectedItem?.representedObject as? Int, 2)

        fullWidthPickerRebuild(button, placeholder: "Select channel...", titles: ["A", "A"], selected: nil)
        XCTAssertEqual(button.numberOfItems, 3)
        XCTAssertEqual(button.indexOfSelectedItem, 0)
    }

    @MainActor
    func testChoosingADuplicateTitleSelectsThatElement() {
        final class Box { var selection: Room? }
        let box = Box()
        let items = [Room(id: 1, name: "General"), Room(id: 2, name: "Lounge"), Room(id: 3, name: "General")]
        let picker = FullWidthPicker(
            items: items,
            placeholder: "Select channel...",
            title: { $0.name },
            selection: Binding(get: { box.selection }, set: { box.selection = $0 })
        )
        let coordinator = picker.makeCoordinator()
        let button = NSPopUpButton(frame: .zero, pullsDown: false)
        button.target = coordinator
        button.action = #selector(FullWidthPicker<Room>.Coordinator.selectionChanged(_:))
        fullWidthPickerRebuild(button, placeholder: "Select channel...", titles: items.map(\.name), selected: nil)

        button.selectItem(at: 3)
        coordinator.selectionChanged(button)
        XCTAssertEqual(button.selectedItem?.representedObject as? Int, 2)
        XCTAssertEqual(box.selection, items[2])

        button.selectItem(at: 2)
        coordinator.selectionChanged(button)
        XCTAssertEqual(box.selection, items[1])

        button.selectItem(at: 0)
        coordinator.selectionChanged(button)
        XCTAssertNil(box.selection)
    }
}
