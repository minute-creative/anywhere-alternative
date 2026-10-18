#![allow(clippy::doc_markdown)] // product names read better plain
//! A virtual USB mouse, plugged into the PC through the same USB/IP path as
//! the virtual DualSense (see `usbip.rs`).
//!
//! Why: Riot Vanguard (Valorant's anti-cheat) throws away mouse clicks that
//! programs send with `SendInput`, ours included, while the game runs
//! (owner, 0.5.9: clicks from the Mac worked again as soon as Valorant
//! closed). A USB mouse is hardware to Windows, so its clicks are not
//! "sent by a program". This is used only while Vanguard runs; the owner
//! chose it knowing Riot doesn't support it.
//!
//! What it carries: buttons, relative movement (game mouse, aiming) and the
//! wheel. Pointing at a spot (absolute moves) still uses `SetCursorPos`,
//! which Vanguard lets through; a click then lands where the cursor is.
//!
//! Each change is one report, queued in order, so a quick press and
//! release both reach Windows (a "latest state only" slot would lose the
//! click). The PC polls every millisecond (`bInterval` 1); with nothing
//! queued we don't answer, exactly like a real mouse that isn't moving.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use crate::usbip::{string_descriptor, Doorbell, Setup, UsbDevice};

/// pid.codes, the open-source USB vendor id, with one of its test product
/// ids: honest about not being any real company's mouse.
const VID: u16 = 0x1209;
const PID: u16 = 0x0001;
const EP_IN: u8 = 1;
/// buttons, X (2 bytes), Y (2 bytes), wheel, horizontal wheel.
pub const REPORT_LEN: usize = 7;
/// A backlog this long means nobody is reading: keep the newest.
const MAX_QUEUE: usize = 256;

/// HID report descriptor: 5 buttons, 16-bit relative X/Y, wheel, pan.
#[rustfmt::skip]
pub fn report_descriptor() -> Vec<u8> {
    vec![
        0x05, 0x01,       // Usage Page (Generic Desktop)
        0x09, 0x02,       // Usage (Mouse)
        0xA1, 0x01,       // Collection (Application)
        0x09, 0x01,       //   Usage (Pointer)
        0xA1, 0x00,       //   Collection (Physical)
        0x05, 0x09,       //     Usage Page (Button)
        0x19, 0x01,       //     Usage Minimum (1)
        0x29, 0x05,       //     Usage Maximum (5)
        0x15, 0x00,       //     Logical Minimum (0)
        0x25, 0x01,       //     Logical Maximum (1)
        0x95, 0x05,       //     Report Count (5)
        0x75, 0x01,       //     Report Size (1)
        0x81, 0x02,       //     Input (Data, Variable, Absolute)
        0x95, 0x01,       //     Report Count (1)
        0x75, 0x03,       //     Report Size (3)
        0x81, 0x01,       //     Input (Constant): padding
        0x05, 0x01,       //     Usage Page (Generic Desktop)
        0x09, 0x30,       //     Usage (X)
        0x09, 0x31,       //     Usage (Y)
        0x16, 0x01, 0x80, //     Logical Minimum (-32767)
        0x26, 0xFF, 0x7F, //     Logical Maximum (32767)
        0x75, 0x10,       //     Report Size (16)
        0x95, 0x02,       //     Report Count (2)
        0x81, 0x06,       //     Input (Data, Variable, Relative)
        0x09, 0x38,       //     Usage (Wheel)
        0x15, 0x81,       //     Logical Minimum (-127)
        0x25, 0x7F,       //     Logical Maximum (127)
        0x75, 0x08,       //     Report Size (8)
        0x95, 0x01,       //     Report Count (1)
        0x81, 0x06,       //     Input (Data, Variable, Relative)
        0x05, 0x0C,       //     Usage Page (Consumer)
        0x0A, 0x38, 0x02, //     Usage (AC Pan)
        0x81, 0x06,       //     Input (Data, Variable, Relative)
        0xC0,             //   End Collection
        0xC0,             // End Collection
    ]
}

#[derive(Debug, Default)]
struct State {
    buttons: u8,
    queue: VecDeque<[u8; REPORT_LEN]>,
    /// Wheel movement smaller than one notch, kept for the next event
    /// (a Mac trackpad sends small pixel steps).
    wheel_rest: (i32, i32),
}

/// The virtual mouse. Clone the `Arc` to feed it from the input thread.
#[derive(Debug, Default)]
pub struct VirtualMouse {
    state: Mutex<State>,
    pub doorbell: Arc<Doorbell>,
}

impl VirtualMouse {
    pub fn new(doorbell: Arc<Doorbell>) -> Self {
        Self { state: Mutex::default(), doorbell }
    }

    fn push(&self, s: &mut State, dx: i16, dy: i16, wheel: i8, pan: i8) {
        let [xl, xh] = dx.max(-32767).to_le_bytes();
        let [yl, yh] = dy.max(-32767).to_le_bytes();
        if s.queue.len() >= MAX_QUEUE {
            s.queue.pop_front();
        }
        s.queue.push_back([s.buttons, xl, xh, yl, yh, wheel.to_le_bytes()[0], pan.to_le_bytes()[0]]);
        self.doorbell.ring();
    }

    /// Button `bit` (1 left, 2 right, 4 middle, 8 back, 16 forward).
    pub fn button(&self, bit: u8, pressed: bool) {
        let mut s = self.state.lock().expect("mouse");
        let before = s.buttons;
        if pressed {
            s.buttons |= bit;
        } else {
            s.buttons &= !bit;
        }
        if s.buttons != before {
            self.push(&mut s, 0, 0, 0, 0);
        }
    }

    /// Relative movement in mouse counts.
    pub fn move_by(&self, dx: i16, dy: i16) {
        if dx != 0 || dy != 0 {
            let mut s = self.state.lock().expect("mouse");
            self.push(&mut s, dx, dy, 0, 0);
        }
    }

    /// Wheel in Windows units (120 per notch, positive = away from you /
    /// to the right), turned into whole notches.
    pub fn scroll(&self, dx: i16, dy: i16) {
        let mut s = self.state.lock().expect("mouse");
        s.wheel_rest.0 += i32::from(dy);
        s.wheel_rest.1 += i32::from(dx);
        let (v, h) = (s.wheel_rest.0 / 120, s.wheel_rest.1 / 120);
        s.wheel_rest.0 -= v * 120;
        s.wheel_rest.1 -= h * 120;
        if v != 0 || h != 0 {
            let clamp = |n: i32| i8::try_from(n.clamp(-127, 127)).unwrap_or(0);
            self.push(&mut s, 0, 0, clamp(v), clamp(h));
        }
    }

    /// Let go of every button (the viewer left or its window lost focus).
    pub fn release_all(&self) {
        let mut s = self.state.lock().expect("mouse");
        if s.buttons != 0 {
            s.buttons = 0;
            self.push(&mut s, 0, 0, 0, 0);
        }
    }

    fn hid_descriptor() -> [u8; 9] {
        let rd = u16::try_from(report_descriptor().len()).unwrap_or(0).to_le_bytes();
        [9, 0x21, 0x11, 0x01, 0, 1, 0x22, rd[0], rd[1]]
    }
}

impl UsbDevice for VirtualMouse {
    fn device_descriptor(&self) -> Vec<u8> {
        let [vl, vh] = VID.to_le_bytes();
        let [pl, ph] = PID.to_le_bytes();
        vec![18, 1, 0x00, 0x02, 0, 0, 0, 64, vl, vh, pl, ph, 0x00, 0x01, 1, 2, 0, 1]
    }

    fn config_descriptor(&self) -> Vec<u8> {
        let mut c = vec![9, 2, 0, 0, 1, 1, 0, 0xA0, 50]; // bus powered, remote wakeup, 100 mA
        c.extend_from_slice(&[9, 4, 0, 0, 1, 3, 1, 2, 0]); // HID, boot subclass, mouse
        c.extend_from_slice(&Self::hid_descriptor());
        c.extend_from_slice(&[7, 5, 0x80 | EP_IN, 3, REPORT_LEN as u8, 0, 1]); // interrupt IN, every 1 ms
        let total = u16::try_from(c.len()).unwrap_or(0).to_le_bytes();
        c[2..4].copy_from_slice(&total);
        c
    }

    fn string_descriptor(&self, index: u8) -> Option<Vec<u8>> {
        match index {
            0 => Some(vec![4, 3, 0x09, 0x04]),
            1 => Some(string_descriptor("Anywhere Alternative")),
            2 => Some(string_descriptor("Anywhere Virtual Mouse")),
            _ => None,
        }
    }

    fn control(&self, s: Setup, _data: &[u8]) -> Option<Vec<u8>> {
        let kind = s.value.to_le_bytes()[1];
        match (s.request_type, s.request) {
            (0x81, 6) => match kind {
                0x21 => Some(Self::hid_descriptor().to_vec()),
                0x22 => Some(report_descriptor()),
                _ => None,
            },
            // GET_REPORT: the current buttons, no movement.
            (0xA1, 0x01) => {
                let b = self.state.lock().expect("mouse").buttons;
                Some(vec![b, 0, 0, 0, 0, 0, 0])
            }
            (0xA1, 0x02) => Some(vec![0]),           // GET_IDLE
            (0xA1, 0x03) => Some(vec![1]),           // GET_PROTOCOL: report protocol
            (0x21, 0x0A | 0x0B) => Some(Vec::new()), // SET_IDLE, SET_PROTOCOL
            _ => None,
        }
    }

    fn poll_in(&self, ep: u8, _stale: bool) -> Option<Vec<u8>> {
        // Never repeat an old report: repeating movement would move twice.
        (ep == EP_IN).then(|| self.state.lock().expect("mouse").queue.pop_front().map(|r| r.to_vec())).flatten()
    }

    fn out(&self, _ep: u8, _data: &[u8]) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drain(m: &VirtualMouse) -> Vec<Vec<u8>> {
        std::iter::from_fn(|| m.poll_in(EP_IN, true)).collect()
    }

    #[test]
    fn a_quick_click_is_two_reports() {
        let m = VirtualMouse::default();
        m.button(1, true);
        m.button(1, false);
        let r = drain(&m);
        assert_eq!(r, vec![vec![1, 0, 0, 0, 0, 0, 0], vec![0, 0, 0, 0, 0, 0, 0]]);
        assert!(m.poll_in(EP_IN, true).is_none(), "nothing new: no answer");
    }

    #[test]
    fn movement_keeps_the_held_button() {
        let m = VirtualMouse::default();
        m.button(2, true);
        m.move_by(-3, 500);
        let r = drain(&m);
        assert_eq!(r[1], vec![2, 0xFD, 0xFF, 0xF4, 0x01, 0, 0]);
    }

    #[test]
    fn trackpad_scroll_adds_up_to_notches() {
        let m = VirtualMouse::default();
        for _ in 0..5 {
            m.scroll(0, 30); // 150 in all: one notch, 30 left over
        }
        let r = drain(&m);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0][5], 1);
    }

    #[test]
    fn descriptors_are_consistent() {
        let m = VirtualMouse::default();
        let c = m.config_descriptor();
        assert_eq!(usize::from(u16::from_le_bytes([c[2], c[3]])), c.len());
        let rd = report_descriptor();
        assert_eq!(usize::from(u16::from_le_bytes([c[25], c[26]])), rd.len());
    }
}
