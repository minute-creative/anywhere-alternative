//! Map the viewer's physical keys to USB HID usage IDs (keyboard page 0x07).
//!
//! `winit` already reports the *physical* key (the scancode's position), so a
//! Mac keyboard and a PC keyboard produce the same `KeyCode` for the key in
//! the same place. HID usages are the one encoding every OS understands,
//! which is why the host side can inject them without knowing the viewer's
//! layout.

use winit::keyboard::KeyCode;

/// Returns `None` for keys we don't forward (media keys, IME, etc.).
#[allow(clippy::too_many_lines)] // a lookup table; splitting it helps nobody
pub fn hid_usage(key: KeyCode) -> Option<u16> {
    use KeyCode as K;
    Some(match key {
        K::KeyA => 0x04,
        K::KeyB => 0x05,
        K::KeyC => 0x06,
        K::KeyD => 0x07,
        K::KeyE => 0x08,
        K::KeyF => 0x09,
        K::KeyG => 0x0A,
        K::KeyH => 0x0B,
        K::KeyI => 0x0C,
        K::KeyJ => 0x0D,
        K::KeyK => 0x0E,
        K::KeyL => 0x0F,
        K::KeyM => 0x10,
        K::KeyN => 0x11,
        K::KeyO => 0x12,
        K::KeyP => 0x13,
        K::KeyQ => 0x14,
        K::KeyR => 0x15,
        K::KeyS => 0x16,
        K::KeyT => 0x17,
        K::KeyU => 0x18,
        K::KeyV => 0x19,
        K::KeyW => 0x1A,
        K::KeyX => 0x1B,
        K::KeyY => 0x1C,
        K::KeyZ => 0x1D,
        K::Digit1 => 0x1E,
        K::Digit2 => 0x1F,
        K::Digit3 => 0x20,
        K::Digit4 => 0x21,
        K::Digit5 => 0x22,
        K::Digit6 => 0x23,
        K::Digit7 => 0x24,
        K::Digit8 => 0x25,
        K::Digit9 => 0x26,
        K::Digit0 => 0x27,
        K::Enter => 0x28,
        K::Escape => 0x29,
        K::Backspace => 0x2A,
        K::Tab => 0x2B,
        K::Space => 0x2C,
        K::Minus => 0x2D,
        K::Equal => 0x2E,
        K::BracketLeft => 0x2F,
        K::BracketRight => 0x30,
        K::Backslash => 0x31,
        K::Semicolon => 0x33,
        K::Quote => 0x34,
        K::Backquote => 0x35,
        K::Comma => 0x36,
        K::Period => 0x37,
        K::Slash => 0x38,
        K::CapsLock => 0x39,
        K::F1 => 0x3A,
        K::F2 => 0x3B,
        K::F3 => 0x3C,
        K::F4 => 0x3D,
        K::F5 => 0x3E,
        K::F6 => 0x3F,
        K::F7 => 0x40,
        K::F8 => 0x41,
        K::F9 => 0x42,
        K::F10 => 0x43,
        K::F11 => 0x44,
        K::F12 => 0x45,
        K::PrintScreen => 0x46,
        K::ScrollLock => 0x47,
        K::Pause => 0x48,
        K::Insert => 0x49,
        K::Home => 0x4A,
        K::PageUp => 0x4B,
        K::Delete => 0x4C,
        K::End => 0x4D,
        K::PageDown => 0x4E,
        K::ArrowRight => 0x4F,
        K::ArrowLeft => 0x50,
        K::ArrowDown => 0x51,
        K::ArrowUp => 0x52,
        K::NumLock => 0x53,
        K::NumpadDivide => 0x54,
        K::NumpadMultiply => 0x55,
        K::NumpadSubtract => 0x56,
        K::NumpadAdd => 0x57,
        K::NumpadEnter => 0x58,
        K::Numpad1 => 0x59,
        K::Numpad2 => 0x5A,
        K::Numpad3 => 0x5B,
        K::Numpad4 => 0x5C,
        K::Numpad5 => 0x5D,
        K::Numpad6 => 0x5E,
        K::Numpad7 => 0x5F,
        K::Numpad8 => 0x60,
        K::Numpad9 => 0x61,
        K::Numpad0 => 0x62,
        K::NumpadDecimal => 0x63,
        K::IntlBackslash => 0x64,
        K::ContextMenu => 0x65,
        K::F13 => 0x68,
        K::F14 => 0x69,
        K::F15 => 0x6A,
        K::F16 => 0x6B,
        K::F17 => 0x6C,
        K::F18 => 0x6D,
        K::F19 => 0x6E,
        K::F20 => 0x6F,
        K::ControlLeft => 0xE0,
        K::ShiftLeft => 0xE1,
        K::AltLeft => 0xE2,
        K::SuperLeft => 0xE3,
        K::ControlRight => 0xE4,
        K::ShiftRight => 0xE5,
        K::AltRight => 0xE6,
        K::SuperRight => 0xE7,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letters_and_modifiers_map() {
        assert_eq!(hid_usage(KeyCode::KeyA), Some(0x04));
        assert_eq!(hid_usage(KeyCode::Space), Some(0x2C));
        assert_eq!(hid_usage(KeyCode::SuperLeft), Some(0xE3));
        assert_eq!(hid_usage(KeyCode::MediaPlayPause), None);
    }
}
