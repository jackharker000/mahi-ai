import CoreGraphics
import XCTest
@testable import MahiKit

/// Pure-logic tests for the keyboard-shortcut surface: `KeyCombo(parsing:)` (the
/// engine's wire form) and `MahiComputerController`'s key-code/flag tables.
/// No CGEvents are created or posted here.
final class KeyComboTests: XCTestCase {
    // MARK: KeyCombo(parsing:)

    func testParsesCommandShiftP() {
        let combo = KeyCombo(parsing: "cmd+shift+p")
        XCTAssertEqual(combo.modifiers, [.command, .shift])
        XCTAssertEqual(combo.key, "p")
    }

    func testParsesBareNamedKey() {
        let combo = KeyCombo(parsing: "return")
        XCTAssertTrue(combo.modifiers.isEmpty)
        XCTAssertEqual(combo.key, "return")
    }

    func testParsesControlAltDelete() {
        let combo = KeyCombo(parsing: "ctrl+alt+delete")
        XCTAssertEqual(combo.modifiers, [.control, .option])
        XCTAssertEqual(combo.key, "delete")
    }

    func testParsingIsCaseInsensitive() {
        let combo = KeyCombo(parsing: "CMD+Shift+P")
        XCTAssertEqual(combo.modifiers, [.command, .shift])
        XCTAssertEqual(combo.key, "p")
        XCTAssertEqual(KeyCombo(parsing: "RETURN").key, "return")
    }

    func testParsingTrimsWhitespaceAndAcceptsAliases() {
        let combo = KeyCombo(parsing: " meta + opt + s ")
        XCTAssertEqual(combo.modifiers, [.command, .option])
        XCTAssertEqual(combo.key, "s")
    }

    func testMemberwiseInitLowercasesKey() {
        let combo = KeyCombo(modifiers: [.command], key: "K")
        XCTAssertEqual(combo.key, "k")
        XCTAssertEqual(combo.description, "⌘K")
    }

    // MARK: description rendering

    func testDescriptionRendersCommandShift() {
        XCTAssertEqual(KeyCombo(parsing: "cmd+shift+p").description, "⌘⇧P")
    }

    func testDescriptionUsesCanonicalModifierOrder() {
        // Modifier.allCases order (⌘ ⌥ ⌃ ⇧ fn) wins over the order typed.
        XCTAssertEqual(KeyCombo(parsing: "shift+cmd+s").description, "⌘⇧S")
        XCTAssertEqual(KeyCombo(parsing: "ctrl+alt+delete").description, "⌥⌃DELETE")
    }

    // MARK: MahiComputerController key-code table

    func testKeyCodeTableCoversCoreKeys() {
        XCTAssertEqual(MahiComputerController.keyCodes["a"], 0x00)
        XCTAssertEqual(MahiComputerController.keyCodes["return"], 0x24)
        XCTAssertEqual(MahiComputerController.keyCodes["enter"], 0x24)
        XCTAssertEqual(MahiComputerController.keyCodes["space"], 0x31)
        XCTAssertEqual(MahiComputerController.keyCodes["escape"], 0x35)
        XCTAssertEqual(MahiComputerController.keyCodes["delete"], 0x33)
        XCTAssertEqual(MahiComputerController.keyCodes["left"], 0x7B)
        XCTAssertEqual(MahiComputerController.keyCodes["up"], 0x7E)
        XCTAssertNil(MahiComputerController.keyCodes["hyperspace"])
    }

    func testKeyCodeTableCoversParsedComboKeys() {
        // Every key name the engine is documented to send resolves to a code.
        for raw in ["cmd+shift+p", "return", "ctrl+alt+delete", "cmd+space", "down", "cmd+,"] {
            let combo = KeyCombo(parsing: raw)
            XCTAssertNotNil(
                MahiComputerController.keyCodes[combo.key],
                "no key code for \(combo.key) (from \(raw))"
            )
        }
    }

    func testEventFlagsMapping() {
        let flags = MahiComputerController.eventFlags(for: [.command, .shift])
        XCTAssertTrue(flags.contains(.maskCommand))
        XCTAssertTrue(flags.contains(.maskShift))
        XCTAssertFalse(flags.contains(.maskControl))
        XCTAssertFalse(flags.contains(.maskAlternate))

        let fnFlags = MahiComputerController.eventFlags(for: [.function, .control, .option])
        XCTAssertTrue(fnFlags.contains(.maskSecondaryFn))
        XCTAssertTrue(fnFlags.contains(.maskControl))
        XCTAssertTrue(fnFlags.contains(.maskAlternate))
        XCTAssertFalse(fnFlags.contains(.maskCommand))
    }
}
