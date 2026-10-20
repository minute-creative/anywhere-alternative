// Turns the controller into input for the other computer while watching
// it, following the chosen profile. Runs 120 times a second from `Pad`.
//
// Desktop profile (using the Mac):
//   left stick   pointer        right stick  scroll
//   A            click          B            right-click
//   R2 (hold)    click & drag   L2           right-click
//   X            keyboard       Y            launcher
//   L1 (hold)    Command ⌘      R1 (hold)    Shift ⇧
//   D-pad        arrow keys     L3           pointer speed slow / fast
//   R3           middle click   Menu ≡       Return      Options ⧉  Escape
//
// Game profile (keyboard + mouse games, e.g. through CrossOver):
//   left stick   W A S D        right stick  mouse look (game mouse)
//   R2 / L2      left / right mouse button
//   A Space      B Left Ctrl    X R          Y E
//   L1 Shift     R1 Q           L3 Shift     R3 F
//   D-pad 1 2 3 4               Menu Esc     Options Tab
//
// Controller profile: the pad itself, sent as a controller report (needs
// a host that can show a virtual controller).

import Foundation

@MainActor
final class InputMapper {
    weak var engine: Engine?
    var profile: InputProfile = .desktop
    var pointerSpeed: Double = 1.0
    /// Where the pointer is, across the Mac's picture (0…1).
    private(set) var pointer = CGPoint(x: 0.5, y: 0.5)
    private var fast = true
    private var scrollAcc = CGPoint.zero
    private var heldKeys = Set<UInt16>()
    private var lastPadSend: TimeInterval = 0
    private var dpadRepeat: (UInt16, TimeInterval)?

    /// Call when the stream starts and whenever the profile changes.
    func reset() {
        releaseAll()
        scrollAcc = .zero
    }

    /// Let go of everything held (leaving the stream, opening a menu).
    func releaseAll() {
        guard let e = engine else { return }
        for k in heldKeys { e.key(k, false) }
        heldKeys.removeAll()
        e.mouseButton(0, false)
        e.mouseButton(1, false)
        dpadRepeat = nil
    }

    /// The pointer was moved by touch: keep the stick in step.
    func setPointer(_ p: CGPoint) {
        pointer = CGPoint(x: min(1, max(0, p.x)), y: min(1, max(0, p.y)))
    }

    func tick(_ s: PadState, _ p: PadState, dt: Double, events: ((NavEvent) -> Void)?) {
        guard let e = engine else { return }
        // Shared: X opens the keyboard, Y the launcher (desktop profile only;
        // games get those from the quick menu).
        switch profile {
        case .desktop: desktopTick(s, p, dt: dt, e, events)
        case .game: gameTick(s, p, dt: dt, e, events)
        case .controller: controllerTick(s, p, e)
        }
    }

    // MARK: Desktop

    private func desktopTick(_ s: PadState, _ p: PadState, dt: Double, _ e: Engine, _ events: ((NavEvent) -> Void)?) {
        if s.x && !p.x { events?(.x) }
        if s.y && !p.y { events?(.y) }
        if s.l3 && !p.l3 { fast.toggle() }

        // Pointer: the stick's deflection squared feels precise near the
        // centre and quick at the edge. Full deflection crosses the screen
        // in about 1.6 s (fast) or 4 s (slow).
        let (dx, dy) = curve(s.lx, s.ly)
        if dx != 0 || dy != 0 {
            let speed = (fast ? 0.62 : 0.25) * pointerSpeed * dt
            pointer.x = min(1, max(0, pointer.x + CGFloat(dx) * speed))
            pointer.y = min(1, max(0, pointer.y - CGFloat(dy) * speed))
            e.mouseAbs(x: pointer.x, y: pointer.y)
        }

        // Scroll: right stick, in 1/120ths of a notch; whole units only.
        let (sx, sy) = curve(s.rx, s.ry)
        scrollAcc.x += CGFloat(sx) * 1100 * dt
        scrollAcc.y += CGFloat(sy) * 1100 * dt
        let ix = Int(scrollAcc.x), iy = Int(scrollAcc.y)
        if ix != 0 || iy != 0 {
            e.scroll(dx: ix, dy: iy)
            scrollAcc.x -= CGFloat(ix)
            scrollAcc.y -= CGFloat(iy)
        }

        // Clicks (A and R2 both left, so a drag can use the trigger).
        let leftNow = s.a || s.r2, leftBefore = p.a || p.r2
        if leftNow != leftBefore { e.mouseButton(0, leftNow) }
        let rightNow = s.b || s.l2, rightBefore = p.b || p.l2
        if rightNow != rightBefore { e.mouseButton(1, rightNow) }
        if s.r3 != p.r3 { e.mouseButton(2, s.r3) }

        // Modifiers held while the bumper is held.
        hold(HID.leftGui, s.l1, e)
        hold(HID.leftShift, s.r1, e)

        // Arrow keys, repeating while held.
        arrows(s, p, e)

        if s.menu && !p.menu { e.tap(HID.enter) }
        if s.options && !p.options { e.tap(HID.escape) }
    }

    private func arrows(_ s: PadState, _ p: PadState, _ e: Engine) {
        let keys: [(Bool, Bool, UInt16)] = [
            (s.up, p.up, HID.up), (s.down, p.down, HID.down), (s.left, p.left, HID.left), (s.right, p.right, HID.right),
        ]
        let now = Date().timeIntervalSinceReferenceDate
        for (down, before, usage) in keys {
            if down && !before {
                e.tap(usage)
                dpadRepeat = (usage, now + 0.4)
            }
            if !down && before, dpadRepeat?.0 == usage {
                dpadRepeat = nil
            }
        }
        if let r = dpadRepeat, now >= r.1 {
            e.tap(r.0)
            dpadRepeat = (r.0, now + 0.05)
        }
    }

    // MARK: Game

    private func gameTick(_ s: PadState, _ p: PadState, dt: Double, _ e: Engine, _ events: ((NavEvent) -> Void)?) {
        // WASD from the left stick (press past 0.45, release under 0.35).
        let w = HID.letter("w")!, a = HID.letter("a")!, sKey = HID.letter("s")!, d = HID.letter("d")!
        hold(w, axisOn(s.ly, heldKeys.contains(w)), e)
        hold(sKey, axisOn(-s.ly, heldKeys.contains(sKey)), e)
        hold(d, axisOn(s.lx, heldKeys.contains(d)), e)
        hold(a, axisOn(-s.lx, heldKeys.contains(a)), e)

        // Mouse look from the right stick: relative pixels per tick.
        let (rx, ry) = curve(s.rx, s.ry)
        if rx != 0 || ry != 0 {
            let gain = 1500.0 * pointerSpeed * dt
            e.mouseRel(dx: Int((Double(rx) * gain).rounded()), dy: Int((-Double(ry) * gain).rounded()))
        }

        if s.r2 != p.r2 { e.mouseButton(0, s.r2) }
        if s.l2 != p.l2 { e.mouseButton(1, s.l2) }
        hold(HID.space, s.a, e)
        hold(HID.leftCtrl, s.b, e)
        hold(HID.letter("r")!, s.x, e)
        hold(HID.letter("e")!, s.y, e)
        hold(HID.leftShift, s.l1 || s.l3, e)
        hold(HID.letter("q")!, s.r1, e)
        hold(HID.letter("f")!, s.r3, e)
        hold(HID.one, s.up, e)
        hold(HID.two, s.right, e)
        hold(HID.three, s.down, e)
        hold(HID.four, s.left, e)
        hold(HID.escape, s.menu, e)
        hold(HID.tab, s.options, e)
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
