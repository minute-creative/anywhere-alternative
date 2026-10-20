// Turns the controller into input for the other computer while watching
// it, following the control map (`Bindings.swift`). Runs 120 times a
// second from `Pad`. The "Controller pass-through" profile skips the map
// and sends the pad itself as a controller report.

import Foundation

@MainActor
final class InputMapper {
    weak var engine: Engine?
    var profile: InputProfile = .desktop
    var map: ControlMap = .desktop
    var pointerSpeed: Double = 1.0
    /// Where the pointer is, across the Mac's picture (0…1).
    private(set) var pointer = CGPoint(x: 0.5, y: 0.5)
    /// When the pointer last moved (the drawn pointer fades when idle).
    private(set) var pointerMovedAt: TimeInterval = 0
    private var fast = true
    private var scrollAcc = CGPoint.zero
    private var heldKeys = Set<UInt16>()
    private var heldMouse = Set<UInt8>()
    private var stickKeys = Set<UInt16>()
    private var lastPadSend: TimeInterval = 0
    /// Keys bound to the d-pad repeat while held (arrows in a text field).
    private var repeatKey: (UInt16, TimeInterval)?

    /// Call when the stream starts and whenever the profile changes.
    func reset() {
        releaseAll()
        scrollAcc = .zero
    }

    /// Let go of everything held (leaving the stream, opening a menu, the
    /// pad disconnecting, the app going to the background).
    func releaseAll() {
        guard let e = engine else { return }
        for k in heldKeys.union(stickKeys) { e.key(k, false) }
        heldKeys.removeAll()
        stickKeys.removeAll()
        for b in heldMouse { e.mouseButton(b, false) }
        heldMouse.removeAll()
        repeatKey = nil
    }

    /// The pointer was moved by touch: keep the stick in step.
    func setPointer(_ p: CGPoint) {
        pointer = CGPoint(x: min(1, max(0, p.x)), y: min(1, max(0, p.y)))
        pointerMovedAt = Date().timeIntervalSinceReferenceDate
    }

    func tick(_ s: PadState, _ p: PadState, dt: Double, events: ((NavEvent) -> Void)?) {
        guard let e = engine else { return }
        if profile == .controller {
            controllerTick(s, p, e)
            return
        }
        // A game on the Mac took the mouse: a pointer stick becomes mouse
        // look, like the desktop viewer's game mouse.
        let captured = e.status.cursor_captured
        stick(captured && map[.leftStick] == .pointer ? .look : map[.leftStick], s.lx, s.ly, dt, e)
        stick(captured && map[.rightStick] == .pointer ? .look : map[.rightStick], s.rx, s.ry, dt, e)
        let buttons: [(PadControl, Bool, Bool)] = [
            (.a, s.a, p.a), (.b, s.b, p.b), (.x, s.x, p.x), (.y, s.y, p.y),
            (.l1, s.l1, p.l1), (.r1, s.r1, p.r1), (.l2, s.l2, p.l2), (.r2, s.r2, p.r2),
            (.l3, s.l3, p.l3), (.r3, s.r3, p.r3), (.l4, s.l4, p.l4), (.r4, s.r4, p.r4),
            (.up, s.up, p.up), (.down, s.down, p.down), (.left, s.left, p.left), (.right, s.right, p.right),
            (.menu, s.menu, p.menu), (.options, s.options, p.options),
        ]
        let now = Date().timeIntervalSinceReferenceDate
        for (control, down, before) in buttons where down != before {
            button(control, map[control], down, now, e, events)
        }
        if let r = repeatKey, now >= r.1 {
            e.tap(r.0)
            repeatKey = (r.0, now + 0.05)
        }
    }

    // MARK: Sticks

    private func stick(_ b: PadAction, _ x: Float, _ y: Float, _ dt: Double, _ e: Engine) {
        switch b {
        case .pointer:
            // Deflection squared feels precise near the centre and quick at
            // the edge. Full deflection crosses the screen in ~1.6 s (fast)
            // or ~4 s (slow).
            let (dx, dy) = curve(x, y)
            if dx != 0 || dy != 0 {
                let speed = (fast ? 0.62 : 0.25) * pointerSpeed * dt
                pointer.x = min(1, max(0, pointer.x + CGFloat(dx) * speed))
                pointer.y = min(1, max(0, pointer.y - CGFloat(dy) * speed))
                pointerMovedAt = Date().timeIntervalSinceReferenceDate
                e.mouseAbs(x: pointer.x, y: pointer.y)
            }
        case .look:
            let (rx, ry) = curve(x, y)
            if rx != 0 || ry != 0 {
                let gain = 1500.0 * pointerSpeed * dt
                e.mouseRel(dx: Int((Double(rx) * gain).rounded()), dy: Int((-Double(ry) * gain).rounded()))
            }
        case .scroll:
            // In 1/120ths of a notch; whole units only.
            let (sx, sy) = curve(x, y)
            scrollAcc.x += CGFloat(sx) * 1100 * dt
            scrollAcc.y += CGFloat(sy) * 1100 * dt
            let ix = Int(scrollAcc.x), iy = Int(scrollAcc.y)
            if ix != 0 || iy != 0 {
                e.scroll(dx: ix, dy: iy)
                scrollAcc.x -= CGFloat(ix)
                scrollAcc.y -= CGFloat(iy)
            }
        case .wasd, .arrows:
            let keys: [UInt16] = b == .wasd
                ? [HID.letter("w")!, HID.letter("s")!, HID.letter("d")!, HID.letter("a")!]
                : [HID.up, HID.down, HID.right, HID.left]
            // Press past 0.45, release under 0.35, so a wobble doesn't chatter.
            holdStick(keys[0], axisOn(y, stickKeys.contains(keys[0])), e)
            holdStick(keys[1], axisOn(-y, stickKeys.contains(keys[1])), e)
            holdStick(keys[2], axisOn(x, stickKeys.contains(keys[2])), e)
            holdStick(keys[3], axisOn(-x, stickKeys.contains(keys[3])), e)
        default:
            break
        }
    }

    // MARK: Buttons

    private func button(_ control: PadControl, _ b: PadAction, _ down: Bool, _ now: TimeInterval, _ e: Engine,
                        _ events: ((NavEvent) -> Void)?) {
        switch b {
        case .key(let u):
            hold(u, down, e)
            // D-pad keys repeat while held, like a keyboard key.
            let dpad = control == .up || control == .down || control == .left || control == .right
            if dpad {
                if down { repeatKey = (u, now + 0.4) } else if repeatKey?.0 == u { repeatKey = nil }
            }
        case .mouse(let btn):
            if down { heldMouse.insert(btn) } else { heldMouse.remove(btn) }
            e.mouseButton(btn, down)
        case .keyboard: if down { events?(.x) }
        case .launcher: if down { events?(.y) }
        case .quickMenu: if down { events?(.home) }
        case .speedToggle: if down { fast.toggle() }
        case .zoom: if down { e.zoom = e.zoom >= 3 ? 1 : e.zoom + 1 }
        default: break
        }
    }

    // MARK: Controller pass-through

    private func controllerTick(_ s: PadState, _ p: PadState, _ e: Engine) {
        let now = Date().timeIntervalSinceReferenceDate
        // Send on change, and every 100 ms regardless (a lost packet must
        // not leave a button stuck on the other side).
        if s == p && now - lastPadSend < 0.1 { return }
        lastPadSend = now
        var buttons: UInt32 = 0
        let bits: [(Bool, UInt32)] = [
            (s.a, 1 << 0), (s.b, 1 << 1), (s.x, 1 << 2), (s.y, 1 << 3), (s.l1, 1 << 4), (s.r1, 1 << 5),
            (s.l3, 1 << 6), (s.r3, 1 << 7), (s.options, 1 << 8), (s.menu, 1 << 9),
            (s.up, 1 << 12), (s.down, 1 << 13), (s.left, 1 << 14), (s.right, 1 << 15),
        ]
        for (on, bit) in bits where on { buttons |= bit }
        func axis(_ v: Float) -> Int16 { Int16(max(-32767, min(32767, v * 32767))) }
        func trig(_ v: Float) -> UInt8 { UInt8(max(0, min(255, v * 255))) }
        e.gamepad(slot: 0, buttons: buttons, lx: axis(s.lx), ly: axis(-s.ly), rx: axis(s.rx), ry: axis(-s.ry),
                  lt: trig(s.lt), rt: trig(s.rt))
    }

    // MARK: Helpers

    private func hold(_ usage: UInt16, _ down: Bool, _ e: Engine) {
        if down && !heldKeys.contains(usage) {
            heldKeys.insert(usage)
            e.key(usage, true)
        } else if !down && heldKeys.contains(usage) {
            heldKeys.remove(usage)
            e.key(usage, false)
        }
    }

    private func holdStick(_ usage: UInt16, _ down: Bool, _ e: Engine) {
        if down && !stickKeys.contains(usage) {
            stickKeys.insert(usage)
            e.key(usage, true)
        } else if !down && stickKeys.contains(usage) {
            stickKeys.remove(usage)
            e.key(usage, false)
        }
    }

    private func axisOn(_ v: Float, _ wasOn: Bool) -> Bool {
        wasOn ? v > 0.35 : v > 0.45
    }

    /// Dead zone, then a squared response.
    private func curve(_ x: Float, _ y: Float) -> (Float, Float) {
        let mag = (x * x + y * y).squareRoot()
        if mag < 0.14 { return (0, 0) }
        let scaled = min(1, (mag - 0.14) / 0.86)
        let gain = scaled * scaled / mag
        return (x * gain, y * gain)
    }
}
