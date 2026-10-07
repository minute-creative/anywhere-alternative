#![allow(clippy::doc_markdown)] // product names (DualSense, PlayStation) read better plain
//! Translating our controller snapshot into what Windows games read.
//!
//! The wire carries one neutral layout ([`GamepadState`]). On the PC the
//! virtual controller is either an Xbox 360 pad (what nearly every PC game
//! expects) or a DualShock 4 (so games show PlayStation button icons when
//! you are holding a DualSense). Both conversions live here, free of driver
//! code, so they are tested on every OS.

use aa_core::input::{gamepad_buttons as b, GamepadState};

/// An Xbox 360 report: XInput's `XINPUT_GAMEPAD` layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct XInputPad {
    pub buttons: u16,
    pub left_trigger: u8,
    pub right_trigger: u8,
    /// Sticks: -32768..=32767, positive = right / up.
    pub thumb_lx: i16,
    pub thumb_ly: i16,
    pub thumb_rx: i16,
    pub thumb_ry: i16,
}

/// XInput button bits.
pub mod xbtn {
    pub const UP: u16 = 0x0001;
    pub const DOWN: u16 = 0x0002;
    pub const LEFT: u16 = 0x0004;
    pub const RIGHT: u16 = 0x0008;
    pub const START: u16 = 0x0010;
    pub const BACK: u16 = 0x0020;
    pub const LTHUMB: u16 = 0x0040;
    pub const RTHUMB: u16 = 0x0080;
    pub const LB: u16 = 0x0100;
    pub const RB: u16 = 0x0200;
    pub const GUIDE: u16 = 0x0400;
    pub const A: u16 = 0x1000;
    pub const B: u16 = 0x2000;
    pub const X: u16 = 0x4000;
    pub const Y: u16 = 0x8000;
}

pub fn to_xinput(s: &GamepadState) -> XInputPad {
    let map = [
        (b::DPAD_UP, xbtn::UP),
        (b::DPAD_DOWN, xbtn::DOWN),
        (b::DPAD_LEFT, xbtn::LEFT),
        (b::DPAD_RIGHT, xbtn::RIGHT),
        (b::OPTIONS, xbtn::START),
        (b::SHARE, xbtn::BACK),
        (b::L3, xbtn::LTHUMB),
        (b::R3, xbtn::RTHUMB),
        (b::L1, xbtn::LB),
        (b::R1, xbtn::RB),
        (b::PS, xbtn::GUIDE),
        (b::CROSS, xbtn::A),
        (b::CIRCLE, xbtn::B),
        (b::SQUARE, xbtn::X),
        (b::TRIANGLE, xbtn::Y),
        // Xbox has no touchpad; the View button is the usual stand-in.
        (b::TOUCHPAD, xbtn::BACK),
    ];
    let buttons = map.iter().filter(|(ours, _)| s.buttons & ours != 0).fold(0, |acc, (_, x)| acc | x);
    XInputPad {
        buttons,
        left_trigger: s.left_trigger,
        right_trigger: s.right_trigger,
        thumb_lx: s.left_x,
        thumb_ly: s.left_y,
        thumb_rx: s.right_x,
        thumb_ry: s.right_y,
    }
}

/// A DualShock 4 input report as `ViGEm` takes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ds4Pad {
    /// Sticks: 0..=255, 128 = centre, and y grows *downwards*.
    pub thumb_lx: u8,
    pub thumb_ly: u8,
    pub thumb_rx: u8,
    pub thumb_ry: u8,
    /// Low 4 bits: d-pad direction (0 = up, clockwise, 8 = none); then buttons.
    pub buttons: u16,
    /// Bit 0: PS, bit 1: touchpad click.
    pub special: u8,
    pub trigger_l: u8,
    pub trigger_r: u8,
}

/// DualShock 4 button bits (above the d-pad nibble).
pub mod ds4btn {
    pub const SQUARE: u16 = 1 << 4;
    pub const CROSS: u16 = 1 << 5;
    pub const CIRCLE: u16 = 1 << 6;
    pub const TRIANGLE: u16 = 1 << 7;
    pub const L1: u16 = 1 << 8;
    pub const R1: u16 = 1 << 9;
    pub const L2: u16 = 1 << 10;
    pub const R2: u16 = 1 << 11;
    pub const SHARE: u16 = 1 << 12;
    pub const OPTIONS: u16 = 1 << 13;
    pub const L3: u16 = 1 << 14;
    pub const R3: u16 = 1 << 15;
    pub const DPAD_NONE: u16 = 8;
}

/// i16 stick (positive up) → DS4 byte (positive down).
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn stick(v: i16, invert: bool) -> u8 {
    let v = if invert { -(i32::from(v)) - 1 } else { i32::from(v) };
    ((v + 32768) >> 8).clamp(0, 255) as u8
}

/// D-pad bits → hat direction 0..=7, or 8 for none.
fn hat(buttons: u32) -> u16 {
    let (u, d, l, r) = (
        buttons & b::DPAD_UP != 0,
        buttons & b::DPAD_DOWN != 0,
        buttons & b::DPAD_LEFT != 0,
        buttons & b::DPAD_RIGHT != 0,
    );
    match (u && !d, d && !u, l && !r, r && !l) {
        (true, _, false, false) => 0,
        (true, _, false, true) => 1,
        (false, false, false, true) => 2,
        (_, true, false, true) => 3,
        (_, true, false, false) => 4,
        (_, true, true, false) => 5,
        (false, false, true, false) => 6,
        (true, _, true, false) => 7,
        _ => ds4btn::DPAD_NONE,
    }
}

/// Trigger travel past which the digital L2/R2 bit is set, like a real pad.
const TRIGGER_CLICK: u8 = 30;

pub fn to_ds4(s: &GamepadState) -> Ds4Pad {
    let map = [
        (b::SQUARE, ds4btn::SQUARE),
        (b::CROSS, ds4btn::CROSS),
        (b::CIRCLE, ds4btn::CIRCLE),
        (b::TRIANGLE, ds4btn::TRIANGLE),
        (b::L1, ds4btn::L1),
        (b::R1, ds4btn::R1),
        (b::SHARE, ds4btn::SHARE),
        (b::OPTIONS, ds4btn::OPTIONS),
        (b::L3, ds4btn::L3),
        (b::R3, ds4btn::R3),
    ];
    let mut buttons = map.iter().filter(|(ours, _)| s.buttons & ours != 0).fold(hat(s.buttons), |acc, (_, d)| acc | d);
    if s.left_trigger > TRIGGER_CLICK {
        buttons |= ds4btn::L2;
    }
    if s.right_trigger > TRIGGER_CLICK {
        buttons |= ds4btn::R2;
    }
    let special = u8::from(s.buttons & b::PS != 0) | (u8::from(s.buttons & b::TOUCHPAD != 0) << 1);
    Ds4Pad {
        thumb_lx: stick(s.left_x, false),
        thumb_ly: stick(s.left_y, true),
        thumb_rx: stick(s.right_x, false),
        thumb_ry: stick(s.right_y, true),
        buttons,
        special,
        trigger_l: s.left_trigger,
        trigger_r: s.right_trigger,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_pad_is_centred_on_both() {
        let s = GamepadState::default();
        assert_eq!(to_xinput(&s), XInputPad::default());
        let d = to_ds4(&s);
        assert_eq!((d.thumb_lx, d.thumb_ly, d.thumb_rx, d.thumb_ry), (128, 127, 128, 127));
        assert_eq!(d.buttons, ds4btn::DPAD_NONE);
        assert_eq!(d.special, 0);
    }

    #[test]
    fn face_buttons_land_on_the_same_physical_position() {
        let s = GamepadState { buttons: b::CROSS | b::TRIANGLE, ..Default::default() };
        assert_eq!(to_xinput(&s).buttons, xbtn::A | xbtn::Y);
        let d = to_ds4(&s);
        assert_eq!(d.buttons & !0xF, ds4btn::CROSS | ds4btn::TRIANGLE);
    }

    #[test]
    fn stick_up_is_up_on_both() {
        let s = GamepadState { left_y: i16::MAX, right_x: i16::MIN, ..Default::default() };
        assert_eq!(to_xinput(&s).thumb_ly, i16::MAX);
        let d = to_ds4(&s);
        assert_eq!(d.thumb_ly, 0, "DS4 y=0 is fully up");
        assert_eq!(d.thumb_rx, 0, "DS4 x=0 is fully left");
    }

    #[test]
    fn dpad_diagonals_and_conflicts() {
        assert_eq!(hat(b::DPAD_UP | b::DPAD_RIGHT), 1);
        assert_eq!(hat(b::DPAD_DOWN | b::DPAD_LEFT), 5);
        assert_eq!(hat(b::DPAD_LEFT), 6);
        assert_eq!(hat(b::DPAD_LEFT | b::DPAD_RIGHT), ds4btn::DPAD_NONE);
    }

    #[test]
    fn triggers_set_the_digital_bit_when_pulled() {
        let d = to_ds4(&GamepadState { left_trigger: 200, right_trigger: 10, ..Default::default() });
        assert_ne!(d.buttons & ds4btn::L2, 0);
        assert_eq!(d.buttons & ds4btn::R2, 0);
        assert_eq!((d.trigger_l, d.trigger_r), (200, 10));
    }

    #[test]
    fn ps_and_touchpad_are_special() {
        let d = to_ds4(&GamepadState { buttons: b::PS | b::TOUCHPAD, ..Default::default() });
        assert_eq!(d.special, 0b11);
        assert_eq!(to_xinput(&GamepadState { buttons: b::PS, ..Default::default() }).buttons, xbtn::GUIDE);
    }
}
