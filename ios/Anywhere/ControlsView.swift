// Edit the Custom layout: one row per control on the pad; left/right (or
// A) cycles what it does. Hold R1 while pressing right/left to jump ten
// at a time through the long key list. Y resets a row to the game layout;
// Options copies the whole Desktop or Game layout in as a starting point.

import SwiftUI

/// Which layout the editor changes.
enum ControlsTarget: Equatable {
    /// The Custom layout (Settings → Edit controls).
    case custom
    /// The play layout of one game (key, display name), from the quick
    /// menu while it is in front; saved for that game only.
    case game(String, String)
}

struct ControlsView: View {
    @EnvironmentObject var engine: Engine
    @EnvironmentObject var pad: Pad
    @EnvironmentObject var router: Router
    @State private var focus = 0
    @State private var token = 0
    var target: ControlsTarget = .custom
    /// Where B goes; nil = back to Settings.
    var onClose: (() -> Void)?

    init(target: ControlsTarget = .custom, onClose: (() -> Void)? = nil) {
        self.target = target
        self.onClose = onClose
    }

    private let controls = PadControl.allCases

    /// The layout being edited.
    private var map: ControlMap {
        get {
            switch target {
            case .custom: return engine.prefs.customMap
            case .game(let key, _): return engine.prefs.gameMaps[key] ?? engine.gamePlayMap
            }
        }
        nonmutating set {
            switch target {
            case .custom: engine.prefs.customMap = newValue
            case .game(let key, _): engine.prefs.gameMaps[key] = newValue
            }
        }
    }

    private var title: String {
        switch target {
        case .custom: return "Custom controls"
        case .game(_, let name): return "Buttons for \(name)"
        }
    }

    var body: some View {
        HStack(spacing: 0) {
            VStack(alignment: .leading, spacing: 6) {
                Text(title).font(.system(size: 30, weight: .bold, design: .rounded)).lineLimit(1).minimumScaleFactor(0.6).padding(.bottom, 6)
                ScrollViewReader { proxy in
                    ScrollView(showsIndicators: false) {
                        VStack(spacing: 6) {
                            ForEach(Array(controls.enumerated()), id: \.element) { i, c in
                                FocusTile(focused: focus == i, width: 460, height: 44) {
                                    HStack {
                                        Text(c.label).font(.system(size: 15, weight: .semibold))
                                        Spacer()
                                        Text(map[c].label).font(.system(size: 15)).foregroundStyle(Theme.accent)
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
                HintBar(hints: [("◀ ▶", "Change"), ("R1", "×10"), ("Y", "Reset row"), ("X", "Reset all"), ("⧉", "Start from"), ("B", "Done")])
            }
            .padding(28)

            VStack(alignment: .leading, spacing: 12) {
                Text("How it works").font(.system(size: 18, weight: .semibold))
                Text("Sticks can be the pointer, mouse look, scroll, W A S D or arrow keys. Buttons can be a key (held while the button is held), a mouse button, Back, or an app action: Keyboard, Launcher, Quick menu, Pointer speed, Zoom, Magnify (while held).")
                    .font(.system(size: 14)).foregroundStyle(Theme.muted)
                Text(target == .custom
                     ? "Choose \"Custom\" under Controls (Settings or the quick menu) to use this layout. The Home button always opens the quick menu."
                     : "These are the keys while the game holds the mouse (playing). In its menus the pad is the pointer: left stick moves, A clicks, B is Escape. Saved for this game only; X puts its built-in layout back.")
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
        let current = map[c]
        let i = choices.firstIndex(of: current) ?? 0
        let step = big ? 10 : 1
        let next = ((i + dir * step) % choices.count + choices.count) % choices.count
        var m = map
        m[c] = choices[next]
        map = m
    }

    private func handle(_ ev: NavEvent) {
        switch ev {
        case .up: focus = max(0, focus - 1)
        case .down: focus = min(controls.count - 1, focus + 1)
        case .right, .confirm: cycle(1, big: pad.r1Held)
        case .left: cycle(-1, big: pad.r1Held)
        case .y:
            // Reset this row to the built-in layout.
            var m = map
            let base: ControlMap
            switch target {
            case .custom: base = .game
            case .game(let key, _): base = Engine.builtInGameMap(key)
            }
            m[controls[focus]] = base[controls[focus]]
            map = m
        case .options:
            // Cycle the ready-made layouts as a starting point.
            let bases: [ControlMap] = [.game, .desktop, .eldenPlay]
            let i = bases.firstIndex(of: map) ?? -1
            map = bases[(i + 1) % bases.count]
        case .x:
            // Whole layout back to the built-in one (a game forgets its own).
            switch target {
            case .custom: engine.prefs.customMap = .game
            case .game(let key, _): engine.prefs.gameMaps.removeValue(forKey: key)
            }
        case .back, .home:
            if let onClose { onClose() } else { router.screen = .settings }
        default: break
        }
    }
}
