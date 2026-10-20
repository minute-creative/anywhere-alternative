// USB HID key numbers (keyboard page 7), the language every Anywhere
// viewer speaks: the same number means the same key on a Mac and a PC.
// Also the map from typed characters to key + shift, for the on-screen
// keyboard and the phone's own keyboard.

import Foundation

enum HID {
    static let a: UInt16 = 0x04
    static let enter: UInt16 = 0x28
    static let escape: UInt16 = 0x29
    static let backspace: UInt16 = 0x2A
    static let tab: UInt16 = 0x2B
    static let space: UInt16 = 0x2C
    static let capsLock: UInt16 = 0x39
    static let f1: UInt16 = 0x3A
    static let delete: UInt16 = 0x4C
    static let right: UInt16 = 0x4F
    static let left: UInt16 = 0x50
    static let down: UInt16 = 0x51
    static let up: UInt16 = 0x52
    static let home: UInt16 = 0x4A
    static let end: UInt16 = 0x4D
    static let pageUp: UInt16 = 0x4B
    static let pageDown: UInt16 = 0x4E
    static let leftCtrl: UInt16 = 0xE0
    static let leftShift: UInt16 = 0xE1
    static let leftAlt: UInt16 = 0xE2 // Option on a Mac
    static let leftGui: UInt16 = 0xE3 // Command on a Mac, Windows key on a PC
    static let rightShift: UInt16 = 0xE5

    static let one: UInt16 = 0x1E
    static let two: UInt16 = 0x1F
    static let three: UInt16 = 0x20
    static let four: UInt16 = 0x21

    static func letter(_ c: Character) -> UInt16? {
        guard let v = c.lowercased().unicodeScalars.first?.value, v >= 0x61, v <= 0x7A else { return nil }
        return UInt16(v - 0x61) + 0x04
    }

    /// A typed character → (key, needs shift). Covers a US layout, which
    /// is what both hosts assume for injected keys.
    static func usage(for c: Character) -> (UInt16, Bool)? {
        if let l = letter(c) { return (l, c.isUppercase) }
        let plain: [Character: UInt16] = [
            "1": 0x1E, "2": 0x1F, "3": 0x20, "4": 0x21, "5": 0x22, "6": 0x23, "7": 0x24, "8": 0x25, "9": 0x26, "0": 0x27,
            "\n": enter, "\r": enter, "\t": tab, " ": space, "-": 0x2D, "=": 0x2E, "[": 0x2F, "]": 0x30, "\\": 0x31,
            ";": 0x33, "'": 0x34, "`": 0x35, ",": 0x36, ".": 0x37, "/": 0x38,
        ]
        if let u = plain[c] { return (u, false) }
        let shifted: [Character: UInt16] = [
            "!": 0x1E, "@": 0x1F, "#": 0x20, "$": 0x21, "%": 0x22, "^": 0x23, "&": 0x24, "*": 0x25, "(": 0x26, ")": 0x27,
            "_": 0x2D, "+": 0x2E, "{": 0x2F, "}": 0x30, "|": 0x31, ":": 0x33, "\"": 0x34, "~": 0x35, "<": 0x36,
            ">": 0x37, "?": 0x38,
        ]
        if let u = shifted[c] { return (u, true) }
        return nil
    }
}
