// What each control on the pad does while watching the Mac: a table the
// person can edit (Settings → Controls), with two ready-made layouts.
// `InputMapper` reads the table 120 times a second.

import Foundation

/// A control on the pad.
enum PadControl: String, Codable, CaseIterable {
    case leftStick, rightStick
    case a, b, x, y
    case l1, r1, l2, r2, l3, r3
    case l4, r4
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
        case .l4: return "L4 (extra button)"
        case .r4: return "R4 (extra button)"
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
enum PadAction: Codable, Equatable, Hashable {
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
    /// Cycle the picture zoom 1× → 2× → 3× → 1×.
    case zoom
    /// Back (⌘[ on the Mac): browsers, Finder, most apps with history.
    case back
    /// Magnify around the pointer while held; back to normal on release.
    case magnify
    /// Shift held with a mouse button (0 left, 1 right): Elden Ring's
    /// strong attack and skill.
    case shiftClick(UInt8)

    var label: String {
        switch self {
        case .none: return "Nothing"
        case .key(let u): return PadAction.keyName(u)
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
        case .zoom: return "Zoom"
        case .back: return "Back"
        case .shiftClick(let b): return b == 0 ? "Shift + Click" : "Shift + Right-click"
        case .magnify: return "Magnify (hold)"
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
    static let buttonChoices: [PadAction] = {
        var out: [PadAction] = [.none, .mouse(0), .mouse(1), .mouse(2), .back, .keyboard, .launcher, .quickMenu, .speedToggle, .zoom, .magnify]
        out += [HID.space, HID.enter, HID.escape, HID.tab, HID.backspace, HID.leftShift, HID.leftCtrl, HID.leftAlt, HID.leftGui].map(PadAction.key)
        out += [HID.up, HID.down, HID.left, HID.right].map(PadAction.key)
        out += (0x04...0x1D).map { PadAction.key(UInt16($0)) } // A–Z
        out += (0x1E...0x27).map { PadAction.key(UInt16($0)) } // 1–0
        out += (0x3A...0x45).map { PadAction.key(UInt16($0)) } // F1–F12
        return out
    }()

    static let stickChoices: [PadAction] = [.none, .pointer, .look, .scroll, .wasd, .arrows]
}

struct ControlMap: Codable, Equatable {
    var bindings: [PadControl: PadAction]

    subscript(c: PadControl) -> PadAction {
        get { bindings[c] ?? .none }
        set { bindings[c] = newValue }
    }

    /// Using the Mac: pointer and scroll on the sticks, clicks, modifiers.
    /// 2026-10-20 (owner): B is Back and Y right-clicks, like a phone
    /// browser; the launcher moved to R4 (also in the quick menu, since
    /// R4 only reaches apps once the Razer Nexus app gives it a job) and
    /// L2 magnifies while held, for small Mac buttons.
    static let desktop = ControlMap(bindings: [
        .leftStick: .pointer, .rightStick: .scroll,
        .a: .mouse(0), .b: .back, .x: .keyboard, .y: .mouse(1),
        .l1: .key(HID.leftGui), .r1: .key(HID.leftShift), .l2: .magnify, .r2: .mouse(0),
        .l3: .speedToggle, .r3: .mouse(2), .l4: .keyboard, .r4: .launcher,
        .up: .key(HID.up), .down: .key(HID.down), .left: .key(HID.left), .right: .key(HID.right),
        .menu: .quickMenu, .options: .key(HID.enter),
    ])

    /// Elden Ring's own PC keys, laid out like its Xbox controls (owner,
    /// 2026-10-20: the Kishi did nothing in the game, because a Mac can't
    /// take a virtual controller). Two layouts, because A, B and the
    /// bumpers mean different keys in play and in menus; the phone uses
    /// this one while the Mac says the game holds the mouse.
    static let eldenPlay = ControlMap(bindings: [
        .leftStick: .wasd, .rightStick: .look,
        .a: .key(HID.letter("f")!), .b: .key(HID.space), .x: .key(HID.letter("r")!), .y: .key(HID.letter("e")!),
        .l1: .mouse(1), .r1: .mouse(0), .l2: .shiftClick(1), .r2: .shiftClick(0),
        .l3: .key(HID.letter("x")!), .r3: .key(HID.letter("q")!), .l4: .quickMenu, .r4: .launcher,
        .up: .key(HID.up), .down: .key(HID.down), .left: .key(HID.left), .right: .key(HID.right),
        .menu: .key(HID.escape), .options: .key(HID.letter("g")!),
    ])

    /// Elden Ring's menus: E is OK and Q is Back (Escape can't back out of
    /// option screens). Z / X for the tab bumpers is from one guide only;
    /// to be confirmed in game. The right stick stays mouse look so a nudge
    /// also tells the Mac the pointer is free.
    static let eldenMenu = ControlMap(bindings: [
        .leftStick: .arrows, .rightStick: .look,
        .a: .key(HID.letter("e")!), .b: .key(HID.letter("q")!), .x: .key(HID.letter("r")!), .y: .key(HID.letter("f")!),
        .l1: .key(HID.letter("z")!), .r1: .key(HID.letter("x")!), .l2: .none, .r2: .none,
        .l3: .none, .r3: .none, .l4: .quickMenu, .r4: .launcher,
        .up: .key(HID.up), .down: .key(HID.down), .left: .key(HID.left), .right: .key(HID.right),
        .menu: .key(HID.escape), .options: .key(HID.letter("g")!),
    ])

    /// Keyboard-and-mouse games: W A S D, mouse look, FPS buttons.
    static let game = ControlMap(bindings: [
        .leftStick: .wasd, .rightStick: .look,
        .a: .key(HID.space), .b: .key(HID.leftCtrl), .x: .key(HID.letter("r")!), .y: .key(HID.letter("e")!),
        .l1: .key(HID.leftShift), .r1: .key(HID.letter("q")!), .l2: .mouse(1), .r2: .mouse(0),
        .l3: .key(HID.leftShift), .r3: .key(HID.letter("f")!), .l4: .quickMenu, .r4: .keyboard,
        .up: .key(HID.one), .down: .key(HID.three), .left: .key(HID.four), .right: .key(HID.two),
        .menu: .quickMenu, .options: .key(HID.escape),
    ])
}
