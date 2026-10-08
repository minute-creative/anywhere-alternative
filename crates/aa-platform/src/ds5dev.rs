#![allow(clippy::doc_markdown)] // product names read better plain
//! A virtual DualSense, served over USB/IP (see [`crate::usbip`]).
//!
//! It is a *mirror*, not an emulator: the input reports it hands to the PC
//! are the real controller's (sent from the viewer, byte for byte), and the
//! output reports games write to it (rumble, adaptive triggers, light bar,
//! player LEDs, mic LED) are passed to a callback that sends them back to
//! the real controller. The descriptors match a real DualSense exactly, so
//! Windows, Steam and games treat it as one.
//!
//! Feature reports (motion-sensor calibration, serial/MAC, firmware) also
//! come from the real controller when the viewer has sent them; until then
//! plausible defaults are answered so nothing waits.

use std::collections::HashMap;
use std::sync::Mutex;

use aa_core::ds5::{self, USB_INPUT_LEN, USB_OUTPUT_LEN};

use crate::usbip::{string_descriptor, IsoPacket, Setup, UsbDevice};

/// Interrupt endpoints of the HID interface.
pub const EP_IN: u8 = 4;
pub const EP_OUT: u8 = 3;

/// Feature reports a DualSense declares, with their size (id included).
const FEATURES: &[(u8, usize)] = &[
    (0x05, 41),
    (0x08, 48),
    (0x09, 20),
    (0x0A, 27),
    (0x20, 64),
    (0x21, 5),
    (0x22, 64),
    (0x80, 64),
    (0x81, 64),
    (0x82, 10),
    (0x83, 64),
    (0x84, 64),
    (0x85, 3),
    (0xA0, 2),
    (0xE0, 64),
    (0xF0, 64),
    (0xF1, 64),
    (0xF2, 53),
    (0xF4, 64),
    (0xF5, 4),
    (0x60, 64),
    (0x61, 64),
    (0x62, 64),
    (0x63, 64),
    (0x64, 64),
    (0x65, 64),
    (0x68, 64),
    (0x70, 64),
    (0x71, 64),
    (0x72, 64),
    (0x73, 64),
    (0x74, 64),
    (0x75, 64),
    (0x76, 64),
    (0x77, 64),
    (0x78, 64),
    (0x79, 64),
    (0x7A, 64),
    (0x7B, 64),
];

fn feature_size(id: u8) -> Option<usize> {
    FEATURES.iter().find(|(i, _)| *i == id).map(|(_, n)| *n)
}

/// The DualSense HID report descriptor (what its fields mean).
pub fn report_descriptor() -> Vec<u8> {
    let mut d = vec![
        0x05, 0x01, // Usage Page (Generic Desktop)
        0x09, 0x05, // Usage (Game Pad)
        0xA1, 0x01, // Collection (Application)
        0x85, 0x01, //   Report ID 1: input
        0x09, 0x30, 0x09, 0x31, 0x09, 0x32, 0x09, 0x35, 0x09, 0x33, 0x09, 0x34, // X Y Z Rz Rx Ry
        0x15, 0x00, 0x26, 0xFF, 0x00, // logical 0..255
        0x75, 0x08, 0x95, 0x06, 0x81, 0x02, // 6 x 8 bit
        0x06, 0x00, 0xFF, 0x09, 0x20, 0x95, 0x01, 0x81, 0x02, // vendor byte (sequence)
        0x05, 0x01, 0x09, 0x39, // hat switch
        0x15, 0x00, 0x25, 0x07, 0x35, 0x00, 0x46, 0x3B, 0x01, 0x65, 0x14, // 0..7, 0..315 degrees
        0x75, 0x04, 0x95, 0x01, 0x81, 0x42, 0x65, 0x00, // 4 bit, null state
        0x05, 0x09, 0x19, 0x01, 0x29, 0x0F, // buttons 1..15
        0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x0F, 0x81, 0x02, 0x06, 0x00, 0xFF, 0x09, 0x21, 0x95, 0x0D, 0x81,
        0x02, // 13 vendor bits
        0x06, 0x00, 0xFF, 0x09, 0x22, 0x15, 0x00, 0x26, 0xFF, 0x00, // vendor bytes
        0x75, 0x08, 0x95, 0x34, 0x81, 0x02, // 52 bytes: motion, touch, battery…
        0x85, 0x02, 0x09, 0x23, 0x95, 0x2F, 0x91, 0x02, // Report ID 2: 47-byte output
    ];
    // Feature reports. Usages follow the real controller's numbering.
    let usages: [u8; 39] = [
        0x33, 0x34, 0x24, 0x25, 0x26, 0x27, 0x40, 0x28, 0x29, 0x2A, 0x2B, 0x2C, 0x2D, 0x2E, 0x2F, 0x30, 0x31, 0x32,
        0x35, 0x36, 0x41, 0x42, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49, 0x4A, 0x4B, 0x4C, 0x4D, 0x4E, 0x4F, 0x50,
        0x51, 0x52, 0x53,
    ];
    for ((id, size), usage) in FEATURES.iter().zip(usages) {
        let count = u8::try_from(size - 1).unwrap_or(63);
        d.extend_from_slice(&[0x85, *id, 0x09, usage, 0x95, count, 0xB1, 0x02]);
    }
    d.push(0xC0); // End Collection
    d
}

/// Defaults until the real controller's reports arrive.
pub fn default_feature(id: u8, slot: u8) -> Option<Vec<u8>> {
    let mut r = vec![0u8; feature_size(id)?];
    r[0] = id;
    match id {
        ds5::FEATURE_CALIBRATION => {
            // Gyro bias 0, gyro range ±8192, speed 500, accel range ±8192.
            let v: [i16; 17] =
                [0, 0, 0, 8192, -8192, 8192, -8192, 8192, -8192, 500, 500, 8192, -8192, 8192, -8192, 8192, -8192];
            for (i, x) in v.iter().enumerate() {
                r[1 + i * 2..3 + i * 2].copy_from_slice(&x.to_le_bytes());
            }
            r[35] = 0x0B;
        }
        ds5::FEATURE_PAIRING => {
            // A stable, per-slot, locally-administered MAC (stored reversed),
            // so two virtual controllers never look like the same one.
            let mac = [0x02, 0x41, 0x41, 0x44, 0x35, slot];
            for (i, b) in mac.iter().rev().enumerate() {
                r[1 + i] = *b;
            }
            r[7..16].copy_from_slice(&[0x08, 0x25, 0x00, 0x1E, 0x00, 0xEE, 0x74, 0xD0, 0xBC]);
        }
        ds5::FEATURE_FIRMWARE => {
            r[1..12].copy_from_slice(b"Jun 19 2023");
            r[12..20].copy_from_slice(b"14:47:34");
            r[20] = 0x03; // hardware type
            r[21] = 0x01;
            r[22] = 0x44;
            r[24..28].copy_from_slice(&0x0000_0617_u32.to_le_bytes());
            r[44..46].copy_from_slice(&0x0630_u16.to_le_bytes()); // firmware version
        }
        0x81 => r[1] = 0x01, // command response: "not supported"
        _ => {}
    }
    Some(r)
}

#[derive(Debug)]
struct State {
    input: [u8; USB_INPUT_LEN],
    /// A report arrived since the PC last read one.
    fresh: bool,
    features: HashMap<u8, Vec<u8>>,
}

/// Where output reports (game → controller) go.
pub type OutputSink = Box<dyn Fn([u8; USB_OUTPUT_LEN]) + Send + Sync>;
/// Where the controller's sound goes: 10 ms of 48 kHz 4-channel audio
/// (speaker left, speaker right, haptic left, haptic right), interleaved.
pub type AudioSink = Box<dyn Fn(&[i16]) + Send + Sync>;

/// Isochronous endpoints of the sound card part.
pub const EP_SPEAKER: u8 = 1;
pub const EP_MIC: u8 = 2;
/// Audio format: 48 kHz, 16-bit; 4 channels out, 2 in.
const RATE: u32 = 48_000;
const OUT_CHANNELS: usize = 4;
/// 10 ms of 4-channel audio, in samples.
pub const AUDIO_FRAME: usize = 480 * OUT_CHANNELS;

pub struct VirtualDualSense {
    slot: u8,
    edge: bool,
    state: Mutex<State>,
    on_output: OutputSink,
    /// `Some`: the device is also a sound card (speaker + haptics), like a
    /// real DualSense on USB.
    on_audio: Option<AudioSink>,
    audio: Mutex<Vec<i16>>,
}

impl std::fmt::Debug for VirtualDualSense {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VirtualDualSense").field("slot", &self.slot).field("edge", &self.edge).finish_non_exhaustive()
    }
}

impl VirtualDualSense {
    pub fn new(slot: u8, edge: bool, on_output: OutputSink) -> Self {
        Self {
            slot,
            edge,
            state: Mutex::new(State { input: ds5::neutral_input(), fresh: true, features: HashMap::new() }),
            on_output,
            on_audio: None,
            audio: Mutex::new(Vec::with_capacity(AUDIO_FRAME * 2)),
        }
    }

    /// Also be a sound card (speaker + haptic actuators), like the real one.
    #[must_use]
    pub fn with_audio(mut self, sink: AudioSink) -> Self {
        self.on_audio = Some(sink);
        self
    }

    /// The HID interface's number: 3 behind the three audio interfaces,
    /// as on the real controller; 0 when there is no sound card.
    fn hid_interface(&self) -> u8 {
        if self.on_audio.is_some() {
            3
        } else {
            0
        }
    }

    /// A new input report from the real controller. Returns `false` (and
    /// ignores it) if it is not a USB-layout input report.
    pub fn set_input(&self, report: &[u8; USB_INPUT_LEN]) -> bool {
        if report[0] != 0x01 {
            return false;
        }
        let mut s = self.state.lock().expect("ds5 state");
        s.input = *report;
        s.fresh = true;
        true
    }

    /// A feature report read from the real controller (id first).
    pub fn set_feature(&self, report: &[u8]) {
        let Some((&id, _)) = report.split_first() else { return };
        let Some(size) = feature_size(id) else { return };
        let mut r = report.to_vec();
        r.resize(size, 0);
        self.state.lock().expect("ds5 state").features.insert(id, r);
    }

    fn feature(&self, id: u8) -> Option<Vec<u8>> {
        let s = self.state.lock().expect("ds5 state");
        s.features.get(&id).cloned().or_else(|| default_feature(id, self.slot))
    }

    fn output(&self, data: &[u8]) {
        if let Some(r) = data.get(..USB_OUTPUT_LEN).filter(|r| r[0] == 0x02) {
            (self.on_output)(r.try_into().expect("48 bytes"));
        }
    }

    fn hid_descriptor() -> [u8; 9] {
        let rd = u16::try_from(report_descriptor().len()).unwrap_or(0).to_le_bytes();
        [9, 0x21, 0x11, 0x01, 0, 1, 0x22, rd[0], rd[1]] // HID 1.11, one report descriptor
    }

    /// Audio Class 1.0 interfaces 0-2: control, speaker+haptics stream
    /// (4 channels out), microphone stream (2 channels in).
    #[allow(clippy::too_many_lines)] // one table of descriptor bytes
    fn audio_interfaces() -> Vec<u8> {
        let [r0, r1, r2, _] = RATE.to_le_bytes();
        let speaker_packet = u16::try_from((RATE as usize / 1000 + 1) * OUT_CHANNELS * 2).unwrap_or(0).to_le_bytes();
        let mic_packet = u16::try_from((RATE as usize / 1000 + 1) * 2 * 2).unwrap_or(0).to_le_bytes();
        let mut ac = vec![
            10, 0x24, 0x01, 0x00, 0x01, 0, 0, 2, 1, 2, // AC header 1.00, streams on interfaces 1 and 2
            12, 0x24, 0x02, 1, 0x01, 0x01, 0, 4, 0x33, 0x00, 0,
            0, // input terminal 1: USB stream, 4 ch (L R Ls Rs)
            9, 0x24, 0x03, 2, 0x01, 0x03, 0, 1, 0, // output terminal 2: speaker, from 1
            12, 0x24, 0x02, 3, 0x01, 0x02, 0, 2, 0x03, 0x00, 0, 0, // input terminal 3: microphone, 2 ch
            9, 0x24, 0x03, 4, 0x01, 0x01, 0, 3, 0, // output terminal 4: USB stream, from 3
        ];
        let total = u16::try_from(ac.len()).unwrap_or(0).to_le_bytes();
        ac[5..7].copy_from_slice(&total);
        let mut d = vec![9, 4, 0, 0, 0, 0x01, 0x01, 0, 0]; // interface 0: audio control
        d.extend(ac);
        d.extend_from_slice(&[
            9,
            4,
            1,
            0,
            0,
            0x01,
            0x02,
            0,
            0, // interface 1 alt 0: idle
            9,
            4,
            1,
            1,
            1,
            0x01,
            0x02,
            0,
            0, // interface 1 alt 1: streaming
            7,
            0x24,
            0x01,
            1,
            1,
            0x01,
            0x00, // general: terminal 1, PCM
            11,
            0x24,
            0x02,
            0x01,
            4,
            2,
            16,
            1,
            r0,
            r1,
            r2, // type I: 4 ch, 16 bit, 48 kHz
            9,
            5,
            EP_SPEAKER,
            0x09,
            speaker_packet[0],
            speaker_packet[1],
            1,
            0,
            0, // iso OUT, adaptive
            7,
            0x25,
            0x01,
            0x00,
            0,
            0,
            0, // class endpoint
            9,
            4,
            2,
            0,
            0,
            0x01,
            0x02,
            0,
            0, // interface 2 alt 0: idle
            9,
            4,
            2,
            1,
            1,
            0x01,
            0x02,
            0,
            0, // interface 2 alt 1: streaming
            7,
            0x24,
            0x01,
            4,
            1,
            0x01,
            0x00, // general: terminal 4, PCM
            11,
            0x24,
            0x02,
            0x01,
            2,
            2,
            16,
            1,
            r0,
            r1,
            r2, // type I: 2 ch, 16 bit, 48 kHz
            9,
            5,
            0x80 | EP_MIC,
            0x05,
            mic_packet[0],
            mic_packet[1],
            1,
            0,
            0, // iso IN, async
            7,
            0x25,
            0x01,
            0x00,
            0,
            0,
            0,
        ]);
        d
    }

    /// Audio-class requests: only the sampling rate exists (fixed 48 kHz).
    fn audio_control(s: Setup) -> Option<Vec<u8>> {
        const SET_CUR: u8 = 0x01;
        const GET_CUR: u8 = 0x81;
        match (s.request_type, s.request) {
            (0x22, SET_CUR) => Some(Vec::new()),
            (0xA2, GET_CUR | 0x82 | 0x83) => Some(RATE.to_le_bytes()[..3].to_vec()),
            _ => None,
        }
    }
}

const HID_GET_REPORT: u8 = 0x01;
const HID_GET_IDLE: u8 = 0x02;
const HID_GET_PROTOCOL: u8 = 0x03;
const HID_SET_REPORT: u8 = 0x09;
const HID_SET_IDLE: u8 = 0x0A;
const HID_SET_PROTOCOL: u8 = 0x0B;

impl UsbDevice for VirtualDualSense {
    fn device_descriptor(&self) -> Vec<u8> {
        let pid = if self.edge { ds5::PID_DUALSENSE_EDGE } else { ds5::PID_DUALSENSE };
        let [vl, vh] = ds5::VID_SONY.to_le_bytes();
        let [pl, ph] = pid.to_le_bytes();
        vec![18, 1, 0x00, 0x02, 0, 0, 0, 64, vl, vh, pl, ph, 0x00, 0x01, 1, 2, 0, 1]
    }

    fn config_descriptor(&self) -> Vec<u8> {
        let audio = self.on_audio.is_some();
        let interfaces = if audio { 4 } else { 1 };
        let mut c = vec![9, 2, 0, 0, interfaces, 1, 0, 0xC0, 0xFA]; // self+bus powered, 500 mA
        if audio {
            c.extend(Self::audio_interfaces());
        }
        c.extend_from_slice(&[9, 4, self.hid_interface(), 0, 2, 3, 0, 0, 0]); // HID, 2 endpoints
        c.extend_from_slice(&Self::hid_descriptor());
        c.extend_from_slice(&[
            7,
            5,
            0x80 | EP_IN,
            3,
            64,
            0,
            4, // interrupt IN, 64 bytes, every 4 ms
            7,
            5,
            EP_OUT,
            3,
            64,
            0,
            4, // interrupt OUT
        ]);
        let total = u16::try_from(c.len()).unwrap_or(0).to_le_bytes();
        c[2..4].copy_from_slice(&total);
        c
    }

    fn string_descriptor(&self, index: u8) -> Option<Vec<u8>> {
        match index {
            0 => Some(vec![4, 3, 0x09, 0x04]),
            1 => Some(string_descriptor("Sony Interactive Entertainment")),
            2 if self.edge => Some(string_descriptor("DualSense Edge Wireless Controller")),
            2 => Some(string_descriptor("DualSense Wireless Controller")),
            _ => None,
        }
    }

    fn control(&self, s: Setup, data: &[u8]) -> Option<Vec<u8>> {
        let [id, kind] = s.value.to_le_bytes();
        let to_hid = s.index.to_le_bytes()[0] == self.hid_interface();
        match (s.request_type, s.request) {
            // GET_DESCRIPTOR addressed to the HID interface.
            (0x81, 6) if to_hid => match kind {
                0x21 => Some(Self::hid_descriptor().to_vec()),
                0x22 => Some(report_descriptor()),
                _ => None,
            },
            // SET_INTERFACE: the audio streams switching on and off.
            (0x01, 11) => Some(Vec::new()),
            (0xA1, HID_GET_REPORT) if to_hid => match kind {
                1 => Some(self.state.lock().expect("ds5 state").input.to_vec()),
                3 => self.feature(id),
                _ => None,
            },
            (0xA1, HID_GET_IDLE) if to_hid => Some(vec![0]),
            (0xA1, HID_GET_PROTOCOL) if to_hid => Some(vec![1]),
            (0x21, HID_SET_IDLE | HID_SET_PROTOCOL) if to_hid => Some(Vec::new()),
            (0x21, HID_SET_REPORT) if to_hid => {
                // Feature writes (e.g. 0x80 test commands) are accepted and
                // dropped: they drive factory functions only.
                if kind == 2 {
                    self.output(data);
                }
                Some(Vec::new())
            }
            _ if self.on_audio.is_some() => Self::audio_control(s),
            _ => None,
        }
    }

    fn poll_in(&self, ep: u8, stale: bool) -> Option<Vec<u8>> {
        if ep != EP_IN {
            return None;
        }
        let mut s = self.state.lock().expect("ds5 state");
        if s.fresh || stale {
            s.fresh = false;
            Some(s.input.to_vec())
        } else {
            None
        }
    }

    fn out(&self, ep: u8, data: &[u8]) {
        if ep == EP_OUT {
            self.output(data);
        }
    }

    fn iso_out(&self, ep: u8, data: &[u8], packets: &[IsoPacket]) {
        let Some(sink) = self.on_audio.as_ref().filter(|_| ep == EP_SPEAKER) else { return };
        let mut buf = self.audio.lock().expect("audio");
        for k in packets {
            let (from, len) = (k.offset as usize, k.length as usize);
            let Some(bytes) = data.get(from..from + len) else { continue };
            // Whole 4-channel frames only; a torn frame would swap channels.
            let whole = bytes.len() / (OUT_CHANNELS * 2) * (OUT_CHANNELS * 2);
            buf.extend(bytes[..whole].chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])));
        }
        while buf.len() >= AUDIO_FRAME {
            sink(&buf[..AUDIO_FRAME]);
            buf.drain(..AUDIO_FRAME);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Walk the report descriptor and add up the bits of each report, the
    /// way Windows' HID parser does; sizes must match the real controller.
    fn report_sizes(d: &[u8]) -> HashMap<(u8, u8), usize> {
        let (mut size, mut count, mut id) = (0usize, 0usize, 0u8);
        let mut bits: HashMap<(u8, u8), usize> = HashMap::new();
        let mut i = 0;
        while i < d.len() {
            let prefix = d[i];
            let n = match prefix & 3 {
                3 => 4,
                x => usize::from(x),
            };
            let mut v = 0usize;
            for k in 0..n {
                v |= usize::from(d[i + 1 + k]) << (8 * k);
            }
            match prefix & 0xFC {
                0x74 => size = v,
                0x94 => count = v,
                0x84 => id = u8::try_from(v).unwrap(),
                0x80 => *bits.entry((1, id)).or_default() += size * count,
                0x90 => *bits.entry((2, id)).or_default() += size * count,
                0xB0 => *bits.entry((3, id)).or_default() += size * count,
                _ => {}
            }
            i += 1 + n;
        }
        bits.into_iter().map(|(k, b)| (k, b / 8 + 1)).collect()
    }

    #[test]
    fn descriptor_sizes_match_the_real_controller() {
        let sizes = report_sizes(&report_descriptor());
        assert_eq!(sizes[&(1, 1)], USB_INPUT_LEN);
        assert_eq!(sizes[&(2, 2)], USB_OUTPUT_LEN);
        for (id, n) in FEATURES {
            assert_eq!(sizes[&(3, *id)], *n, "feature {id:#04x}");
        }
    }

    /// Walk a configuration descriptor: every piece's length adds up and
    /// the interface and endpoint layout is what Windows will expect.
    #[test]
    fn composite_configuration_is_well_formed() {
        let d = VirtualDualSense::new(0, false, Box::new(|_| {})).with_audio(Box::new(|_| {}));
        let c = d.config_descriptor();
        assert_eq!(usize::from(u16::from_le_bytes([c[2], c[3]])), c.len());
        let (mut i, mut interfaces, mut endpoints) = (0, Vec::new(), Vec::new());
        while i < c.len() {
            let len = usize::from(c[i]);
            assert!(len >= 2 && i + len <= c.len(), "descriptor at {i} overruns");
            match c[i + 1] {
                4 => interfaces.push((c[i + 2], c[i + 3], c[i + 5])),
                5 => endpoints.push((c[i + 2], c[i + 3] & 3)),
                _ => {}
            }
            i += len;
        }
        assert_eq!(c[4], 4, "four interfaces");
        assert_eq!(interfaces, vec![(0, 0, 1), (1, 0, 1), (1, 1, 1), (2, 0, 1), (2, 1, 1), (3, 0, 3)]);
        assert_eq!(endpoints, vec![(0x01, 1), (0x82, 1), (0x84, 3), (0x03, 3)]);
        // HID requests now go to interface 3; interface 0 is audio.
        let hid = |index| d.control(Setup { request_type: 0x81, request: 6, value: 0x2200, index, length: 1024 }, &[]);
        assert_eq!(hid(3), Some(report_descriptor()));
        assert_eq!(hid(0), None);
        let rate = d.control(Setup { request_type: 0xA2, request: 0x81, value: 0x0100, index: 1, length: 3 }, &[]);
        assert_eq!(rate, Some(vec![0x80, 0xBB, 0x00]));
    }

    #[test]
    fn speaker_audio_is_cut_into_10ms_frames() {
        let frames = std::sync::Arc::new(Mutex::new(Vec::<Vec<i16>>::new()));
        let f = std::sync::Arc::clone(&frames);
        let d = VirtualDualSense::new(0, false, Box::new(|_| {}))
            .with_audio(Box::new(move |pcm| f.lock().unwrap().push(pcm.to_vec())));
        // 25 packets of 1 ms (48 frames x 4 ch x 2 bytes = 384 bytes).
        let mut data = Vec::new();
        for n in 0..25 * 48 * 4 {
            data.extend_from_slice(&i16::try_from(n % 30_000).unwrap().to_le_bytes());
        }
        let packets: Vec<IsoPacket> = (0..25).map(|i| IsoPacket { offset: i * 384, length: 384 }).collect();
        d.iso_out(EP_SPEAKER, &data, &packets);
        let got = frames.lock().unwrap().clone();
        assert_eq!(got.len(), 2, "20 ms delivered, 5 ms waiting");
        assert!(got.iter().all(|f| f.len() == AUDIO_FRAME));
        assert_eq!(got[1][0], 1920, "continuous, nothing dropped");
        // A packet pointing past the buffer is ignored, not a crash.
        d.iso_out(EP_SPEAKER, &data[..10], &[IsoPacket { offset: 4, length: 400 }]);
    }

    #[test]
    fn real_features_replace_defaults_and_outputs_pass_through() {
        let got = std::sync::Arc::new(Mutex::new(Vec::new()));
        let g = std::sync::Arc::clone(&got);
        let d = VirtualDualSense::new(1, false, Box::new(move |r| g.lock().unwrap().push(r)));
        let get = |id: u8| {
            d.control(
                Setup { request_type: 0xA1, request: 1, value: 0x0300 | u16::from(id), index: 0, length: 64 },
                &[],
            )
        };
        assert_eq!(get(0x05).unwrap().len(), 41);
        assert_eq!(get(0x09).unwrap()[1..7], [1, 0x35, 0x44, 0x41, 0x41, 0x02]);
        d.set_feature(&[0x09, 9, 9, 9, 9, 9, 9]);
        assert_eq!(get(0x09).unwrap()[..8], [0x09, 9, 9, 9, 9, 9, 9, 0]);
        assert_eq!(get(0x09).unwrap().len(), 20, "short report padded to size");
        assert!(get(0x42).is_none(), "undeclared feature stalls");

        let mut out = [0u8; USB_OUTPUT_LEN];
        out[0] = 0x02;
        out[3] = 200; // rumble
        d.out(EP_OUT, &out);
        d.control(Setup { request_type: 0x21, request: HID_SET_REPORT, value: 0x0202, index: 0, length: 48 }, &out);
        d.out(EP_OUT, &[0x05; 48]); // not an output report
        assert_eq!(got.lock().unwrap().len(), 2);
    }
}
