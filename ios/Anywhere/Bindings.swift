// What each control on the pad does while watching the Mac: a table the
// person can edit (Settings → Controls), with two ready-made layouts.
// `InputMapper` reads the table 120 times a second.

import Foundation

/// A control on the pad.
enum PadControl: String, Codable, CaseIterable {
    case leftStick, rightStick
    case a, b, x, y
    case l1, r1, l2, r2, l3, r3
    case up, down, left, right
    case menu, options

    var label: String {
        switch self {
        case .leftStick: return "Left stick"
        case .rightStick: return "Right stick"
        case .a: return "A"
        case .b: return "B"
        case .x: return "X"
        case .y: return "Y"
        case .l1: return "L1"
        case .r1: return "R1"
        case .l2: return "L2"
        case .r2: return "R2"
        case .l3: return "L3 (press left stick)"
        case .r3: return "R3 (press right stick)"
        case .up: return "D-pad up"
        case .down: return "D-pad down"
        case .left: return "D-pad left"
        case .right: return "D-pad right"
        case .menu: return "Menu ≡"
        case .options: return "Options ⧉"
        }
    }

    var isStick: Bool { self == .leftStick || self == .rightStick }
}

/// What a control does.
enum Binding: Codable, Equatable, Hashable {
    case none
    /// Hold a key (USB HID usage) while the control is held.
    case key(UInt16)
    /// A mouse button: 0 left, 1 right, 2 middle.
    case mouse(UInt8)
    // Sticks only:
    case pointer
    case look
    case scroll
    case wasd
    case arrows
    // App actions (on press):
    case keyboard
    case launcher
    case quickMenu
    case speedToggle

    var label: String {
        switch self {
        case .none: return "Nothing"
        case .key(let u): return Binding.keyName(u)
        case .mouse(let b): return ["Click", "Right-click", "Middle click"][Int(min(b, 2))]
        case .pointer: return "Pointer"
        case .look: return "Mouse look"
        case .scroll: return "Scroll"
        case .wasd: return "W A S D"
        case .arrows: return "Arrow keys"
        case .keyboard: return "Keyboard"
        case .launcher: return "Launcher"
        case .quickMenu: return "Quick menu"
        case .speedToggle: return "Pointer speed"
        }
    }

    static func keyName(_ u: UInt16) -> String {
        switch u {
        case 0x04...0x1D: return String(UnicodeScalar(UInt8(u - 0x04) + 65)).uppercased()
        case 0x1E...0x26: return String(u - 0x1D)
        case 0x27: return "0"
        case HID.enter: return "Return"
        case HID.escape: return "Escape"
        case HID.backspace: return "Backspace"
        case HID.tab: return "Tab"
        case HID.space: return "Space"
        case HID.leftCtrl: return "Control"
        case HID.leftShift: return "Shift"
        case HID.leftAlt: return "Option / Alt"
        case HID.leftGui: return "Command ⌘ / Win"
        case HID.up: return "↑"
        case HID.down: return "↓"
        case HID.left: return "←"
        case HID.right: return "→"
        case HID.delete: return "Delete"
        case HID.home: return "Home"
        case HID.end: return "End"
        case HID.pageUp: return "Page Up"
        case HID.pageDown: return "Page Down"
        case 0x3A...0x45: return "F\(u - 0x39)"
        default: return String(format: "Key 0x%02X", u)
        }
    }

    /// Everything a button can be set to, in the order the editor cycles.
    static let buttonChoices: [Binding] = {
        var out: [Binding] = [.none, .mouse(0), .mouse(1), .mouse(2), .keyboard, .launcher, .quickMenu, .speedToggle]
        out += [HID.space, HID.enter, HID.escape, HID.tab, HID.backspace, HID.leftShift, HID.leftCtrl, HID.leftAlt, HID.leftGui].map(Binding.key)
        out += [HID.up, HID.down, HID.left, HID.right].map(Binding.key)
        out += (0x04...0x1D).map { Binding.key(UInt16($0)) } // A–Z
        out += (0x1E...0x27).map { Binding.key(UInt16($0)) } // 1–0
        out += (0x3A...0x45).map { Binding.key(UInt16($0)) } // F1–F12
        return out
    }()

    static let stickChoices: [Binding] = [.none, .pointer, .look, .scroll, .wasd, .arrows]
}

struct ControlMap: Codable, Equatable {
    var bindings: [PadControl: Binding]

    subscript(c: PadControl) -> Binding {
        get { bindings[c] ?? .none }
        set { bindings[c] = newValue }
    }

    /// Using the Mac: pointer and scroll on the sticks, clicks, modifiers.
    static let desktop = ControlMap(bindings: [
        .leftStick: .pointer, .rightStick: .scroll,
        .a: .mouse(0), .b: .mouse(1), .x: .keyboard, .y: .launcher,
        .l1: .key(HID.leftGui), .r1: .key(HID.leftShift), .l2: .mouse(1), .r2: .mouse(0),
        .l3: .speedToggle, .r3: .mouse(2),
        .up: .key(HID.up), .down: .key(HID.down), .left: .key(HID.left), .right: .key(HID.right),
        .menu: .key(HID.enter), .options: .key(HID.escape),
    ])

    /// Keyboard-and-mouse games: W A S D, mouse look, FPS buttons.
    static let game = ControlMap(bindings: [
        .leftStick: .wasd, .rightStick: .look,
        .a: .key(HID.space), .b: .key(HID.leftCtrl), .x: .key(HID.letter("r")!), .y: .key(HID.letter("e")!),
        .l1: .key(HID.leftShift), .r1: .key(HID.letter("q")!), .l2: .mouse(1), .r2: .mouse(0),
        .l3: .key(HID.leftShift), .r3: .key(HID.letter("f")!),
        .up: .key(HID.one), .down: .key(HID.three), .left: .key(HID.four), .right: .key(HID.two),
        .menu: .key(HID.escape), .options: .key(HID.tab),
    ])
}
