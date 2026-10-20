// Edit the Custom layout: one row per control on the pad; left/right (or
// A) cycles what it does. Hold R1 while pressing right/left to jump ten
// at a time through the long key list. Y resets a row to the game layout;
// Options copies the whole Desktop or Game layout in as a starting point.

import SwiftUI

struct ControlsView: View {
    @EnvironmentObject var engine: Engine
    @EnvironmentObject var pad: Pad
    @EnvironmentObject var router: Router
    @State private var focus = 0
    @State private var token = 0

    private let controls = PadControl.allCases

    var body: some View {
        HStack(spacing: 0) {
            VStack(alignment: .leading, spacing: 6) {
                Text("Custom controls").font(.system(size: 30, weight: .bold, design: .rounded)).padding(.bottom, 6)
                ScrollViewReader { proxy in
                    ScrollView(showsIndicators: false) {
                        VStack(spacing: 6) {
                            ForEach(Array(controls.enumerated()), id: \.element) { i, c in
                                FocusTile(focused: focus == i, width: 460, height: 44) {
                                    HStack {
                                        Text(c.label).font(.system(size: 15, weight: .semibold))
                                        Spacer()
                                        Text(engine.prefs.customMap[c].label).font(.system(size: 15)).foregroundStyle(Theme.accent)
                                    }
                                    .padding(.horizontal, 16)
                                }
                                .id(i)
                                .onTapGesture { focus = i; cycle(1, big: false) }
                            }
                        }
                        .padding(.vertical, 8)
                    }
                    .onChange(of: focus) { _, f in withAnimation { proxy.scrollTo(f, anchor: .center) } }
                }
                HintBar(hints: [("◀ ▶", "Change"), ("R1 + ◀ ▶", "Jump"), ("Y", "Reset row"), ("⧉", "Copy a layout"), ("B", "Back")])
            }
            .padding(28)

            VStack(alignment: .leading, spacing: 12) {
                Text("How it works").font(.system(size: 18, weight: .semibold))
                Text("Sticks can be the pointer, mouse look, scroll, W A S D or arrow keys. Buttons can be a key (held while the button is held), a mouse button, or an app action: Keyboard, Launcher, Quick menu, Pointer speed.")
                    .font(.system(size: 14)).foregroundStyle(Theme.muted)
                Text("Choose \"Custom\" under Controls (Settings or the quick menu) to use this layout. The Home button always opens the quick menu.")
                    .font(.system(size: 14)).foregroundStyle(Theme.muted)
                Spacer()
                Text("Using: \(engine.prefs.profile.label)").font(.system(size: 13)).foregroundStyle(Theme.muted)
            }
            .padding(28)
            .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
            .background(Color.black.opacity(0.25))
        }
        .onAppear { pad.mode = .ui; token = pad.push(handle) }
        .onDisappear { pad.pop(token) }
    }

    private func cycle(_ dir: Int, big: Bool) {
        let c = controls[focus]
        let choices = c.isStick ? PadAction.stickChoices : PadAction.buttonChoices
        let current = engine.prefs.customMap[c]
        let i = choices.firstIndex(of: current) ?? 0
        let step = big ? 10 : 1
        let next = ((i + dir * step) % choices.count + choices.count) % choices.count
        engine.prefs.customMap[c] = choices[next]
    }

    private func handle(_ ev: NavEvent) {
        switch ev {
        case .up: focus = max(0, focus - 1)
        case .down: focus = min(controls.count - 1, focus + 1)
        case .right, .confirm: cycle(1, big: pad.r1Held)
        case .left: cycle(-1, big: pad.r1Held)
        case .y: engine.prefs.customMap[controls[focus]] = ControlMap.game[controls[focus]]
        case .options:
            // Alternate between the two ready-made layouts as a base.
            engine.prefs.customMap = engine.prefs.customMap == .game ? .desktop : .game
        case .back, .home: router.screen = .settings
        default: break
        }
    }
}
