// The controller (a Razer Kishi V2 clipped to the phone, or any pad iOS
// knows). One object reads it and does two jobs:
//
// 1. In menus ("ui" mode) it turns the d-pad / left stick and the face
//    buttons into navigation events with key-repeat, so every screen can
//    be driven like a console: move, confirm (A), back (B), and so on.
// 2. While watching the Mac ("stream" mode) it hands the pad to
//    `InputMapper`, which translates it into mouse, keyboard or controller
//    input for the other computer following the chosen profile, 120 times
//    a second.
//
// The Home button always opens the quick menu, in both modes, so there is
// always a way out. Menu + Options together do the same, for pads whose
// Home button iOS keeps for itself.

import Combine
import Foundation
import GameController
import QuartzCore

/// What a menu screen receives.
enum NavEvent {
    case up, down, left, right
    case confirm, back
    case x, y, l1, r1, l2, r2, l3, r3
    case menu, options
    case home
}

/// Which way the pad's input goes.
enum PadMode {
    case ui
    case stream
}

/// A snapshot of the whole controller.
struct PadState: Equatable {
    var lx: Float = 0, ly: Float = 0, rx: Float = 0, ry: Float = 0
    var lt: Float = 0, rt: Float = 0
    var a = false, b = false, x = false, y = false
    var l1 = false, r1 = false, l2 = false, r2 = false, l3 = false, r3 = false
    var menu = false, options = false, home = false
    var up = false, down = false, left = false, right = false
}

@MainActor
final class Pad: ObservableObject {
    @Published var connected = false
    @Published var name = ""
    @Published var mode: PadMode = .ui
    /// Where menu navigation goes: the screen on top. Screens push a
    /// handler when they appear and pop it when they go, so an overlay
    /// (quick menu, keyboard) takes the pad and hands it back by itself.
    private var navStack: [(Int, (NavEvent) -> Void)] = []
    private var nextToken = 1
    private var onNav: ((NavEvent) -> Void)? { navStack.last?.1 }

    @discardableResult
    func push(_ handler: @escaping (NavEvent) -> Void) -> Int {
        let t = nextToken
        nextToken += 1
        navStack.append((t, handler))
        return t
    }

    func pop(_ token: Int) {
        navStack.removeAll { $0.0 == token }
    }
    /// Things the stream mapper asks the app to do (open the keyboard…).
    var onStreamEvent: ((NavEvent) -> Void)?

    private(set) var state = PadState()
    /// For menus that use a bumper as a modifier ("R1 + right = jump").
    var r1Held: Bool { state.r1 }
    private var previous = PadState()
    private var link: CADisplayLink?
    private var lastTick = CACurrentMediaTime()
    private var repeatDir: NavEvent?
    private var repeatAt: TimeInterval = 0
    private var controller: GCController?
    let mapper = InputMapper()

    init() {
        let center = NotificationCenter.default
        center.addObserver(forName: .GCControllerDidConnect, object: nil, queue: .main) { [weak self] note in
            let c = note.object as? GCController
            Task { @MainActor in self?.attach(c) }
        }
        center.addObserver(forName: .GCControllerDidDisconnect, object: nil, queue: .main) { [weak self] _ in
            Task { @MainActor in self?.detach() }
        }
        if let c = GCController.controllers().first { attach(c) }
        let link = CADisplayLink(target: self, selector: #selector(tick))
        link.preferredFrameRateRange = CAFrameRateRange(minimum: 60, maximum: 120, preferred: 120)
        link.add(to: .main, forMode: .common)
        self.link = link
    }

    private func attach(_ c: GCController?) {
        guard let c, let pad = c.extendedGamepad else { return }
        controller = c
        connected = true
        name = c.vendorName ?? "Controller"
        pad.valueChangedHandler = { [weak self] gp, _ in
            guard let self else { return }
            var s = PadState()
            s.lx = gp.leftThumbstick.xAxis.value
            s.ly = gp.leftThumbstick.yAxis.value
            s.rx = gp.rightThumbstick.xAxis.value
            s.ry = gp.rightThumbstick.yAxis.value
            s.lt = gp.leftTrigger.value
            s.rt = gp.rightTrigger.value
            s.a = gp.buttonA.isPressed
            s.b = gp.buttonB.isPressed
            s.x = gp.buttonX.isPressed
            s.y = gp.buttonY.isPressed
            s.l1 = gp.leftShoulder.isPressed
            s.r1 = gp.rightShoulder.isPressed
            s.l2 = gp.leftTrigger.isPressed
            s.r2 = gp.rightTrigger.isPressed
            s.l3 = gp.leftThumbstickButton?.isPressed ?? false
            s.r3 = gp.rightThumbstickButton?.isPressed ?? false
            s.menu = gp.buttonMenu.isPressed
            s.options = gp.buttonOptions?.isPressed ?? false
            s.home = gp.buttonHome?.isPressed ?? false
            s.up = gp.dpad.up.isPressed
            s.down = gp.dpad.down.isPressed
            s.left = gp.dpad.left.isPressed
            s.right = gp.dpad.right.isPressed
            Task { @MainActor in self.state = s }
        }
    }

    private func detach() {
        mapper.releaseAll()
        controller = nil
        connected = false
        state = PadState()
        name = ""
    }

    /// 120 times a second: edges, repeats, and the stream mapping.
    @objc private func tick() {
        let now = CACurrentMediaTime()
        let dt = min(0.05, now - lastTick)
        lastTick = now
        let s = state
        let p = previous
        previous = s

        // The way out, whatever the mode.
        let homeNow = s.home || (s.menu && s.options)
        let homeBefore = p.home || (p.menu && p.options)
        if homeNow && !homeBefore {
            if mode == .ui { onNav?(.home) } else { onStreamEvent?(.home) }
            return
        }

        switch mode {
        case .ui: uiTick(s, p, now)
        case .stream: mapper.tick(s, p, dt: dt, events: onStreamEvent)
        }
    }

    private func uiTick(_ s: PadState, _ p: PadState, _ now: TimeInterval) {
        // Direction from the d-pad or the left stick, with key-repeat.
        let dir = direction(s)
        if dir != repeatDir {
            repeatDir = dir
            if let d = dir {
                onNav?(d)
                repeatAt = now + 0.38
            }
        } else if let d = dir, now >= repeatAt {
            onNav?(d)
            repeatAt = now + 0.1
        }
        // Buttons on press.
        let presses: [(Bool, Bool, NavEvent)] = [
            (s.a, p.a, .confirm), (s.b, p.b, .back), (s.x, p.x, .x), (s.y, p.y, .y),
            (s.l1, p.l1, .l1), (s.r1, p.r1, .r1), (s.l2, p.l2, .l2), (s.r2, p.r2, .r2),
            (s.l3, p.l3, .l3), (s.r3, p.r3, .r3), (s.menu, p.menu, .menu), (s.options, p.options, .options),
        ]
        for (now, before, ev) in presses where now && !before {
            onNav?(ev)
        }
    }

    private func direction(_ s: PadState) -> NavEvent? {
        if s.up { return .up }
        if s.down { return .down }
        if s.left { return .left }
        if s.right { return .right }
        let t: Float = 0.55
        if abs(s.ly) > abs(s.lx) {
            if s.ly > t { return .up }
            if s.ly < -t { return .down }
        } else {
            if s.lx > t { return .right }
            if s.lx < -t { return .left }
        }
        return nil
    }
}
