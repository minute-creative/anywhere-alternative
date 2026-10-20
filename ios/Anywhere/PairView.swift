// Pairing: pick the computer, type the six-digit code from its Share
// page on a number pad the controller can drive. Happens once per Mac.

import SwiftUI

struct PairView: View {
    @EnvironmentObject var engine: Engine
    @EnvironmentObject var pad: Pad
    @EnvironmentObject var router: Router

    @State private var found: [FoundHost] = []
    @State private var chosen: FoundHost?
    @State private var manual = ""
    @State private var code = ""
    @State private var focus = 0
    @State private var padFocus = 0
    @State private var busy = false
    @State private var error = ""
    @State private var token = 0
    @FocusState private var typing: Bool

    private let keys = ["1", "2", "3", "4", "5", "6", "7", "8", "9", "⌫", "0", "✓"]

    var body: some View {
        HStack(spacing: 0) {
            // Left: which computer.
            VStack(alignment: .leading, spacing: 14) {
                Text("Add a computer").font(.system(size: 30, weight: .bold, design: .rounded))
                Text("On the Mac, open Anywhere → Share. It shows a six-digit code.")
                    .font(.system(size: 14)).foregroundStyle(Theme.muted)
                ScrollView {
                    VStack(spacing: 10) {
                        ForEach(Array(found.enumerated()), id: \.element.id) { i, f in
                            FocusTile(focused: chosen == nil && focus == i, height: 64) {
                                HStack {
                                    Image(systemName: "desktopcomputer").foregroundStyle(Theme.accent)
                                    VStack(alignment: .leading) {
                                        Text(f.name).font(.system(size: 17, weight: .semibold))
                                        Text(f.paired ? "Already paired" : f.addr).font(.system(size: 12)).foregroundStyle(Theme.muted)
                                    }
                                    Spacer()
                                }
                                .padding(.horizontal, 16)
                            }
                            .onTapGesture { focus = i; choose(f) }
                        }
                        FocusTile(focused: chosen == nil && focus == found.count, height: 64) {
                            HStack {
                                Image(systemName: "network").foregroundStyle(Theme.muted)
                                TextField("Type an address (Tailscale 100.x.y.z)", text: $manual)
                                    .textInputAutocapitalization(.never).autocorrectionDisabled()
                                    .keyboardType(.numbersAndPunctuation)
                                    .focused($typing)
                                    .onSubmit { chooseManual() }
                                    .foregroundStyle(Theme.text)
                            }
                            .padding(.horizontal, 16)
                        }
                        .onTapGesture { focus = found.count; typing = true }
                        if found.isEmpty {
                            Text("Looking on this Wi-Fi… From elsewhere, type the Mac's Tailscale address.")
                                .font(.system(size: 13)).foregroundStyle(Theme.muted).padding(.top, 6)
                        }
                    }
                }
                Spacer()
                HintBar(hints: chosen == nil ? [("A", "Choose"), ("B", "Back")] : [("A", "Type"), ("B", "Back")])
            }
            .padding(32)
            .frame(maxWidth: .infinity, alignment: .leading)

            // Right: the code.
            VStack(spacing: 18) {
                Text(chosen.map { "Code for \($0.name)" } ?? "Choose a computer first")
                    .font(.system(size: 18, weight: .semibold)).foregroundStyle(chosen == nil ? Theme.muted : Theme.text)
                HStack(spacing: 10) {
                    ForEach(0..<6, id: \.self) { i in
                        let ch = i < code.count ? String(code[code.index(code.startIndex, offsetBy: i)]) : ""
                        Text(ch).font(.system(size: 30, weight: .bold, design: .monospaced))
                            .frame(width: 44, height: 58)
                            .background(RoundedRectangle(cornerRadius: 12).fill(Theme.glass))
                            .overlay(RoundedRectangle(cornerRadius: 12).stroke(i == code.count ? Theme.accent : .clear, lineWidth: 2))
                    }
                }
                LazyVGrid(columns: Array(repeating: GridItem(.fixed(72), spacing: 10), count: 3), spacing: 10) {
                    ForEach(Array(keys.enumerated()), id: \.offset) { i, k in
                        FocusTile(focused: chosen != nil && padFocus == i, width: 72, height: 54) {
                            Text(k).font(.system(size: 22, weight: .semibold, design: .rounded))
                                .foregroundStyle(k == "✓" ? Theme.good : Theme.text)
                        }
                        .onTapGesture { padFocus = i; press(k) }
                    }
                }
                .opacity(chosen == nil ? 0.4 : 1)
                if busy { ProgressView().tint(.white) }
                if !error.isEmpty {
                    Text(error).font(.system(size: 13)).foregroundStyle(Theme.warn).multilineTextAlignment(.center)
                }
            }
            .padding(32)
            .frame(width: 360)
            .background(Color.black.opacity(0.25))
        }
        .onAppear {
            pad.mode = .ui
            token = pad.push(handle)
            Task { await search() }
        }
        .onDisappear { pad.pop(token) }
    }

    private func choose(_ f: FoundHost) {
        chosen = f
        code = ""
        error = f.paired ? "Already paired; pairing again just refreshes it." : ""
    }

    private func chooseManual() {
        let a = manual.trimmingCharacters(in: .whitespaces)
        guard !a.isEmpty else { return }
        typing = false
        chosen = FoundHost(name: a, addr: a, key: "", paired: false, version: "", tailscale: a.hasPrefix("100."))
        code = ""
        error = ""
    }

    private func handle(_ ev: NavEvent) {
        if chosen != nil {
            switch ev {
            case .up, .down, .left, .right: padFocus = FocusMove.step(padFocus, ev, columns: 3, count: keys.count)
            case .confirm: press(keys[padFocus])
            case .back:
                if code.isEmpty { chosen = nil } else { code.removeLast() }
            default: break
            }
            return
        }
        switch ev {
        case .up: focus = max(0, focus - 1)
        case .down: focus = min(found.count, focus + 1)
        case .confirm:
            if focus < found.count { choose(found[focus]) } else if manual.isEmpty { typing = true } else { chooseManual() }
        case .back: router.screen = .home
        default: break
        }
    }

    private func press(_ k: String) {
        guard chosen != nil, !busy else { return }
        switch k {
        case "⌫": if !code.isEmpty { code.removeLast() }
        case "✓": submit()
        default:
            if code.count < 6 { code += k }
            if code.count == 6 { submit() }
        }
    }

    private func submit() {
        guard let c = chosen, code.count == 6 else { return }
        busy = true
        error = ""
        Task {
            let result = await engine.pair(addr: c.addr, code: code)
            busy = false
            if let result {
                error = result
                code = ""
            } else {
                router.screen = .home
            }
        }
    }

    private func search() async {
        while router.screen == .pair {
            let list = await engine.discover()
            if chosen == nil || !list.isEmpty { found = list }
            try? await Task.sleep(nanoseconds: 2_500_000_000)
        }
    }
}
