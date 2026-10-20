// The quick menu (Home button while watching): the few things you need
// mid-session, in one column the d-pad walks down. A chooses, B closes.

import SwiftUI

struct QuickMenu: View {
    @Binding var overlay: Overlay
    @EnvironmentObject var engine: Engine
    @EnvironmentObject var pad: Pad
    @EnvironmentObject var router: Router
    @State private var focus = 0
    @State private var token = 0

    private struct Item {
        let symbol: String
        let title: String
        let value: String
        let action: () -> Void
    }

    private var items: [Item] {
        let p = engine.prefs
        return [
            Item(symbol: "square.grid.3x3.fill", title: "Launcher", value: "Open apps and games") { overlay = .launcher },
            Item(symbol: "keyboard", title: "Keyboard", value: "Type on the Mac") { overlay = .keyboard },
            Item(symbol: "gamecontroller", title: "Controls", value: p.profile.label) {
                let all = InputProfile.allCases
                let i = all.firstIndex(of: p.profile) ?? 0
                engine.prefs.profile = all[(i + 1) % all.count]
            },
            Item(symbol: "cursorarrow.motionlines", title: "Pointer speed", value: String(format: "%.1f×", p.pointerSpeed)) {
                let next = p.pointerSpeed >= 2.0 ? 0.5 : p.pointerSpeed + 0.25
                engine.prefs.pointerSpeed = next
                pad.mapper.pointerSpeed = next
            },
            Item(symbol: "speaker.slash", title: "Mac speakers", value: p.muteMac ? "Muted while connected" : "Playing there too") {
                engine.setMuteMac(!p.muteMac)
            },
            Item(symbol: "chart.bar", title: "Stats corner", value: p.showStats ? "On" : "Off") { engine.prefs.showStats.toggle() },
            Item(symbol: "gauge.with.dots.needle.67percent", title: "Data cap", value: "\(Int(p.maxMbps)) Mbps") {
                let steps: [Float] = [8, 15, 30, 50, 80]
                let i = steps.firstIndex { $0 >= p.maxMbps } ?? 0
                engine.setMaxMbps(steps[(i + 1) % steps.count])
            },
            Item(symbol: "xmark.circle", title: "Disconnect", value: "Back to Home") { router.screen = .home },
        ]
    }

    var body: some View {
        ZStack {
            Color.black.opacity(0.55).ignoresSafeArea().onTapGesture { overlay = .none }
            HStack {
                VStack(alignment: .leading, spacing: 8) {
                    Text(engine.status.sharing.isEmpty ? "Anywhere" : engine.status.sharing)
                        .font(.system(size: 20, weight: .bold, design: .rounded)).padding(.bottom, 6)
                    ForEach(Array(items.enumerated()), id: \.offset) { i, it in
                        FocusTile(focused: focus == i, width: 340, height: 54) {
                            HStack(spacing: 14) {
                                Image(systemName: it.symbol).frame(width: 26).foregroundStyle(Theme.accent)
                                Text(it.title).font(.system(size: 16, weight: .semibold))
                                Spacer()
                                Text(it.value).font(.system(size: 13)).foregroundStyle(Theme.muted).lineLimit(1)
                            }
                            .padding(.horizontal, 16)
                        }
                        .onTapGesture { focus = i; it.action() }
                    }
                    Spacer()
                    HintBar(hints: [("A", "Choose"), ("B", "Close")])
                }
                .padding(28)
                .frame(maxHeight: .infinity)
                .background(Color(red: 0.08, green: 0.09, blue: 0.13).opacity(0.96))
                Spacer()
            }
            .ignoresSafeArea()
        }
        .onAppear { token = pad.push(handle) }
        .onDisappear { pad.pop(token) }
    }

    private func handle(_ ev: NavEvent) {
        switch ev {
        case .up: focus = max(0, focus - 1)
        case .down: focus = min(items.count - 1, focus + 1)
        case .confirm: items[focus].action()
        case .back, .home: overlay = .none
        default: break
        }
    }
}
