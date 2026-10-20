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

    private var letters: [[String]] {
        if symbols {
            return [
                ["1", "2", "3", "4", "5", "6", "7", "8", "9", "0"],
                ["!", "@", "#", "$", "%", "^", "&", "*", "(", ")"],
                ["-", "_", "=", "+", "[", "]", "{", "}", "\\", "|"],
                [";", ":", "'", "\"", ",", ".", "/", "?", "<", ">"],
            ]
        }
        return [
            ["1", "2", "3", "4", "5", "6", "7", "8", "9", "0"],
            ["q", "w", "e", "r", "t", "y", "u", "i", "o", "p"],
            ["a", "s", "d", "f", "g", "h", "j", "k", "l", "`"],
            ["z", "x", "c", "v", "b", "n", "m", ",", ".", "~"],
        ]
    }

    private var rows: [[Key]] {
        var out: [[Key]] = letters.map { line in
            line.map { ch in
                let shown = shift ? ch.uppercased() : ch
                return Key(id: ch, label: shown, width: 58) { type(shown) }
            }
        }
        out.append([
            Key(id: "sym", label: symbols ? "abc" : "#+=", width: 80) { symbols.toggle() },
            Key(id: "shift", label: "⇧", width: 58) { shift.toggle() },
            Key(id: "space", label: "space", width: 240) { engine.tap(HID.space) },
            Key(id: "bs", label: "⌫", width: 70) { engine.tap(HID.backspace) },
            Key(id: "ret", label: "return", width: 96) { engine.tap(HID.enter) },
        ])
        out.append([
            Key(id: "esc", label: "esc", width: 70) { engine.tap(HID.escape) },
            Key(id: "tab", label: "tab", width: 70) { engine.tap(HID.tab) },
            Key(id: "cmd-tab", label: "⌘ tab", width: 90) { engine.tap(HID.tab, modifiers: [HID.leftGui]) },
            Key(id: "cmd-space", label: "⌘ space", width: 110) { engine.tap(HID.space, modifiers: [HID.leftGui]) },
            Key(id: "cmd-q", label: "⌘ Q", width: 70) { engine.tap(HID.letter("q")!, modifiers: [HID.leftGui]) },
            Key(id: "left", label: "←", width: 50) { engine.tap(HID.left) },
            Key(id: "right", label: "→", width: 50) { engine.tap(HID.right) },
            Key(id: "phone", label: "Phone keyboard", width: 150) { phoneKeyboard = true },
        ])
        return out
    }

    var body: some View {
        ZStack {
            Color.clear.contentShape(Rectangle()).onTapGesture { overlay = .none }
            VStack {
                Spacer()
                VStack(spacing: 8) {
                    ForEach(Array(rows.enumerated()), id: \.offset) { ri, line in
                        HStack(spacing: 8) {
                            ForEach(Array(line.enumerated()), id: \.element.id) { ci, k in
                                FocusTile(focused: ri == row && ci == col, width: k.width, height: 44) {
                                    Text(k.label).font(.system(size: 18, weight: .medium, design: .rounded))
                                        .foregroundStyle((k.id == "shift" && shift) || (k.id == "sym" && symbols) ? Theme.accent : Theme.text)
                                }
                                .onTapGesture { row = ri; col = ci; k.press() }
                            }
                        }
                    }
                    HStack {
                        HintBar(hints: [("A", "Type"), ("B", "Delete"), ("X", "Space"), ("Y", "Shift"), ("≡", "Return"), ("⧉", "Close")])
                        Spacer()
                        // Hidden field for the phone's own keyboard.
                        TextField("", text: $typed)
                            .focused($phoneKeyboard)
                            .frame(width: 1, height: 1).opacity(0.02)
                            .onChange(of: typed) { old, new in forward(old, new) }
                            .onSubmit { engine.tap(HID.enter); phoneKeyboard = true }
                    }
                    .padding(.top, 4)
                }
                .padding(16)
                .background(Color(red: 0.08, green: 0.09, blue: 0.13).opacity(0.94))
                .clipShape(RoundedRectangle(cornerRadius: 24, style: .continuous))
                .padding(.bottom, 10)
            }
        }
        .onAppear { token = pad.push(handle) }
        .onDisappear { pad.pop(token) }
    }

    private func type(_ s: String) {
        guard let ch = s.first, let u = HID.usage(for: ch) else { return }
        engine.tap(u.0, modifiers: u.1 ? [HID.leftShift] : [])
        if shift && !symbols { shift = false }
    }

    /// The phone's keyboard: send whatever was added or removed.
    private func forward(_ old: String, _ new: String) {
        if new.count < old.count {
            for _ in 0..<(old.count - new.count) { engine.tap(HID.backspace) }
        } else if new.hasPrefix(old) {
            for ch in new.dropFirst(old.count) { type(String(ch)) }
        } else {
            for ch in new { type(String(ch)) }
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
        case .menu: engine.tap(HID.enter)
        case .l1: symbols.toggle()
        case .r1: engine.tap(HID.tab)
        case .options, .home: overlay = .none
        default: break
        }
    }
}
