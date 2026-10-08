//! Viewer input → this Mac, through Quartz events (`CGEventPost`).
//!
//! macOS only lets an app post events if the user has allowed it under
//! System Settings → Privacy & Security → Accessibility (for the app that
//! runs `aa-host`, normally Terminal). Without that the events are silently
//! dropped, so we check once at start and say so clearly.
//!
//! Three things macOS needs that Windows does not, and that are easy to
//! miss: modifier flags on every event (else Cmd+C arrives as plain C),
//! click counts (else double-click never opens anything) and "dragged"
//! events while a button is held (else drag-and-drop and text selection
//! don't work).

#![allow(unsafe_code, clippy::pedantic)]

use aa_core::input::{InputEvent, MouseButton};
use objc2_core_foundation::{CFRetained, CGPoint};
use objc2_core_graphics::{
    CGDisplayBounds, CGEvent, CGEventField, CGEventFlags, CGEventSource, CGEventSourceStateID, CGEventTapLocation,
    CGEventType, CGMainDisplayID, CGMouseButton, CGPreflightPostEventAccess, CGRequestPostEventAccess,
    CGScrollEventUnit,
};

use crate::hid_mac::{hid_to_mac_keycode, modifier_flag, ClickCounter};
use crate::{InputInjector, Result};

// Quartz event types (CGEventTypes.h).
const LEFT_DOWN: u32 = 1;
const LEFT_UP: u32 = 2;
const RIGHT_DOWN: u32 = 3;
const RIGHT_UP: u32 = 4;
const MOVED: u32 = 5;
const LEFT_DRAGGED: u32 = 6;
const RIGHT_DRAGGED: u32 = 7;
const KEY_DOWN: u32 = 10;
const KEY_UP: u32 = 11;
const FLAGS_CHANGED: u32 = 12;
const OTHER_DOWN: u32 = 25;
const OTHER_UP: u32 = 26;
const OTHER_DRAGGED: u32 = 27;
// Event fields.
const FIELD_CLICK_STATE: u32 = 1;
const FIELD_BUTTON_NUMBER: u32 = 3;
const FIELD_DELTA_X: u32 = 4;
const FIELD_DELTA_Y: u32 = 5;

pub struct MacInput {
    source: Option<CFRetained<CGEventSource>>,
    /// Where we last put the pointer, in global display points.
    x: f64,
    y: f64,
    /// Mouse buttons held (bit per `MouseButton`).
    buttons: u8,
    /// Keys held, so modifier flags and `ReleaseAll` are right.
    keys: Vec<u16>,
    clicks: ClickCounter,
}

// SAFETY: the event source is an immutable CF object only used from the
// input thread that owns this struct.
unsafe impl Send for MacInput {}

impl std::fmt::Debug for MacInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MacInput").finish_non_exhaustive()
    }
}

impl MacInput {
    pub fn new() -> Result<Self> {
        if !CGPreflightPostEventAccess() {
            // Shows the system prompt (once) and opens the right settings pane.
            let _ = CGRequestPostEventAccess();
            tracing::warn!(
                "macOS has not allowed this app to control the Mac yet, so the viewer's mouse and keyboard will \
                 be ignored. Allow it in System Settings → Privacy & Security → Accessibility (turn on Terminal, \
                 or whichever app runs aa-host), then restart aa-host"
            );
        }
        let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState);
        let b = CGDisplayBounds(CGMainDisplayID());
        Ok(Self {
            source,
            x: b.origin.x + b.size.width / 2.0,
            y: b.origin.y + b.size.height / 2.0,
            buttons: 0,
            keys: Vec::new(),
            clicks: ClickCounter::default(),
        })
    }

    fn flags(&self) -> u64 {
        self.keys.iter().filter_map(|k| modifier_flag(*k)).fold(0, |a, f| a | f)
    }

    fn post(&self, ev: &CGEvent) {
        CGEvent::set_flags(Some(ev), CGEventFlags(self.flags()));
        CGEvent::post(CGEventTapLocation(0), Some(ev)); // the HID tap: as if from real hardware
    }

    fn mouse(&self, kind: u32, button: u32) -> Option<CFRetained<CGEvent>> {
        CGEvent::new_mouse_event(
            self.source.as_deref(),
            CGEventType(kind),
            CGPoint { x: self.x, y: self.y },
            CGMouseButton(button),
        )
    }

    /// Pointer moved: a drag if a button is held, else a plain move.
    fn moved(&self, dx: f64, dy: f64) {
        let (kind, button) = if self.buttons & 1 != 0 {
            (LEFT_DRAGGED, 0)
        } else if self.buttons & 2 != 0 {
            (RIGHT_DRAGGED, 1)
        } else if self.buttons != 0 {
            (OTHER_DRAGGED, 2)
        } else {
            (MOVED, 0)
        };
        if let Some(ev) = self.mouse(kind, button) {
            // Games in "mouse look" read the deltas, not the position.
            CGEvent::set_integer_value_field(Some(&ev), CGEventField(FIELD_DELTA_X), dx as i64);
            CGEvent::set_integer_value_field(Some(&ev), CGEventField(FIELD_DELTA_Y), dy as i64);
            self.post(&ev);
        }
    }

    fn clamp_to_screen(&mut self) {
        let b = CGDisplayBounds(CGMainDisplayID());
        self.x = self.x.clamp(b.origin.x, b.origin.x + b.size.width - 1.0);
        self.y = self.y.clamp(b.origin.y, b.origin.y + b.size.height - 1.0);
    }

    fn button(&mut self, button: MouseButton, pressed: bool) {
        let (bit, number) = match button {
            MouseButton::Left => (1u8, 0u32),
            MouseButton::Right => (2, 1),
            MouseButton::Middle => (4, 2),
            MouseButton::Back => (8, 3),
            MouseButton::Forward => (16, 4),
        };
        let kind = match (number, pressed) {
            (0, true) => LEFT_DOWN,
            (0, false) => LEFT_UP,
            (1, true) => RIGHT_DOWN,
            (1, false) => RIGHT_UP,
            (_, true) => OTHER_DOWN,
            (_, false) => OTHER_UP,
        };
        let count = if pressed { self.clicks.press(bit, self.x, self.y) } else { self.clicks.current() };
        if pressed {
            self.buttons |= bit;
        } else {
            self.buttons &= !bit;
        }
        if let Some(ev) = self.mouse(kind, number) {
            CGEvent::set_integer_value_field(Some(&ev), CGEventField(FIELD_CLICK_STATE), count);
            CGEvent::set_integer_value_field(Some(&ev), CGEventField(FIELD_BUTTON_NUMBER), i64::from(number));
            self.post(&ev);
        }
    }

    fn key(&mut self, usage: u16, pressed: bool) {
        let Some(code) = hid_to_mac_keycode(usage) else { return };
        if pressed {
            if !self.keys.contains(&usage) {
                self.keys.push(usage);
            }
        } else {
            self.keys.retain(|k| *k != usage);
        }
        let Some(ev) = CGEvent::new_keyboard_event(self.source.as_deref(), code, pressed) else { return };
        if modifier_flag(usage).is_some() {
            // Modifiers are "flags changed" events on a Mac, not key presses.
            CGEvent::set_type(Some(&ev), CGEventType(FLAGS_CHANGED));
        } else {
            CGEvent::set_type(Some(&ev), CGEventType(if pressed { KEY_DOWN } else { KEY_UP }));
        }
        self.post(&ev);
    }

    fn scroll(&self, dx: i16, dy: i16) {
        // From a PC mouse wheel the values are whole notches (120 each):
        // scroll by lines like a Mac mouse wheel. From a trackpad they are
        // pixels: scroll by pixels so it stays smooth.
        let notches = dx % 120 == 0 && dy % 120 == 0;
        let (unit, v, h) =
            if notches { (1, i32::from(dy / 120), i32::from(dx / 120)) } else { (0, i32::from(dy), i32::from(dx)) };
        if let Some(ev) = CGEvent::new_scroll_wheel_event2(self.source.as_deref(), CGScrollEventUnit(unit), 2, v, h, 0)
        {
            self.post(&ev);
        }
    }

    fn release_all(&mut self) {
        for usage in std::mem::take(&mut self.keys) {
            self.key(usage, false);
        }
        for (bit, button) in [
            (1u8, MouseButton::Left),
            (2, MouseButton::Right),
            (4, MouseButton::Middle),
            (8, MouseButton::Back),
            (16, MouseButton::Forward),
        ] {
            if self.buttons & bit != 0 {
                self.button(button, false);
            }
        }
    }
}

impl InputInjector for MacInput {
    fn inject(&mut self, event: InputEvent) -> Result<()> {
        match event {
            InputEvent::MouseMoveAbs { x, y } => {
                let b = CGDisplayBounds(CGMainDisplayID());
                let nx = b.origin.x + f64::from(x) / 65535.0 * b.size.width;
                let ny = b.origin.y + f64::from(y) / 65535.0 * b.size.height;
                let (dx, dy) = (nx - self.x, ny - self.y);
                self.x = nx;
                self.y = ny;
                self.clamp_to_screen();
                self.moved(dx, dy);
            }
            InputEvent::MouseMoveRel { dx, dy } => {
                self.x += f64::from(dx);
                self.y += f64::from(dy);
                self.clamp_to_screen();
                self.moved(f64::from(dx), f64::from(dy));
            }
            InputEvent::MouseButton { button, pressed } => self.button(button, pressed),
            InputEvent::MouseScroll { dx, dy } => self.scroll(dx, dy),
            InputEvent::Key { hid_usage, pressed } => self.key(hid_usage, pressed),
            InputEvent::ReleaseAll => self.release_all(),
            // Controllers are handled by the gamepad backend.
            _ => {}
        }
        Ok(())
    }
}
