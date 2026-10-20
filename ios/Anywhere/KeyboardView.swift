// An on-screen keyboard the controller can drive: d-pad moves, A types,
// B deletes (hold to repeat is the Mac's job), Y is shift, X is space,
// Menu is Return, Options closes. Tapping keys works too, and "Phone
// keyboard" brings up iOS's own keyboard for long text.

import SwiftUI

struct KeyboardView: View {
    @Binding var overlay: Overlay
    @EnvironmentObject var engine: Engine
    @EnvironmentObject var pad: Pad
    @State private var row = 1
    @State private var col = 0
    @State private var shift = false
    @State private var symbols = false
    @State private var token = 0
    @State private var typed = ""
    @FocusState private var phoneKeyboard: Bool

    private struct Key: Identifiable {
        let id: String
        let label: String
        let width: CGFloat
        let press: () -> Void
    }

    /// How much of the screen the keyboard takes; the picture shrinks
    /// to the space above it.
    static let height: CGFloat = 212

    private var letters: [[String]] {
        if symbols {
            return [
                ["1", "2", "3", "4", "5", "6", "7", "8", "9", "0"],
                ["!", "@", "#", "$", "%", "^", "&", "*", "(", ")"],
                ["-", "_", "=", "+", "[", "]", ";", ":", "'", "\""],
                ["{", "}", "\\", "|", ",", ".", "/", "?", "<", ">"],
            ]
        }
        return [
            ["q", "w", "e", "r", "t", "y", "u", "i", "o", "p"],
            ["a", "s", "d", "f", "g", "h", "j", "k", "l", "`"],
            ["z", "x", "c", "v", "b", "n", "m", ",", ".", "~"],
        ]
    }

    private var rows: [[Key]] {
        var out: [[Key]] = letters.map { line in
            line.map { ch in
                let shown = shift ? ch.uppercased() : ch
                return Key(id: ch, label: shown, width: 52) { typeChar(shown) }
            }
        }
        out.append([
            Key(id: "sym", label: symbols ? "abc" : "123", width: 64) { symbols.toggle() },
            Key(id: "shift", label: "⇧", width: 48) { shift.toggle() },
            Key(id: "space", label: "space", width: 176) { engine.tap(HID.space) },
            Key(id: "bs", label: "⌫", width: 56) { engine.tap(HID.backspace) },
            Key(id: "ret", label: "return", width: 76) { engine.tap(HID.enter) },
            Key(id: "esc", label: "esc", width: 52) { engine.tap(HID.escape) },
            Key(id: "tab", label: "tab", width: 52) { engine.tap(HID.tab) },
            Key(id: "cmd-tab", label: "⌘⇥", width: 52) { engine.tap(HID.tab, modifiers: [HID.leftGui]) },
            Key(id: "left", label: "←", width: 40) { engine.tap(HID.left) },
            Key(id: "right", label: "→", width: 40) { engine.tap(HID.right) },
            Key(id: "phone", label: "iOS ⌨", width: 60) { phoneKeyboard = true },
        ])
        return out
    }

    var body: some View {
        VStack(spacing: 0) {
            Spacer()
            VStack(spacing: 6) {
                ForEach(Array(rows.enumerated()), id: \.offset) { ri, line in
                    HStack(spacing: 6) {
                        ForEach(Array(line.enumerated()), id: \.element.id) { ci, k in
                            FocusTile(focused: ri == row && ci == col, width: k.width, height: 34) {
                                Text(k.label).font(.system(size: 16, weight: .medium, design: .rounded))
                                    .foregroundStyle((k.id == "shift" && shift) || (k.id == "sym" && symbols) ? Theme.accent : Theme.text)
                            }
                            .onTapGesture { row = ri; col = ci; k.press() }
                        }
                    }
                }
                HStack(spacing: 16) {
                    ButtonHint(glyph: "A", text: "Type")
                    ButtonHint(glyph: "B", text: "Delete")
                    ButtonHint(glyph: "X", text: "Space")
                    ButtonHint(glyph: "Y", text: "Shift")
                    ButtonHint(glyph: "⧉", text: "Return")
                    ButtonHint(glyph: "≡", text: "Menu")
                    ButtonHint(glyph: "L1", text: "123")
                    Spacer()
                    // Hidden field for the phone's own keyboard.
                    TextField("", text: $typed)
                        .focused($phoneKeyboard)
                        .frame(width: 1, height: 1).opacity(0.02)
                        .onChange(of: typed) { old, new in forward(old, new) }
                        .onSubmit { engine.tap(HID.enter); phoneKeyboard = true }
                }
                .padding(.top, 2)
            }
            .padding(.horizontal, 14).padding(.vertical, 10)
            .frame(maxWidth: .infinity)
            .frame(height: Self.height)
            .background(Color(red: 0.08, green: 0.09, blue: 0.13))
        }
        .ignoresSafeArea()
        .onAppear { token = pad.push(handle) }
        .onDisappear { pad.pop(token) }
    }

    private func typeChar(_ s: String) {
        guard let ch = s.first, let u = HID.usage(for: ch) else { return }
        engine.tap(u.0, modifiers: u.1 ? [HID.leftShift] : [])
        if shift && !symbols { shift = false }
    }

    /// The phone's keyboard: send whatever was added or removed.
    private func forward(_ old: String, _ new: String) {
        if new.count < old.count {
            for _ in 0..<(old.count - new.count) { engine.tap(HID.backspace) }
        } else if new.hasPrefix(old) {
            for ch in new.dropFirst(old.count) { typeChar(String(ch)) }
        } else {
            for ch in new { typeChar(String(ch)) }
        }
        if typed.count > 200 { typed = "" }
    }

    private func handle(_ ev: NavEvent) {
        let r = rows
        switch ev {
        case .up: row = max(0, row - 1); col = min(col, r[row].count - 1)
        case .down: row = min(r.count - 1, row + 1); col = min(col, r[row].count - 1)
        case .left: col = max(0, col - 1)
        case .right: col = min(r[row].count - 1, col + 1)
        case .confirm: r[row][col].press()
        case .back: engine.tap(HID.backspace)
        case .x: engine.tap(HID.space)
        case .y: shift.toggle()
        case .options: engine.tap(HID.enter)
        case .l1: symbols.toggle()
        case .r1: engine.tap(HID.tab)
        case .menu, .home: overlay = .quickMenu
        default: break
        }
    }
}
