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
use windows::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYBD_EVENT_FLAGS, KEYEVENTF_EXTENDEDKEY,
    KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN,
    MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN,
    MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEINPUT,
    MOUSE_EVENT_FLAGS, VIRTUAL_KEY, VK_PAUSE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SetCursorPos, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
};

use crate::hid_scancode::hid_to_scancode;
use crate::{InputInjector, PlatformError, Result};
use aa_core::share::ShareTarget;

/// Keeping a viewer inside one shared app or window.
#[derive(Debug)]
struct Limit {
    target: ShareTarget,
    dropped_buttons: Vec<MouseButton>,
    dropped_keys: Vec<u16>,
    /// The shared window's frame on screen, re-read every few hundred ms.
    frame: Option<(RECT, std::time::Instant)>,
}

impl Limit {
    fn window_frame(&mut self) -> Option<RECT> {
        let ShareTarget::Window { id, .. } = self.target else { return None };
        if let Some((r, at)) = self.frame {
            if at.elapsed() < std::time::Duration::from_millis(300) {
                return Some(r);
            }
        }
        let hwnd = windows::Win32::Foundation::HWND(id as usize as *mut std::ffi::c_void);
        let r = super::share::frame_of(hwnd)?;
        self.frame = Some((r, std::time::Instant::now()));
        Some(r)
    }
}

/// HID usages of modifier keys (Ctrl, Shift, Alt, Windows; left and right).
fn is_modifier(usage: u16) -> bool {
    (0xE0..=0xE7).contains(&usage)
}

/// Keys that would leave the shared app or show the rest of the PC: the
/// Windows key (Start, and every Win+ shortcut), Alt+Tab, Alt+Esc,
/// Ctrl+Esc, Ctrl+Shift+Esc (Task Manager), Print Screen.
fn escapes_the_app(usage: u16, held: &[u16]) -> bool {
    let ctrl = held.iter().any(|k| matches!(k, 0xE0 | 0xE4));
    let alt = held.iter().any(|k| matches!(k, 0xE2 | 0xE6));
    match usage {
        0xE3 | 0xE7 => true, // Windows key
        0x2B => alt,         // Tab
        0x29 => alt || ctrl, // Esc
        0x46 => true,        // Print Screen
        _ => false,
    }
}

#[derive(Debug)]
pub struct SendInputInjector {
    /// The captured output on the virtual desktop, in pixels.
    output: RECT,
    /// The whole virtual desktop, in pixels.
    virt_x: i32,
    virt_y: i32,
    virt_w: i32,
    virt_h: i32,
    /// Last absolute position we moved to, in screen pixels.
    last_pos: Option<(i32, i32)>,
    /// Keys (HID usage) and buttons currently down, for `ReleaseAll`.
    held_keys: Vec<u16>,
    held_buttons: Vec<MouseButton>,
    limit: Option<Limit>,
}

impl SendInputInjector {
    pub fn new(output: RECT) -> Result<Self> {
        // Without this, Windows lies to us about screen sizes on scaled
        // displays (a 2880x1800 panel at 200% reports as 1440x900) and every
        // injected position lands in the wrong place. Must run before any
        // window or DPI-dependent call; a failure means it was already set.
        // SAFETY: plain Win32 call with a constant argument.
        unsafe {
            let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        }
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
        tracing::info!(?output, virt_x, virt_y, virt_w, virt_h, "input injector ready");
        Ok(Self {
            output,
            virt_x,
            virt_y,
            virt_w,
            virt_h,
            last_pos: None,
            held_keys: Vec::new(),
            held_buttons: Vec::new(),
            limit: None,
        })
    }

    /// Stream-normalised (0..=65535 over the captured output) → screen pixels.
    fn pixels_for(&mut self, x: u16, y: u16) -> (i32, i32) {
        // The captured screen as it is *now*: resolution changes and display
        // switches move it, and a stale rectangle sends clicks to the wrong place.
        let screen = super::capture::current_output_rect().unwrap_or(self.output);
        // Sharing one window: the picture is that window (cut to the screen).
        let o = match self.limit.as_mut().and_then(Limit::window_frame) {
            Some(w) => RECT {
                left: w.left.max(screen.left),
                top: w.top.max(screen.top),
                right: w.right.min(screen.right),
                bottom: w.bottom.min(screen.bottom),
            },
            None => screen,
        };
        let ow = f64::from(o.right - o.left);
        let oh = f64::from(o.bottom - o.top);
        let px = f64::from(o.left) + f64::from(x) / 65535.0 * ow;
        let py = f64::from(o.top) + f64::from(y) / 65535.0 * oh;
        (px.round() as i32, py.round() as i32)
    }

    /// Screen pixels → `SendInput` absolute coordinates (0..=65535 over the
    /// virtual desktop). Used so a click carries its own position and lands
    /// right even if the preceding move was lost.
    fn to_absolute(&self, px: i32, py: i32) -> (i32, i32) {
        // Live virtual-desktop size, for the same reason as above.
        // SAFETY: GetSystemMetrics has no preconditions.
        let (vx, vy, vw, vh) = unsafe {
            (
                GetSystemMetrics(SM_XVIRTUALSCREEN),
                GetSystemMetrics(SM_YVIRTUALSCREEN),
                GetSystemMetrics(SM_CXVIRTUALSCREEN),
                GetSystemMetrics(SM_CYVIRTUALSCREEN),
            )
        };
        let (vx, vy, vw, vh) =
            if vw > 0 && vh > 0 { (vx, vy, vw, vh) } else { (self.virt_x, self.virt_y, self.virt_w, self.virt_h) };
        let nx = f64::from(px - vx) / f64::from(vw) * 65535.0;
        let ny = f64::from(py - vy) / f64::from(vh) * 65535.0;
        (nx.round().clamp(0.0, 65535.0) as i32, ny.round().clamp(0.0, 65535.0) as i32)
    }

    fn send(inputs: &[INPUT]) -> Result<()> {
        // Typing into the sign-in screen needs this thread on that desktop
        // (only possible for the sharing service; harmless otherwise).
        super::session::follow_input_desktop_throttled();
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

impl SendInputInjector {
    /// Sharing one app: may this event go through? Presses outside the app
    /// (and their releases) are dropped, typing only reaches the app while
    /// it is in front, and app-switching keys never pass.
    fn allowed(&mut self, event: &InputEvent) -> bool {
        let pos = self.last_pos;
        let held = self.held_keys.clone();
        let Some(l) = self.limit.as_mut() else { return true };
        let point_ok = |l: &Limit| pos.map_or(true, |(x, y)| super::share::point_allowed(&l.target, x, y));
        match *event {
            InputEvent::MouseButton { button, pressed: true } => {
                let ok = point_ok(l);
                if !ok && !l.dropped_buttons.contains(&button) {
                    l.dropped_buttons.push(button);
                }
                ok
            }
            InputEvent::MouseButton { button, pressed: false } => {
                let dropped = l.dropped_buttons.contains(&button);
                l.dropped_buttons.retain(|b| *b != button);
                !dropped
            }
            InputEvent::MouseScroll { .. } => point_ok(l),
            InputEvent::Key { hid_usage, pressed: true } => {
                let ok = (is_modifier(hid_usage) || super::share::foreground_allowed(&l.target))
                    && !escapes_the_app(hid_usage, &held);
                if !ok && !l.dropped_keys.contains(&hid_usage) {
                    l.dropped_keys.push(hid_usage);
                }
                ok
            }
            InputEvent::Key { hid_usage, pressed: false } => {
                let dropped = l.dropped_keys.contains(&hid_usage);
                l.dropped_keys.retain(|k| *k != hid_usage);
                !dropped
            }
            _ => true,
        }
    }
}

impl InputInjector for SendInputInjector {
    fn set_limit(&mut self, limit: Option<&ShareTarget>) {
        self.limit = limit.filter(|t| !t.is_screen()).map(|t| Limit {
            target: t.clone(),
            dropped_buttons: Vec::new(),
            dropped_keys: Vec::new(),
            frame: None,
        });
        match &self.limit {
            Some(l) => tracing::info!(app = l.target.label(), "the viewer can only use the shared app"),
            None => tracing::info!("the viewer can use the whole PC"),
        }
    }

    fn inject(&mut self, event: InputEvent) -> Result<()> {
        if !self.allowed(&event) {
            return Ok(());
        }
        // Bookkeeping for ReleaseAll.
        match event {
            InputEvent::Key { hid_usage, pressed: true } => {
                if !self.held_keys.contains(&hid_usage) {
                    self.held_keys.push(hid_usage);
                }
            }
            InputEvent::Key { hid_usage, pressed: false } => self.held_keys.retain(|k| *k != hid_usage),
            InputEvent::MouseButton { button, pressed: true } => {
                if !self.held_buttons.contains(&button) {
                    self.held_buttons.push(button);
                }
            }
            InputEvent::MouseButton { button, pressed: false } => self.held_buttons.retain(|b| *b != button),
            InputEvent::ReleaseAll => {
                let keys = std::mem::take(&mut self.held_keys);
                let buttons = std::mem::take(&mut self.held_buttons);
                if !keys.is_empty() || !buttons.is_empty() {
                    tracing::info!(keys = keys.len(), buttons = buttons.len(), "releasing stuck input");
                }
                for k in keys {
                    self.inject(InputEvent::Key { hid_usage: k, pressed: false })?;
                }
                for b in buttons {
                    self.inject(InputEvent::MouseButton { button: b, pressed: false })?;
                }
                return Ok(());
            }
            _ => {}
        }
        let input = match event {
            InputEvent::MouseMoveAbs { x, y } => {
                // SetCursorPos takes real pixels and is immune to the
                // normalisation quirks of SendInput on scaled displays.
                let (px, py) = self.pixels_for(x, y);
                self.last_pos = Some((px, py));
                // SAFETY: plain Win32 call.
                unsafe { SetCursorPos(px, py) }
                    .map_err(|e| PlatformError::Backend(anyhow::anyhow!("SetCursorPos: {e}")))?;
                return Ok(());
            }
            InputEvent::MouseMoveRel { dx, dy } => Self::mouse(i32::from(dx), i32::from(dy), 0, MOUSEEVENTF_MOVE),
            InputEvent::MouseButton { button, pressed } => {
                let (mut flags, data) = match (button, pressed) {
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
                // Carry the position with the click so it lands where the
                // viewer's cursor is, even if a move packet went missing.
                let (ax, ay) = match self.last_pos {
                    Some((px, py)) => {
                        flags |= MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK;
                        self.to_absolute(px, py)
                    }
                    None => (0, 0),
                };
                Self::mouse(ax, ay, data, flags)
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
            InputEvent::Gamepad { .. }
            | InputEvent::GamepadAttach { .. }
            | InputEvent::GamepadDetach { .. }
            | InputEvent::ReleaseAll => return Ok(()), // handled above / elsewhere
        };
        Self::send(&[input])
    }
}
