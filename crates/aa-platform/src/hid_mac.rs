//! USB HID keyboard usage (page 0x07) → macOS virtual key code (`kVK_*`
//! from Carbon's `Events.h`), the form `CGEventCreateKeyboardEvent` takes.
//! Pure data, so it is compiled and tested on every OS even though only
//! the Mac host uses it.
//!
//! Mac key codes are positions on Apple's original ANSI keyboard, not
//! letters, which is why the table looks scrambled (A is 0, S is 1…).

/// Modifier flags macOS expects on events while a modifier is held
/// (`kCGEventFlagMask*`).
pub const FLAG_SHIFT: u64 = 0x0002_0000;
pub const FLAG_CONTROL: u64 = 0x0004_0000;
pub const FLAG_OPTION: u64 = 0x0008_0000;
pub const FLAG_COMMAND: u64 = 0x0010_0000;

/// The flag a modifier key sets, if it is one.
pub fn modifier_flag(usage: u16) -> Option<u64> {
    Some(match usage {
        0xE0 | 0xE4 => FLAG_CONTROL,
        0xE1 | 0xE5 => FLAG_SHIFT,
        0xE2 | 0xE6 => FLAG_OPTION,
        0xE3 | 0xE7 => FLAG_COMMAND,
        _ => return None,
    })
}

#[allow(clippy::too_many_lines, clippy::match_same_arms)] // a lookup table
pub fn hid_to_mac_keycode(usage: u16) -> Option<u16> {
    Some(match usage {
        0x04 => 0x00, // A
        0x05 => 0x0B, // B
        0x06 => 0x08, // C
        0x07 => 0x02, // D
        0x08 => 0x0E, // E
        0x09 => 0x03, // F
        0x0A => 0x05, // G
        0x0B => 0x04, // H
        0x0C => 0x22, // I
        0x0D => 0x26, // J
        0x0E => 0x28, // K
        0x0F => 0x25, // L
        0x10 => 0x2E, // M
        0x11 => 0x2D, // N
        0x12 => 0x1F, // O
        0x13 => 0x23, // P
        0x14 => 0x0C, // Q
        0x15 => 0x0F, // R
        0x16 => 0x01, // S
        0x17 => 0x11, // T
        0x18 => 0x20, // U
        0x19 => 0x09, // V
        0x1A => 0x0D, // W
        0x1B => 0x07, // X
        0x1C => 0x10, // Y
        0x1D => 0x06, // Z
        0x1E => 0x12, // 1
        0x1F => 0x13, // 2
        0x20 => 0x14, // 3
        0x21 => 0x15, // 4
        0x22 => 0x17, // 5
        0x23 => 0x16, // 6
        0x24 => 0x1A, // 7
        0x25 => 0x1C, // 8
        0x26 => 0x19, // 9
        0x27 => 0x1D, // 0
        0x28 => 0x24, // Return
        0x29 => 0x35, // Escape
        0x2A => 0x33, // Backspace ("Delete" on a Mac)
        0x2B => 0x30, // Tab
        0x2C => 0x31, // Space
        0x2D => 0x1B, // -
        0x2E => 0x18, // =
        0x2F => 0x21, // [
        0x30 => 0x1E, // ]
        0x31 => 0x2A, // backslash
        0x32 => 0x2A, // non-US # (same key position)
        0x33 => 0x29, // ;
        0x34 => 0x27, // '
        0x35 => 0x32, // `
        0x36 => 0x2B, // ,
        0x37 => 0x2F, // .
        0x38 => 0x2C, // /
        0x39 => 0x39, // Caps Lock
        0x3A => 0x7A, // F1
        0x3B => 0x78, // F2
        0x3C => 0x63, // F3
        0x3D => 0x76, // F4
        0x3E => 0x60, // F5
        0x3F => 0x61, // F6
        0x40 => 0x62, // F7
        0x41 => 0x64, // F8
        0x42 => 0x65, // F9
        0x43 => 0x6D, // F10
        0x44 => 0x67, // F11
        0x45 => 0x6F, // F12
        0x46 => 0x69, // Print Screen → F13 (where Mac keyboards have it)
        0x47 => 0x6B, // Scroll Lock → F14
        0x48 => 0x71, // Pause → F15
        0x49 => 0x72, // Insert → Help
        0x4A => 0x73, // Home
        0x4B => 0x74, // Page Up
        0x4C => 0x75, // Delete (forward)
        0x4D => 0x77, // End
        0x4E => 0x79, // Page Down
        0x4F => 0x7C, // Right
        0x50 => 0x7B, // Left
        0x51 => 0x7D, // Down
        0x52 => 0x7E, // Up
        0x53 => 0x47, // Num Lock → keypad Clear
        0x54 => 0x4B, // keypad /
        0x55 => 0x43, // keypad *
        0x56 => 0x4E, // keypad -
        0x57 => 0x45, // keypad +
        0x58 => 0x4C, // keypad Enter
        0x59 => 0x53, // keypad 1
        0x5A => 0x54, // keypad 2
        0x5B => 0x55, // keypad 3
        0x5C => 0x56, // keypad 4
        0x5D => 0x57, // keypad 5
        0x5E => 0x58, // keypad 6
        0x5F => 0x59, // keypad 7
        0x60 => 0x5B, // keypad 8
        0x61 => 0x5C, // keypad 9
        0x62 => 0x52, // keypad 0
        0x63 => 0x41, // keypad .
        0x64 => 0x0A, // ISO key left of Z
        0x67 => 0x51, // keypad =
        0x68 => 0x69, // F13
        0x69 => 0x6B, // F14
        0x6A => 0x71, // F15
        0x6B => 0x6A, // F16
        0x6C => 0x40, // F17
        0x6D => 0x4F, // F18
        0x6E => 0x50, // F19
        0x6F => 0x5A, // F20
        0x7F => 0x4A, // Mute
        0x80 => 0x48, // Volume Up
        0x81 => 0x49, // Volume Down
        0xE0 => 0x3B, // Left Control
        0xE1 => 0x38, // Left Shift
        0xE2 => 0x3A, // Left Option (Alt)
        0xE3 => 0x37, // Left Command (Windows key)
        0xE4 => 0x3E, // Right Control
        0xE5 => 0x3C, // Right Shift
        0xE6 => 0x3D, // Right Option
        0xE7 => 0x36, // Right Command
        _ => return None,
    })
}

/// macOS doesn't count clicks for injected events: a double-click only
/// opens a file if the second press says "click 2". This counts them the
/// way the system does: same button, quickly, without moving far.
#[derive(Debug, Default)]
pub struct ClickCounter {
    last: Option<(u8, std::time::Instant, f64, f64)>,
    count: i64,
}

impl ClickCounter {
    /// The macOS default double-click interval.
    const WINDOW: std::time::Duration = std::time::Duration::from_millis(500);
    /// How far (in points) the pointer may drift between clicks.
    const SLOP: f64 = 4.0;

    /// Call on each button press; returns the click count to report.
    pub fn press(&mut self, button: u8, x: f64, y: f64) -> i64 {
        let now = std::time::Instant::now();
        let continues = self.last.is_some_and(|(b, t, lx, ly)| {
            b == button
                && now.duration_since(t) <= Self::WINDOW
                && (x - lx).abs() <= Self::SLOP
                && (y - ly).abs() <= Self::SLOP
        });
        self.count = if continues { self.count + 1 } else { 1 };
        self.last = Some((button, now, x, y));
        self.count
    }

    /// The count to report on the matching release.
    pub fn current(&self) -> i64 {
        self.count.max(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clicks_are_counted_like_macos() {
        let mut c = ClickCounter::default();
        assert_eq!(c.press(0, 10.0, 10.0), 1);
        assert_eq!(c.press(0, 11.0, 10.0), 2, "quick second click nearby");
        assert_eq!(c.press(0, 11.0, 10.0), 3, "triple-click selects a line");
        assert_eq!(c.press(1, 11.0, 10.0), 1, "another button starts over");
        assert_eq!(c.press(1, 40.0, 10.0), 1, "moved away starts over");
        assert_eq!(c.current(), 1);
    }

    #[test]
    fn letters_and_digits_map_to_distinct_keys() {
        let mut seen = std::collections::HashSet::new();
        for usage in 0x04..=0x27 {
            let k = hid_to_mac_keycode(usage).expect("mapped");
            assert!(seen.insert(k), "usage {usage:#x} collides on key code {k:#x}");
        }
    }

    #[test]
    fn well_known_keys() {
        assert_eq!(hid_to_mac_keycode(0x04), Some(0x00)); // A
        assert_eq!(hid_to_mac_keycode(0x06), Some(0x08)); // C
        assert_eq!(hid_to_mac_keycode(0x19), Some(0x09)); // V
        assert_eq!(hid_to_mac_keycode(0x28), Some(0x24)); // Return
        assert_eq!(hid_to_mac_keycode(0xE3), Some(0x37)); // Command
        assert_eq!(hid_to_mac_keycode(0x52), Some(0x7E)); // Up
        assert_eq!(hid_to_mac_keycode(0x00), None);
    }

    #[test]
    fn every_modifier_has_a_key_and_a_flag() {
        for usage in 0xE0..=0xE7 {
            assert!(hid_to_mac_keycode(usage).is_some());
            assert!(modifier_flag(usage).is_some());
        }
        assert_eq!(modifier_flag(0x04), None);
    }
}
