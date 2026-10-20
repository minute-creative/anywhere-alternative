// The quick menu (Home button while watching): a control bar along the
// bottom, like a console's control centre. Left / right move, A chooses,
// B (or Home again) closes. The picture stays visible above it.

import SwiftUI

struct QuickMenu: View {
    @Binding var overlay: Overlay
    @EnvironmentObject var engine: Engine
    @EnvironmentObject var pad: Pad
    @EnvironmentObject var router: Router
    @State private var focus = 0
    @State private var token = 0

    private struct Item: Identifiable {
        let id: String
        let symbol: String
        let title: String
        let value: String
        let action: () -> Void
    }

    private var items: [Item] {
        let p = engine.prefs
        let s = engine.status
        return [
            Item(id: "launcher", symbol: "square.grid.2x2.fill", title: "Launcher", value: "Apps and games") { overlay = .launcher },
            Item(id: "keyboard", symbol: "keyboard.fill", title: "Keyboard", value: "Type on the Mac") { overlay = .keyboard },
            Item(id: "desktop", symbol: "macwindow", title: "Whole Mac", value: s.sharing.isEmpty ? "Showing everything" : s.sharing) {
                engine.showDesktop()
                overlay = .none
            },
            Item(id: "controls", symbol: "gamecontroller.fill", title: "Controls", value: p.profile.label) {
                let all = InputProfile.allCases
                let i = all.firstIndex(of: p.profile) ?? 0
                engine.prefs.profile = all[(i + 1) % all.count]
            },
            Item(id: "help", symbol: "questionmark.circle.fill", title: "Show controls", value: "What the buttons do") { overlay = .help },
            Item(id: "zoom", symbol: "plus.magnifyingglass", title: "Zoom", value: engine.zoom == 1 ? "Off" : "\(Int(engine.zoom))×") {
                engine.zoom = engine.zoom >= 3 ? 1 : engine.zoom + 1
            },
            Item(id: "fullscreen", symbol: "arrow.up.left.and.arrow.down.right", title: "Full screen", value: "⌃⌘F on the Mac") {
                engine.tap(HID.letter("f")!, modifiers: [HID.leftCtrl, HID.leftGui])
                overlay = .none
            },
            // Mac window buttons are a few points wide on a phone; these do
            // the same through shortcuts, no aiming needed (owner, 0.6.0).
            Item(id: "close", symbol: "xmark.square.fill", title: "Close window", value: "⌘W on the Mac") {
                engine.tap(HID.letter("w")!, modifiers: [HID.leftGui])
                overlay = .none
            },
            Item(id: "minimise", symbol: "minus.square.fill", title: "Minimise", value: "⌘M on the Mac") {
                engine.tap(HID.letter("m")!, modifiers: [HID.leftGui])
                overlay = .none
            },
            Item(id: "nextwin", symbol: "rectangle.on.rectangle", title: "Next window", value: "⌘` on the Mac") {
                engine.tap(HID.grave, modifiers: [HID.leftGui])
                overlay = .none
            },
            Item(id: "quit", symbol: "power", title: "Quit app", value: "⌘Q on the Mac") {
                engine.tap(HID.letter("q")!, modifiers: [HID.leftGui])
                overlay = .none
            },
            Item(id: "mute", symbol: p.muteMac ? "speaker.slash.fill" : "speaker.wave.2.fill", title: "Mac speakers", value: p.muteMac ? "Muted there" : "Playing there") {
                engine.setMuteMac(!p.muteMac)
            },
            Item(id: "speed", symbol: "cursorarrow.motionlines", title: "Pointer", value: String(format: "%.2g× speed", p.pointerSpeed)) {
                let next = p.pointerSpeed >= 2.0 ? 0.5 : p.pointerSpeed + 0.25
                engine.prefs.pointerSpeed = next
                pad.mapper.pointerSpeed = next
            },
            Item(id: "cap", symbol: "gauge.with.dots.needle.67percent", title: "Data cap", value: "\(Int(p.maxMbps)) Mbps") {
                let steps: [Float] = [8, 15, 30, 50, 80]
                let i = steps.firstIndex { $0 >= p.maxMbps } ?? 0
                engine.setMaxMbps(steps[(i + 1) % steps.count])
            },
            Item(id: "stats", symbol: "chart.bar.fill", title: "Stats", value: p.showStats ? "Shown" : "Hidden") { engine.prefs.showStats.toggle() },
            Item(id: "clip", symbol: "doc.on.clipboard.fill", title: "Send clipboard", value: "Phone → Mac") {
                if let t = UIPasteboard.general.string, !t.isEmpty { engine.copyText(t) }
                overlay = .none
            },
            Item(id: "off", symbol: "xmark.circle.fill", title: "Disconnect", value: "Back to Home") { router.screen = .home },
        ]
    }

    var body: some View {
        ZStack {
            Color.clear.contentShape(Rectangle()).onTapGesture { overlay = .none }
            VStack(spacing: 10) {
                Spacer()
                statusLine
                ScrollViewReader { proxy in
                    ScrollView(.horizontal, showsIndicators: false) {
                        HStack(spacing: 12) {
                            ForEach(Array(items.enumerated()), id: \.element.id) { i, it in
                                tile(it, focused: focus == i).id(it.id)
                                    .onTapGesture { focus = i; it.action() }
                            }
                        }
                        .padding(.horizontal, 28).padding(.vertical, 14)
                    }
                    .onChange(of: focus) { _, _ in
                        withAnimation(.spring(response: 0.3)) { proxy.scrollTo(items[min(focus, items.count - 1)].id, anchor: .center) }
                    }
                }
                .background(
                    LinearGradient(colors: [.clear, .black.opacity(0.85)], startPoint: .top, endPoint: .bottom)
                        .ignoresSafeArea()
                )
            }
        }
        .onAppear { token = pad.push(handle) }
        .onDisappear { pad.pop(token) }
    }

    private var statusLine: some View {
        let s = engine.status
        return HStack(spacing: 14) {
            Text(s.sharing.isEmpty ? "Anywhere" : s.sharing).font(.system(size: 16, weight: .bold, design: .rounded))
            if s.connected {
                Text("\(s.fps) fps · \(String(format: "%.1f", s.mbps)) Mbps · \(Int(s.rtt_ms)) ms · \(s.codec)")
                    .font(.system(size: 12, design: .monospaced)).foregroundStyle(Theme.muted)
            }
            Spacer()
            HintBar(hints: [("A", "Choose"), ("B", "Close")])
        }
        .padding(.horizontal, 28)
    }

    private func tile(_ it: Item, focused: Bool) -> some View {
        VStack(spacing: 6) {
            Image(systemName: it.symbol).font(.system(size: 26, weight: .medium))
                .frame(width: 64, height: 64)
                .background(RoundedRectangle(cornerRadius: 18, style: .continuous).fill(focused ? Color.white : Color.white.opacity(0.14)))
                .foregroundStyle(focused ? .black : Theme.text)
            Text(it.title).font(.system(size: 12, weight: .semibold)).foregroundStyle(focused ? Theme.text : Theme.muted)
            Text(it.value).font(.system(size: 10)).foregroundStyle(Theme.muted).lineLimit(1)
        }
        .frame(width: 104)
        .scaleEffect(focused ? 1.1 : 1.0)
        .animation(.spring(response: 0.25, dampingFraction: 0.8), value: focused)
    }

    private func handle(_ ev: NavEvent) {
        switch ev {
        case .left: focus = max(0, focus - 1)
        case .right: focus = min(items.count - 1, focus + 1)
        case .confirm: items[focus].action()
        case .back, .home, .menu: overlay = .none
        default: break
        }
    }
}
