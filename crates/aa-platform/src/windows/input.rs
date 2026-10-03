//! `SendInput`-based injection.
//!
//! Mouse positions arrive normalised to the streamed output (0..=65535
//! across the captured display). Windows' absolute mouse coordinates are
//! normalised across the *virtual desktop* (all monitors) when
//! `MOUSEEVENTF_VIRTUALDESK` is set, so we map through the output's
//! desktop rectangle. Keys are sent as scan codes so games that read raw
//! input see the right physical key regardless of layout.

// FFI code: `unsafe` is the point here, each block carries a SAFETY note;
// the pedantic cast/pointer lints add noise, not safety.
#![allow(unsafe_code, clippy::pedantic)]

use aa_core::input::{InputEvent, MouseButton};
use windows::Win32::Foundation::RECT;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYBD_EVENT_FLAGS, KEYEVENTF_EXTENDEDKEY,
    KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN,
    MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN,
    MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEINPUT,
    MOUSE_EVENT_FLAGS, VIRTUAL_KEY, VK_PAUSE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
};

use crate::hid_scancode::hid_to_scancode;
use crate::{InputInjector, PlatformError, Result};

#[derive(Debug)]
pub struct SendInputInjector {
    /// The captured output on the virtual desktop, in pixels.
    output: RECT,
    /// The whole virtual desktop, in pixels.
    virt_x: i32,
    virt_y: i32,
    virt_w: i32,
    virt_h: i32,
}

impl SendInputInjector {
    pub fn new(output: RECT) -> Result<Self> {
        // SAFETY: GetSystemMetrics has no preconditions.
        let (virt_x, virt_y, virt_w, virt_h) = unsafe {
            (
                GetSystemMetrics(SM_XVIRTUALSCREEN),
                GetSystemMetrics(SM_YVIRTUALSCREEN),
                GetSystemMetrics(SM_CXVIRTUALSCREEN),
                GetSystemMetrics(SM_CYVIRTUALSCREEN),
            )
        };
        if virt_w <= 0 || virt_h <= 0 {
            return Err(PlatformError::Backend(anyhow::anyhow!("virtual desktop has no size")));
        }
        Ok(Self { output, virt_x, virt_y, virt_w, virt_h })
    }

    /// Stream-normalised (0..=65535 over the output) → virtual-desktop
    /// normalised (0..=65535 over all monitors), as `SendInput` wants.
    fn to_virtual(&self, x: u16, y: u16) -> (i32, i32) {
        let ow = f64::from(self.output.right - self.output.left);
        let oh = f64::from(self.output.bottom - self.output.top);
        let px = f64::from(self.output.left) + f64::from(x) / 65535.0 * ow;
        let py = f64::from(self.output.top) + f64::from(y) / 65535.0 * oh;
        let nx = (px - f64::from(self.virt_x)) / f64::from(self.virt_w) * 65535.0;
        let ny = (py - f64::from(self.virt_y)) / f64::from(self.virt_h) * 65535.0;
        (nx.round().clamp(0.0, 65535.0) as i32, ny.round().clamp(0.0, 65535.0) as i32)
    }

    fn send(inputs: &[INPUT]) -> Result<()> {
        // SAFETY: the slice is valid for its length; cbsize is the struct size.
        let n = unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) };
        if n as usize == inputs.len() {
            Ok(())
        } else {
            Err(PlatformError::Backend(anyhow::anyhow!("SendInput accepted {n} of {} events", inputs.len())))
        }
    }

    fn mouse(dx: i32, dy: i32, data: u32, flags: MOUSE_EVENT_FLAGS) -> INPUT {
        INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 { mi: MOUSEINPUT { dx, dy, mouseData: data, dwFlags: flags, time: 0, dwExtraInfo: 0 } },
        }
    }
}

impl InputInjector for SendInputInjector {
    fn inject(&mut self, event: InputEvent) -> Result<()> {
        let input = match event {
            InputEvent::MouseMoveAbs { x, y } => {
                let (vx, vy) = self.to_virtual(x, y);
                Self::mouse(vx, vy, 0, MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK)
            }
            InputEvent::MouseMoveRel { dx, dy } => Self::mouse(i32::from(dx), i32::from(dy), 0, MOUSEEVENTF_MOVE),
            InputEvent::MouseButton { button, pressed } => {
                let (flags, data) = match (button, pressed) {
                    (MouseButton::Left, true) => (MOUSEEVENTF_LEFTDOWN, 0),
                    (MouseButton::Left, false) => (MOUSEEVENTF_LEFTUP, 0),
                    (MouseButton::Right, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
                    (MouseButton::Right, false) => (MOUSEEVENTF_RIGHTUP, 0),
                    (MouseButton::Middle, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
                    (MouseButton::Middle, false) => (MOUSEEVENTF_MIDDLEUP, 0),
                    (MouseButton::Back, true) => (MOUSEEVENTF_XDOWN, 1),
                    (MouseButton::Back, false) => (MOUSEEVENTF_XUP, 1),
                    (MouseButton::Forward, true) => (MOUSEEVENTF_XDOWN, 2),
                    (MouseButton::Forward, false) => (MOUSEEVENTF_XUP, 2),
                };
                Self::mouse(0, 0, data, flags)
            }
            InputEvent::MouseScroll { dx, dy } => {
                let mut batch = Vec::with_capacity(2);
                if dy != 0 {
                    batch.push(Self::mouse(0, 0, i32::from(dy) as u32, MOUSEEVENTF_WHEEL));
                }
                if dx != 0 {
                    batch.push(Self::mouse(0, 0, i32::from(dx) as u32, MOUSEEVENTF_HWHEEL));
                }
                return Self::send(&batch);
            }
            InputEvent::Key { hid_usage, pressed } => {
                let up = if pressed { KEYBD_EVENT_FLAGS(0) } else { KEYEVENTF_KEYUP };
                let ki = match hid_to_scancode(hid_usage) {
                    Some((scan, extended)) => {
                        let mut flags = KEYEVENTF_SCANCODE | up;
                        if extended {
                            flags |= KEYEVENTF_EXTENDEDKEY;
                        }
                        KEYBDINPUT { wVk: VIRTUAL_KEY(0), wScan: scan, dwFlags: flags, time: 0, dwExtraInfo: 0 }
                    }
                    None if hid_usage == 0x48 => {
                        KEYBDINPUT { wVk: VK_PAUSE, wScan: 0, dwFlags: up, time: 0, dwExtraInfo: 0 }
                    }
                    None => return Ok(()), // unmapped key: ignore rather than guess
                };
                INPUT { r#type: INPUT_KEYBOARD, Anonymous: INPUT_0 { ki } }
            }
            InputEvent::Gamepad { .. } => return Ok(()), // routed to VirtualGamepad when one exists
        };
        Self::send(&[input])
    }
}
