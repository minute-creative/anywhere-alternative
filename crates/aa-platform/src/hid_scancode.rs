//! USB HID keyboard usage (page 0x07) → PC scan code (set 1), the form
//! Windows `SendInput` takes with `KEYEVENTF_SCANCODE`. Pure data, so it is
//! compiled and tested on every OS even though only Windows uses it.
//!
//! Returns `(scancode, extended)`. Extended keys are the ones a PS/2
//! keyboard prefixes with `E0` (arrows, right-hand modifiers, nav cluster,
//! keypad Enter and divide).

#[allow(clippy::too_many_lines)] // a lookup table
pub fn hid_to_scancode(usage: u16) -> Option<(u16, bool)> {
    Some(match usage {
        0x04 => (0x1E, false), // A
        0x05 => (0x30, false), // B
        0x06 => (0x2E, false), // C
        0x07 => (0x20, false), // D
        0x08 => (0x12, false), // E
        0x09 => (0x21, false), // F
        0x0A => (0x22, false), // G
        0x0B => (0x23, false), // H
        0x0C => (0x17, false), // I
        0x0D => (0x24, false), // J
        0x0E => (0x25, false), // K
        0x0F => (0x26, false), // L
        0x10 => (0x32, false), // M
        0x11 => (0x31, false), // N
        0x12 => (0x18, false), // O
        0x13 => (0x19, false), // P
        0x14 => (0x10, false), // Q
        0x15 => (0x13, false), // R
        0x16 => (0x1F, false), // S
        0x17 => (0x14, false), // T
        0x18 => (0x16, false), // U
        0x19 => (0x2F, false), // V
        0x1A => (0x11, false), // W
        0x1B => (0x2D, false), // X
        0x1C => (0x15, false), // Y
        0x1D => (0x2C, false), // Z
        0x1E => (0x02, false), // 1
        0x1F => (0x03, false), // 2
        0x20 => (0x04, false), // 3
        0x21 => (0x05, false), // 4
        0x22 => (0x06, false), // 5
        0x23 => (0x07, false), // 6
        0x24 => (0x08, false), // 7
        0x25 => (0x09, false), // 8
        0x26 => (0x0A, false), // 9
        0x27 => (0x0B, false), // 0
        0x28 => (0x1C, false), // Enter
        0x29 => (0x01, false), // Escape
        0x2A => (0x0E, false), // Backspace
        0x2B => (0x0F, false), // Tab
        0x2C => (0x39, false), // Space
        0x2D => (0x0C, false), // -
        0x2E => (0x0D, false), // =
        0x2F => (0x1A, false), // [
        0x30 => (0x1B, false), // ]
        0x31 => (0x2B, false), // backslash
        0x33 => (0x27, false), // ;
        0x34 => (0x28, false), // '
        0x35 => (0x29, false), // `
        0x36 => (0x33, false), // ,
        0x37 => (0x34, false), // .
        0x38 => (0x35, false), // /
        0x39 => (0x3A, false), // CapsLock
        0x3A => (0x3B, false), // F1
        0x3B => (0x3C, false),
        0x3C => (0x3D, false),
        0x3D => (0x3E, false),
        0x3E => (0x3F, false),
        0x3F => (0x40, false),
        0x40 => (0x41, false),
        0x41 => (0x42, false),
        0x42 => (0x43, false),
        0x43 => (0x44, false), // F10
        0x44 => (0x57, false), // F11
        0x45 => (0x58, false), // F12
        0x46 => (0x37, true),  // PrintScreen
        0x47 => (0x46, false), // ScrollLock
        0x49 => (0x52, true),  // Insert
        0x4A => (0x47, true),  // Home
        0x4B => (0x49, true),  // PageUp
        0x4C => (0x53, true),  // Delete
        0x4D => (0x4F, true),  // End
        0x4E => (0x51, true),  // PageDown
        0x4F => (0x4D, true),  // Right
        0x50 => (0x4B, true),  // Left
        0x51 => (0x50, true),  // Down
        0x52 => (0x48, true),  // Up
        0x53 => (0x45, false), // NumLock
        0x54 => (0x35, true),  // KP /
        0x55 => (0x37, false), // KP *
        0x56 => (0x4A, false), // KP -
        0x57 => (0x4E, false), // KP +
        0x58 => (0x1C, true),  // KP Enter
        0x59 => (0x4F, false), // KP 1
        0x5A => (0x50, false),
        0x5B => (0x51, false),
        0x5C => (0x4B, false),
        0x5D => (0x4C, false),
        0x5E => (0x4D, false),
        0x5F => (0x47, false),
        0x60 => (0x48, false),
        0x61 => (0x49, false), // KP 9
        0x62 => (0x52, false), // KP 0
        0x63 => (0x53, false), // KP .
        0x64 => (0x56, false), // IntlBackslash
        0x65 => (0x5D, true),  // ContextMenu
        0x68 => (0x64, false), // F13
        0x69 => (0x65, false),
        0x6A => (0x66, false),
        0x6B => (0x67, false),
        0x6C => (0x68, false),
        0x6D => (0x69, false),
        0x6E => (0x6A, false),
        0x6F => (0x6B, false), // F20
        0xE0 => (0x1D, false), // LCtrl
        0xE1 => (0x2A, false), // LShift
        0xE2 => (0x38, false), // LAlt
        0xE3 => (0x5B, true),  // LGui
        0xE4 => (0x1D, true),  // RCtrl
        0xE5 => (0x36, false), // RShift
        0xE6 => (0x38, true),  // RAlt
        0xE7 => (0x5C, true),  // RGui
        _ => return None,      // Pause (0x48) needs the E1 sequence; handled by VK on Windows
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn common_keys() {
        assert_eq!(hid_to_scancode(0x04), Some((0x1E, false)));
        assert_eq!(hid_to_scancode(0x2C), Some((0x39, false)));
        assert_eq!(hid_to_scancode(0x4F), Some((0x4D, true)));
        assert_eq!(hid_to_scancode(0xE7), Some((0x5C, true)));
        assert_eq!(hid_to_scancode(0x48), None);
    }

    #[test]
    fn letters_are_unique_and_cover_a_to_z() {
        let mut seen = std::collections::HashSet::new();
        for usage in 0x04..=0x1D {
            let (sc, ext) = hid_to_scancode(usage).expect("letter mapped");
            assert!(!ext);
            assert!(seen.insert(sc), "duplicate scancode {sc:#x}");
        }
    }
}
