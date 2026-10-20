// Watching the Mac. The picture fills the screen; over it, depending on
// what is open: the pointer (desktop profile), a small stats corner, the
// quick menu (Home button), the launcher (Y), the keyboard (X), or a
// notice while connecting / reconnecting / paused.

import SwiftUI

enum Overlay: Equatable {
    case none
    case quickMenu
    case launcher
    case keyboard
    /// The current layout, as a cheat-sheet.
    case help
}

struct StreamScreen: View {
    @EnvironmentObject var engine: Engine
    @EnvironmentObject var pad: Pad
    @EnvironmentObject var router: Router

    @State private var overlay: Overlay = .none
    @State private var videoRect = CGRect.zero
    @State private var pointer = CGPoint(x: 0.5, y: 0.5)
    /// The pointer fades a few seconds after it last moved.
    @State private var pointerVisible = false
    @State private var audio = AudioOutput()
    @State private var pointerTimer: Timer?
    @State private var endedToken = 0
    @Environment(\.scenePhase) private var scenePhase

    var body: some View {
        ZStack {
            Color.black.ignoresSafeArea()
            StreamView(fps: Int(engine.prefs.fps)) { r in videoRect = r }
                .padding(.bottom, overlay == .keyboard ? KeyboardView.height : 0)
                .ignoresSafeArea()
                .animation(.easeInOut(duration: 0.2), value: overlay)

            if showsPointer, overlay == .none, engine.status.connected, videoRect.width > 0 {
                PointerView()
                    .position(pointerOnScreen)
                    .opacity(pointerVisible ? 1 : 0)
                    .animation(.easeOut(duration: 0.4), value: pointerVisible)
                    .allowsHitTesting(false)
                    .ignoresSafeArea()
            }

            if engine.prefs.showStats, engine.status.connected, overlay == .none {
                VStack { Spacer(); HStack { Spacer(); statsCorner } }.padding(10).ignoresSafeArea()
            }

            // Touch fallback for the quick menu (no controller).
            if overlay == .none {
                VStack {
                    HStack {
                        Spacer()
                        Button { open(.quickMenu) } label: {
                            Image(systemName: "line.3.horizontal.circle.fill").font(.system(size: 26))
                                .foregroundStyle(.white.opacity(0.35))
                        }
                        .padding(8)
                    }
                    Spacer()
                }
            }

            notice

            switch overlay {
            case .none: EmptyView()
            case .quickMenu: QuickMenu(overlay: $overlay)
            case .launcher: LauncherView(overlay: $overlay)
            case .keyboard: KeyboardView(overlay: $overlay)
            case .help: ControlsSheet(overlay: $overlay)
            }
        }
        .onAppear { start() }
        .onDisappear { stop() }
        .onChange(of: overlay) { _, o in
            if o == .none {
                pad.mode = .stream
            } else {
                pad.mapper.releaseAll()
                pad.mode = .ui
            }
        }
        .onChange(of: engine.prefs.profile) { _, p in
            pad.mapper.profile = p
            pad.mapper.map = engine.prefs.activeMap
            pad.mapper.reset()
        }
        .onChange(of: scenePhase) { _, phase in
            // Phone locked or app switched away: let go of every key and
            // button so nothing stays held on the Mac; keep the session.
            if phase != .active {
                pad.mapper.releaseAll()
                if overlay == .none { pad.mode = .ui }
            } else if overlay == .none, engine.status.live {
                pad.mode = .stream
            }
        }
        .onChange(of: pad.connected) { _, on in
            if !on { pad.mapper.releaseAll() }
        }
        .onChange(of: engine.status.state) { old, s in
            if s == "ended" && overlay != .none { overlay = .none }
            // Like a console: the launcher greets you on the first connect.
            if s == "connected", old == "connecting", engine.prefs.launcherOnConnect, engine.status.has_launcher {
                open(.launcher)
            }
        }
    }

    /// Where the pointer lands on screen, through the zoom (the zoomed
    /// window follows the pointer, so it sits near the middle when zoomed).
    private var pointerOnScreen: CGPoint {
        let z = engine.zoom
        let win = ZoomWindow.around(pointer, zoom: z)
        let x = videoRect.minX + (pointer.x - win.minX) / win.width * videoRect.width
        let y = videoRect.minY + (pointer.y - win.minY) / win.height * videoRect.height
        return CGPoint(x: x, y: y)
    }

    /// The pointer is drawn when a stick moves it (not in mouse-look games).
    private var showsPointer: Bool {
        let m = engine.prefs.activeMap
        return engine.prefs.profile != .controller && !engine.status.cursor_captured
            && (m[.leftStick] == .pointer || m[.rightStick] == .pointer)
    }

    @ViewBuilder private var notice: some View {
        let st = engine.status
        if st.state == "connecting" {
            Notice(symbol: "antenna.radiowaves.left.and.right", title: "Connecting…", detail: "Finding the Mac and setting up the secure link.")
                .background(Color.black.opacity(0.6)).clipShape(RoundedRectangle(cornerRadius: 24))
        } else if st.state == "reconnecting" {
            Notice(symbol: "arrow.triangle.2.circlepath", title: "Reconnecting…", detail: "The Mac stopped answering; trying again until it is back.")
                .background(Color.black.opacity(0.6)).clipShape(RoundedRectangle(cornerRadius: 24))
        } else if st.state == "ended" {
            VStack(spacing: 14) {
                Notice(symbol: "xmark.circle", title: "Disconnected", detail: st.reason)
                HintBar(hints: [("A", "Try again"), ("B", "Home")])
            }
            .background(Color.black.opacity(0.75)).clipShape(RoundedRectangle(cornerRadius: 24))
            .onAppear { endedHandler() }
        } else if let p = st.paused, overlay == .none {
            VStack { Spacer(); Text(p).font(.system(size: 15, weight: .medium)).padding(12)
                .background(Capsule().fill(Color.black.opacity(0.6))).padding(.bottom, 24) }
        }
    }

    private var statsCorner: some View {
        let s = engine.status
        return Text("\(s.fps) fps · \(String(format: "%.1f", s.mbps)) Mbps · \(Int(s.rtt_ms)) ms · \(String(format: "%.0f", s.loss_pct))% loss · \(s.codec)")
            .font(.system(size: 11, weight: .medium, design: .monospaced))
            .foregroundStyle(.white.opacity(0.7))
            .padding(.horizontal, 8).padding(.vertical, 4)
            .background(Capsule().fill(Color.black.opacity(0.45)))
    }

    private func start() {
        pad.mapper.profile = engine.prefs.profile
        pad.mapper.map = engine.prefs.activeMap
        pad.mapper.pointerSpeed = engine.prefs.pointerSpeed
        pad.mapper.reset()
        pad.mode = .stream
        pad.onStreamEvent = { ev in
            switch ev {
            case .home: open(.quickMenu)
            case .x: open(.keyboard)
            case .y: open(.launcher)
            default: break
            }
        }
        audio.start()
        // The pointer drawn on the phone follows the mapper's position.
        pointerTimer = Timer.scheduledTimer(withTimeInterval: 1.0 / 60.0, repeats: true) { _ in
            Task { @MainActor in
                let p = pad.mapper.pointer
                if p != pointer { pointer = p }
                let idle = Date().timeIntervalSinceReferenceDate - pad.mapper.pointerMovedAt
                let visible = idle < 2.5
                if visible != pointerVisible { pointerVisible = visible }
            }
        }
    }

    private func stop() {
        pad.pop(endedToken)
        pointerTimer?.invalidate()
        pad.onStreamEvent = nil
        pad.mapper.releaseAll()
        pad.mode = .ui
        audio.stop()
        engine.disconnect()
    }

    private func open(_ o: Overlay) {
        guard engine.status.state != "ended" else { return }
        if o == .launcher, engine.apps.isEmpty { engine.requestApps() }
        overlay = o
    }

    private func endedHandler() {
        pad.mode = .ui
        pad.pop(endedToken)
        endedToken = pad.push { ev in
            switch ev {
            case .confirm:
                let last = UserDefaults.standard.string(forKey: "last-host") ?? ""
                if !last.isEmpty { engine.connect(host: last) } else { router.screen = .home }
            case .back, .home: router.screen = .home
            default: break
            }
        }
    }
}

/// The pointer, drawn by the phone (the Mac's own is hidden in the stream).
struct PointerView: View {
    var body: some View {
        Image(systemName: "cursorarrow").font(.system(size: 20, weight: .bold))
            .foregroundStyle(.white)
            .shadow(color: .black.opacity(0.9), radius: 1.5)
            .offset(x: 7, y: 9)
    }
}
