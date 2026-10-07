#![allow(clippy::doc_markdown)] // product names read better plain
//! DualSense pass-through: the real controller's raw reports, both ways.
//!
//! Why raw instead of a button/stick snapshot like other pads: the
//! DualSense's best features (adaptive triggers, light bar, player and mic
//! LEDs, motion sensors, touchpad, and the commands games send to drive
//! them) live in bytes a generic snapshot drops. So the viewer forwards the
//! controller's input reports untouched, the host presents an exact
//! virtual DualSense that replays them, and whatever the game writes to the
//! virtual controller comes back and is written to the real one.
//!
//! All reports travel in **USB layout** (the reference form games expect);
//! the viewer converts to and from Bluetooth layout when the controller is
//! wireless.
//!
//! `Kind::Pad` payloads, first byte is the message type, second the slot:
//!
//! ```text
//!  viewer → host  1 input   [1][slot][64-byte USB input report, id 0x01]
//!  viewer → host  2 feature [2][slot][feature report incl. id]   (at attach, repeated)
//!  viewer → host  3 detach  [3][slot]
//!  host → viewer  4 output  [4][slot][48-byte USB output report, id 0x02]
//! ```

/// Sony's USB vendor id and the two DualSense product ids.
pub const VID_SONY: u16 = 0x054C;
pub const PID_DUALSENSE: u16 = 0x0CE6;
pub const PID_DUALSENSE_EDGE: u16 = 0x0DF2;

/// USB input report: id 0x01 + 63 bytes.
pub const USB_INPUT_LEN: usize = 64;
/// USB output report: id 0x02 + 47 bytes of effects.
pub const USB_OUTPUT_LEN: usize = 48;
/// Bluetooth input/output reports (id 0x31) are 78 bytes incl. a CRC32.
pub const BT_REPORT_LEN: usize = 78;
/// Effects block inside an output report.
pub const EFFECTS_LEN: usize = 47;

/// Feature reports the PC asks a DualSense for when it appears: motion
/// calibration, pairing info (serial / MAC), firmware info.
pub const FEATURE_CALIBRATION: u8 = 0x05;
pub const FEATURE_PAIRING: u8 = 0x09;
pub const FEATURE_FIRMWARE: u8 = 0x20;
pub const FEATURES_AT_ATTACH: [u8; 3] = [FEATURE_CALIBRATION, FEATURE_PAIRING, FEATURE_FIRMWARE];

/// Up to four DualSenses, like every console.
pub const SLOTS: u8 = 4;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PadMsg {
    Input { slot: u8, report: [u8; USB_INPUT_LEN] },
    Feature { slot: u8, report: Vec<u8> },
    Detach { slot: u8 },
    Output { slot: u8, report: [u8; USB_OUTPUT_LEN] },
}

impl PadMsg {
    pub fn encode(&self) -> Vec<u8> {
        match self {
            Self::Input { slot, report } => [&[1, *slot][..], &report[..]].concat(),
            Self::Feature { slot, report } => [&[2, *slot][..], &report[..]].concat(),
            Self::Detach { slot } => vec![3, *slot],
            Self::Output { slot, report } => [&[4, *slot][..], &report[..]].concat(),
        }
    }

    /// `None` for anything malformed, unknown, or for a slot out of range.
    pub fn decode(b: &[u8]) -> Option<Self> {
        let (&ty, rest) = b.split_first()?;
        let (&slot, body) = rest.split_first()?;
        if slot >= SLOTS {
            return None;
        }
        Some(match ty {
            1 => Self::Input { slot, report: body.get(..USB_INPUT_LEN)?.try_into().ok()? },
            2 if (2..=64).contains(&body.len()) => Self::Feature { slot, report: body.to_vec() },
            3 => Self::Detach { slot },
            4 => Self::Output { slot, report: body.get(..USB_OUTPUT_LEN)?.try_into().ok()? },
            _ => return None,
        })
    }
}

/// CRC-32 (IEEE, reflected), as the DualSense uses on Bluetooth.
pub fn crc32(seed_byte: u8, data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in std::iter::once(&seed_byte).chain(data) {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

/// A Bluetooth input report (id 0x31) in USB layout, if it is genuine.
/// Bluetooth adds a sequence byte after the id and a CRC at the end; the
/// controller state in between is identical to USB.
pub fn bt_input_to_usb(bt: &[u8]) -> Option<[u8; USB_INPUT_LEN]> {
    if bt.len() < BT_REPORT_LEN || bt[0] != 0x31 {
        return None;
    }
    let (body, crc) = bt[..BT_REPORT_LEN].split_at(BT_REPORT_LEN - 4);
    if crc32(0xA1, body) != u32::from_le_bytes(crc.try_into().ok()?) {
        return None; // radio corruption: drop rather than replay garbage
    }
    let mut usb = [0u8; USB_INPUT_LEN];
    usb[0] = 0x01;
    usb[1..].copy_from_slice(&bt[2..2 + USB_INPUT_LEN - 1]);
    Some(usb)
}

/// A USB-layout output report (id 0x02) for a controller connected by
/// Bluetooth: id 0x31, sequence tag, the 0x10 marker, effects, CRC.
pub fn usb_output_to_bt(usb: &[u8; USB_OUTPUT_LEN], seq: u8) -> [u8; BT_REPORT_LEN] {
    let mut bt = [0u8; BT_REPORT_LEN];
    bt[0] = 0x31;
    bt[1] = (seq & 0x0F) << 4;
    bt[2] = 0x10;
    bt[3..3 + EFFECTS_LEN].copy_from_slice(&usb[1..]);
    let crc = crc32(0xA2, &bt[..BT_REPORT_LEN - 4]);
    bt[BT_REPORT_LEN - 4..].copy_from_slice(&crc.to_le_bytes());
    bt
}

/// The neutral input report (sticks centred, nothing pressed, d-pad
/// released), sent by the virtual controller before the first real one.
pub fn neutral_input() -> [u8; USB_INPUT_LEN] {
    let mut r = [0u8; USB_INPUT_LEN];
    r[0] = 0x01;
    r[1..5].fill(0x80); // sticks
    r[8] = 0x08; // d-pad: none
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_matches_the_standard_check_value() {
        // CRC-32/IEEE of "123456789" is 0xCBF43926; feed it with the first
        // byte as the "seed" to use the same code path.
        assert_eq!(crc32(b'1', b"23456789"), 0xCBF4_3926);
    }

    #[test]
    fn messages_round_trip_and_reject_junk() {
        let mut input = neutral_input();
        input[10] = 0x55;
        for m in [
            PadMsg::Input { slot: 3, report: input },
            PadMsg::Feature { slot: 0, report: vec![0x05, 1, 2, 3] },
            PadMsg::Detach { slot: 1 },
            PadMsg::Output { slot: 2, report: [7; USB_OUTPUT_LEN] },
        ] {
            assert_eq!(PadMsg::decode(&m.encode()), Some(m));
        }
        assert_eq!(PadMsg::decode(&[1, 4]), None, "slot out of range");
        assert_eq!(PadMsg::decode(&[1, 0, 1, 2]), None, "short report");
        assert_eq!(PadMsg::decode(&[9, 0]), None, "unknown type");
        assert_eq!(PadMsg::decode(&[]), None);
    }

    #[test]
    fn bluetooth_and_usb_layouts_convert() {
        // Build a BT report the way the controller does.
        let mut bt = [0u8; BT_REPORT_LEN];
        bt[0] = 0x31;
        bt[1] = 0x42; // sequence
        for (i, b) in bt[2..BT_REPORT_LEN - 4].iter_mut().enumerate() {
            *b = i as u8;
        }
        let crc = crc32(0xA1, &bt[..BT_REPORT_LEN - 4]);
        bt[BT_REPORT_LEN - 4..].copy_from_slice(&crc.to_le_bytes());
        let usb = bt_input_to_usb(&bt).expect("valid");
        assert_eq!(usb[0], 0x01);
        assert_eq!(&usb[1..], &bt[2..65]);
        // One flipped bit: rejected.
        bt[20] ^= 1;
        assert!(bt_input_to_usb(&bt).is_none());

        let mut out = [0u8; USB_OUTPUT_LEN];
        out[0] = 0x02;
        out[1] = 0x0F; // enable flags
        out[11..22].copy_from_slice(&[0x26, 0x90, 0xA0, 0xFF, 0, 0, 0, 0, 0, 0, 0]); // a trigger effect
        let o = usb_output_to_bt(&out, 3);
        assert_eq!((o[0], o[1], o[2]), (0x31, 0x30, 0x10));
        assert_eq!(&o[3..3 + EFFECTS_LEN], &out[1..]);
        let crc = u32::from_le_bytes(o[BT_REPORT_LEN - 4..].try_into().unwrap());
        assert_eq!(crc, crc32(0xA2, &o[..BT_REPORT_LEN - 4]));
    }
}
