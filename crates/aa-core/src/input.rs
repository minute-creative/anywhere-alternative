//! Input events travelling viewer → host, and feedback (rumble) host → viewer.
//!
//! Every event is small and fixed-size so the viewer can send it the instant
//! it happens, on its own UDP lane, never queued behind video. Encoding is
//! hand-written rather than serde so a mouse move is exactly 13 bytes.

use bytes::{Buf, BufMut};

/// Mouse buttons. Values match the wire format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum MouseButton {
    Left = 0,
    Right = 1,
    Middle = 2,
    Back = 3,
    Forward = 4,
}

impl MouseButton {
    fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            0 => Self::Left,
            1 => Self::Right,
            2 => Self::Middle,
            3 => Self::Back,
            4 => Self::Forward,
            _ => return None,
        })
    }
}

/// Snapshot of a game controller, sent whenever any value changes.
///
/// Layout follows the `DualSense` physically: two sticks, two analog triggers,
/// a button bitmask, a d-pad. Host backends translate to whatever virtual
/// device they expose (`DualSense` or Xbox); the wire format never changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct GamepadState {
    /// Bitmask; see [`gamepad_buttons`].
    pub buttons: u32,
    pub left_x: i16,
    pub left_y: i16,
    pub right_x: i16,
    pub right_y: i16,
    pub left_trigger: u8,
    pub right_trigger: u8,
}

/// Bit positions for [`GamepadState::buttons`].
pub mod gamepad_buttons {
    pub const CROSS: u32 = 1 << 0; // A on Xbox
    pub const CIRCLE: u32 = 1 << 1; // B
    pub const SQUARE: u32 = 1 << 2; // X
    pub const TRIANGLE: u32 = 1 << 3; // Y
    pub const L1: u32 = 1 << 4;
    pub const R1: u32 = 1 << 5;
    pub const L3: u32 = 1 << 6;
    pub const R3: u32 = 1 << 7;
    pub const SHARE: u32 = 1 << 8; // Create / View
    pub const OPTIONS: u32 = 1 << 9; // Menu
    pub const PS: u32 = 1 << 10; // Guide
    pub const TOUCHPAD: u32 = 1 << 11;
    pub const DPAD_UP: u32 = 1 << 12;
    pub const DPAD_DOWN: u32 = 1 << 13;
    pub const DPAD_LEFT: u32 = 1 << 14;
    pub const DPAD_RIGHT: u32 = 1 << 15;
}

/// What a controller is, so the host can present the matching virtual pad
/// (`PlayStation` button prompts for a `DualSense`, Xbox prompts otherwise).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum GamepadKind {
    Xbox = 0,
    PlayStation = 1,
}

/// An input event from the viewer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputEvent {
    /// Absolute position, normalised to 0..=65535 across the streamed
    /// picture so it is independent of both machines' resolutions and DPI.
    MouseMoveAbs {
        x: u16,
        y: u16,
    },
    /// Relative motion in host pixels, used when a game has captured the
    /// cursor (first-person camera etc.).
    MouseMoveRel {
        dx: i16,
        dy: i16,
    },
    MouseButton {
        button: MouseButton,
        pressed: bool,
    },
    /// Scroll in 1/120ths of a notch (Windows convention; macOS values are
    /// converted on each side).
    MouseScroll {
        dx: i16,
        dy: i16,
    },
    /// USB HID usage ID (page 0x07), so the same physical key means the same
    /// thing from a Mac and a PC keyboard regardless of layout.
    Key {
        hid_usage: u16,
        pressed: bool,
    },
    Gamepad {
        slot: u8,
        state: GamepadState,
    },
    /// A controller was connected on the viewer: plug in a virtual one.
    GamepadAttach {
        slot: u8,
        kind: GamepadKind,
    },
    /// The controller in `slot` was disconnected: unplug the virtual one.
    GamepadDetach {
        slot: u8,
    },
    /// Release every key and mouse button the host believes is held. Sent
    /// when the viewer loses focus or disconnects, so nothing stays stuck.
    ReleaseAll,
}

/// Feedback from host to viewer for a game controller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rumble {
    pub slot: u8,
    pub low_freq: u8,
    pub high_freq: u8,
}

mod tag {
    pub const MOUSE_MOVE_ABS: u8 = 1;
    pub const MOUSE_MOVE_REL: u8 = 2;
    pub const MOUSE_BUTTON: u8 = 3;
    pub const MOUSE_SCROLL: u8 = 4;
    pub const KEY: u8 = 5;
    pub const GAMEPAD: u8 = 6;
    pub const RELEASE_ALL: u8 = 7;
    pub const GAMEPAD_ATTACH: u8 = 8;
    pub const GAMEPAD_DETACH: u8 = 9;
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum InputDecodeError {
    #[error("input event truncated")]
    Truncated,
    #[error("unknown input event tag {0}")]
    UnknownTag(u8),
    #[error("invalid value in input event")]
    InvalidValue,
}

impl InputEvent {
    /// Largest encoded size of any event.
    pub const MAX_ENCODED: usize = 1 + 1 + 4 + 2 * 4 + 2;

    pub fn encode(&self, out: &mut impl BufMut) {
        match *self {
            Self::MouseMoveAbs { x, y } => {
                out.put_u8(tag::MOUSE_MOVE_ABS);
                out.put_u16(x);
                out.put_u16(y);
            }
            Self::MouseMoveRel { dx, dy } => {
                out.put_u8(tag::MOUSE_MOVE_REL);
                out.put_i16(dx);
                out.put_i16(dy);
            }
            Self::MouseButton { button, pressed } => {
                out.put_u8(tag::MOUSE_BUTTON);
                out.put_u8(button as u8);
                out.put_u8(u8::from(pressed));
            }
            Self::MouseScroll { dx, dy } => {
                out.put_u8(tag::MOUSE_SCROLL);
                out.put_i16(dx);
                out.put_i16(dy);
            }
            Self::Key { hid_usage, pressed } => {
                out.put_u8(tag::KEY);
                out.put_u16(hid_usage);
                out.put_u8(u8::from(pressed));
            }
            Self::ReleaseAll => out.put_u8(tag::RELEASE_ALL),
            Self::GamepadAttach { slot, kind } => {
                out.put_u8(tag::GAMEPAD_ATTACH);
                out.put_u8(slot);
                out.put_u8(kind as u8);
            }
            Self::GamepadDetach { slot } => {
                out.put_u8(tag::GAMEPAD_DETACH);
                out.put_u8(slot);
            }
            Self::Gamepad { slot, state } => {
                out.put_u8(tag::GAMEPAD);
                out.put_u8(slot);
                out.put_u32(state.buttons);
                out.put_i16(state.left_x);
                out.put_i16(state.left_y);
                out.put_i16(state.right_x);
                out.put_i16(state.right_y);
                out.put_u8(state.left_trigger);
                out.put_u8(state.right_trigger);
            }
        }
    }

    pub fn decode(buf: &mut impl Buf) -> Result<Self, InputDecodeError> {
        use InputDecodeError::Truncated;
        if buf.remaining() < 1 {
            return Err(Truncated);
        }
        let t = buf.get_u8();
        let need = match t {
            tag::MOUSE_MOVE_ABS | tag::MOUSE_MOVE_REL | tag::MOUSE_SCROLL => 4,
            tag::MOUSE_BUTTON | tag::GAMEPAD_ATTACH => 2,
            tag::KEY => 3,
            tag::GAMEPAD => 15,
            tag::RELEASE_ALL => 0,
            tag::GAMEPAD_DETACH => 1,
            other => return Err(InputDecodeError::UnknownTag(other)),
        };
        if buf.remaining() < need {
            return Err(Truncated);
        }
        Ok(match t {
            tag::MOUSE_MOVE_ABS => Self::MouseMoveAbs { x: buf.get_u16(), y: buf.get_u16() },
            tag::MOUSE_MOVE_REL => Self::MouseMoveRel { dx: buf.get_i16(), dy: buf.get_i16() },
            tag::MOUSE_BUTTON => {
                let button = MouseButton::from_u8(buf.get_u8()).ok_or(InputDecodeError::InvalidValue)?;
                let pressed = buf.get_u8() != 0;
                Self::MouseButton { button, pressed }
            }
            tag::MOUSE_SCROLL => Self::MouseScroll { dx: buf.get_i16(), dy: buf.get_i16() },
            tag::KEY => {
                let hid_usage = buf.get_u16();
                let pressed = buf.get_u8() != 0;
                Self::Key { hid_usage, pressed }
            }
            tag::RELEASE_ALL => Self::ReleaseAll,
            tag::GAMEPAD_ATTACH => {
                let slot = buf.get_u8();
                let kind = match buf.get_u8() {
                    0 => GamepadKind::Xbox,
                    1 => GamepadKind::PlayStation,
                    _ => return Err(InputDecodeError::InvalidValue),
                };
                Self::GamepadAttach { slot, kind }
            }
            tag::GAMEPAD_DETACH => Self::GamepadDetach { slot: buf.get_u8() },
            tag::GAMEPAD => {
                let slot = buf.get_u8();
                let state = GamepadState {
                    buttons: buf.get_u32(),
                    left_x: buf.get_i16(),
                    left_y: buf.get_i16(),
                    right_x: buf.get_i16(),
                    right_y: buf.get_i16(),
                    left_trigger: buf.get_u8(),
                    right_trigger: buf.get_u8(),
                };
                Self::Gamepad { slot, state }
            }
            _ => unreachable!("tag validated above"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::BytesMut;

    fn round_trip(ev: InputEvent) {
        let mut buf = BytesMut::with_capacity(InputEvent::MAX_ENCODED);
        ev.encode(&mut buf);
        assert!(buf.len() <= InputEvent::MAX_ENCODED);
        let mut rd = buf.freeze();
        assert_eq!(InputEvent::decode(&mut rd).unwrap(), ev);
        assert_eq!(rd.remaining(), 0, "decoder must consume exactly one event");
    }

    #[test]
    fn all_events_round_trip() {
        round_trip(InputEvent::MouseMoveAbs { x: 65535, y: 0 });
        round_trip(InputEvent::MouseMoveRel { dx: -7, dy: 300 });
        round_trip(InputEvent::MouseButton { button: MouseButton::Forward, pressed: true });
        round_trip(InputEvent::MouseScroll { dx: 0, dy: -120 });
        round_trip(InputEvent::Key { hid_usage: 0x04, pressed: false });
        round_trip(InputEvent::ReleaseAll);
        round_trip(InputEvent::GamepadAttach { slot: 2, kind: GamepadKind::PlayStation });
        round_trip(InputEvent::GamepadDetach { slot: 3 });
        round_trip(InputEvent::Gamepad {
            slot: 1,
            state: GamepadState {
                buttons: gamepad_buttons::CROSS | gamepad_buttons::DPAD_LEFT,
                left_x: i16::MIN,
                left_y: i16::MAX,
                right_x: 0,
                right_y: -1,
                left_trigger: 255,
                right_trigger: 1,
            },
        });
    }

    #[test]
    fn mouse_move_is_five_bytes() {
        let mut buf = BytesMut::new();
        InputEvent::MouseMoveAbs { x: 1, y: 2 }.encode(&mut buf);
        assert_eq!(buf.len(), 5);
    }

    #[test]
    fn rejects_truncated_and_unknown() {
        let mut empty = &[][..];
        assert_eq!(InputEvent::decode(&mut empty), Err(InputDecodeError::Truncated));
        let mut short = &[tag::KEY, 0x00][..];
        assert_eq!(InputEvent::decode(&mut short), Err(InputDecodeError::Truncated));
        let mut bad = &[200u8][..];
        assert_eq!(InputEvent::decode(&mut bad), Err(InputDecodeError::UnknownTag(200)));
        let mut bad_btn = &[tag::MOUSE_BUTTON, 9, 1][..];
        assert_eq!(InputEvent::decode(&mut bad_btn), Err(InputDecodeError::InvalidValue));
    }
}
