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
use aa_core::share::ShareTarget;
use std::time::{Duration, Instant};

use super::wininfo::{self, WinInfo};

/// Keeping a viewer inside one shared app or window.
struct Limit {
    target: ShareTarget,
    /// The app's processes.
    pids: Vec<i32>,
    pids_at: Instant,
    /// The window list, refreshed every few hundred ms.
    windows: Vec<WinInfo>,
    windows_at: Instant,
    /// Buttons whose press was dropped (so their release is dropped too).
    dropped_buttons: u8,
    /// Keys whose press was dropped.
    dropped_keys: Vec<u16>,
}

impl Limit {
    fn new(target: &ShareTarget) -> Self {
        let mut l = Self {
            target: target.clone(),
            pids: Vec::new(),
            pids_at: Instant::now() - Duration::from_secs(60),
            windows: Vec::new(),
            windows_at: Instant::now() - Duration::from_secs(60),
            dropped_buttons: 0,
            dropped_keys: Vec::new(),
        };
        l.refresh();
        l
    }

    fn refresh(&mut self) {
        if self.windows_at.elapsed() > Duration::from_millis(300) {
            self.windows = wininfo::on_screen();
            self.windows_at = Instant::now();
        }
        if self.pids_at.elapsed() > Duration::from_secs(2) {
            self.pids = match &self.target {
                ShareTarget::App { id, .. } => super::capture::app_pids(id),
                ShareTarget::Window { id, .. } => {
                    self.windows.iter().filter(|w| u64::from(w.id) == *id).map(|w| w.pid).collect()
                }
                ShareTarget::Screen => Vec::new(),
            };
            self.pids_at = Instant::now();
        }
    }

    /// The shared window's place on screen (window sharing only).
    fn window(&mut self) -> Option<WinInfo> {
        let ShareTarget::Window { id, .. } = self.target else { return None };
        self.refresh();
        self.windows.iter().copied().find(|w| u64::from(w.id) == id)
    }

    /// Would a click at this point land in the shared app?
    fn allows_point(&mut self, x: f64, y: f64) -> bool {
        self.refresh();
        wininfo::at(&self.windows, x, y).is_some_and(|w| self.pids.contains(&w.pid))
    }

    /// Is the shared app the one in front (so typing goes to it)?
    fn allows_keys(&mut self) -> bool {
        self.refresh();
        wininfo::front_pid(&self.windows).is_some_and(|p| self.pids.contains(&p))
    }
}

/// Shortcuts that would leave the shared app or show the rest of the Mac:
/// app switcher, Spotlight, Force Quit, Mission Control and Spaces,
/// screenshots, lock screen. (HID usage of the key, modifier flags that
/// must all be held.)
fn escapes_the_app(usage: u16, flags: u64) -> bool {
    const CMD: u64 = 0x10_0000;
    const OPT: u64 = 0x8_0000;
    const CTRL: u64 = 0x4_0000;
    const SHIFT: u64 = 0x2_0000;
    let has = |m: u64| flags & m == m;
    match usage {
        0x2B => has(CMD) || has(CTRL),   // Tab: app switcher (Ctrl+Tab moves focus areas)
        0x35 => has(CMD),                // ` : next window of an app (harmless) / switcher
        0x2C => has(CMD) || has(CTRL),   // Space: Spotlight, input sources
        0x29 => has(CMD | OPT),          // Esc: Force Quit
        0x4F..=0x52 => has(CTRL),        // arrows: Spaces, Mission Control
        0x3C | 0x3D => true,             // F3 Mission Control, F4 Launchpad
        0x20..=0x22 => has(CMD | SHIFT), // 3, 4, 5: screenshots
        0x14 => has(CMD | CTRL),         // Q: lock screen
        0x07 => has(CMD | OPT) || has(CMD | CTRL), // D: Dock, Dictionary
        0x0B => has(CMD | OPT),          // H: hide all other apps
        _ => false,
    }
}

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

/// USB HID usage of Caps Lock.
const HID_CAPS_LOCK: u16 = 0x39;

/// Caps Lock on a Mac is not a key event: posting `kVK_CapsLock` through
/// Quartz does nothing, because the lock state lives in the HID system.
/// `IOHIDSetModifierLockState` on the `IOHIDSystem` service flips it (and
/// the keyboard's light) the way the real key does.
mod caps_lock {
    use std::ffi::c_void;

    const IO_HID_PARAM_CONNECT_TYPE: u32 = 1;
    /// `kIOHIDCapsLockState`.
    const CAPS_LOCK_SELECTOR: i32 = 0;

    #[link(name = "IOKit", kind = "framework")]
    extern "C" {
        fn IOServiceMatching(name: *const std::ffi::c_char) -> *mut c_void;
        fn IOServiceGetMatchingService(main_port: u32, matching: *mut c_void) -> u32;
        fn IOServiceOpen(service: u32, owning_task: u32, connect_type: u32, connect: *mut u32) -> i32;
        fn IOServiceClose(connect: u32) -> i32;
        fn IOObjectRelease(object: u32) -> i32;
        fn IOHIDGetModifierLockState(handle: u32, selector: i32, state: *mut bool) -> i32;
        fn IOHIDSetModifierLockState(handle: u32, selector: i32, state: bool) -> i32;
    }
    extern "C" {
        static mach_task_self_: u32;
    }

    /// A connection to the HID system, opened once.
    pub struct Lock(u32);

    impl Lock {
        pub fn open() -> Option<Self> {
            // SAFETY: plain IOKit calls; the service handle is released
            // after the connection is opened, as the API requires.
            unsafe {
                let matching = IOServiceMatching(c"IOHIDSystem".as_ptr());
                if matching.is_null() {
                    return None;
                }
                let service = IOServiceGetMatchingService(0, matching); // consumes `matching`
                if service == 0 {
                    return None;
                }
                let mut connect = 0u32;
                let rc = IOServiceOpen(service, mach_task_self_, IO_HID_PARAM_CONNECT_TYPE, &mut connect);
                IOObjectRelease(service);
                (rc == 0 && connect != 0).then_some(Self(connect))
            }
        }

        pub fn toggle(&self) -> bool {
            // SAFETY: valid connection from `open`.
            unsafe {
                let mut on = false;
                if IOHIDGetModifierLockState(self.0, CAPS_LOCK_SELECTOR, &mut on) != 0 {
                    return false;
                }
                IOHIDSetModifierLockState(self.0, CAPS_LOCK_SELECTOR, !on) == 0
            }
        }
    }

    impl Drop for Lock {
        fn drop(&mut self) {
            // SAFETY: closing our own connection once.
            unsafe {
                IOServiceClose(self.0);
            }
        }
    }
}

pub struct MacInput {
    source: Option<CFRetained<CGEventSource>>,
    caps: Option<caps_lock::Lock>,
    /// Where we last put the pointer, in global display points.
    x: f64,
    y: f64,
    /// Mouse buttons held (bit per `MouseButton`).
    buttons: u8,
    /// Keys held, so modifier flags and `ReleaseAll` are right.
    keys: Vec<u16>,
    clicks: ClickCounter,
    limit: Option<Limit>,
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
            caps: {
                let c = caps_lock::Lock::open();
                if c.is_none() {
                    tracing::warn!("Caps Lock cannot be toggled on this Mac (HID system not reachable)");
                }
                c
            },
            limit: None,
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
        // Sharing one window: the pointer stays inside it.
        if let Some(w) = self.limit.as_mut().and_then(Limit::window) {
            self.x = self.x.clamp(w.x, w.x + w.w - 1.0);
            self.y = self.y.clamp(w.y, w.y + w.h - 1.0);
            return;
        }
        let b = CGDisplayBounds(CGMainDisplayID());
        self.x = self.x.clamp(b.origin.x, b.origin.x + b.size.width - 1.0);
        self.y = self.y.clamp(b.origin.y, b.origin.y + b.size.height - 1.0);
    }

    /// Sharing one app: may this press go through? Releases follow their press.
    fn button_allowed(&mut self, bit: u8, pressed: bool) -> bool {
        let (x, y) = (self.x, self.y);
        let Some(l) = self.limit.as_mut() else { return true };
        if pressed {
            let ok = l.allows_point(x, y);
            if !ok {
                l.dropped_buttons |= bit;
            }
            ok
        } else if l.dropped_buttons & bit != 0 {
            l.dropped_buttons &= !bit;
            false
        } else {
            true
        }
    }

    fn key_allowed(&mut self, usage: u16, pressed: bool) -> bool {
        let flags = self.flags();
        let Some(l) = self.limit.as_mut() else { return true };
        if modifier_flag(usage).is_some() {
            return true; // modifiers alone do nothing; keep their state right
        }
        if pressed {
            let ok = l.allows_keys() && !escapes_the_app(usage, flags);
            if !ok && !l.dropped_keys.contains(&usage) {
                l.dropped_keys.push(usage);
            }
            ok
        } else if l.dropped_keys.contains(&usage) {
            l.dropped_keys.retain(|k| *k != usage);
            false
        } else {
            true
        }
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
        if !self.button_allowed(bit, pressed) {
            return;
        }
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
        if usage == HID_CAPS_LOCK {
            // Toggle on press only; the release of a toggle key means nothing.
            if pressed {
                match &self.caps {
                    Some(c) if c.toggle() => {}
                    _ => tracing::warn!("Caps Lock toggle failed"),
                }
            }
            return;
        }
        let Some(code) = hid_to_mac_keycode(usage) else { return };
        if !self.key_allowed(usage, pressed) {
            return;
        }
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

    fn scroll(&mut self, dx: i16, dy: i16) {
        let (x, y) = (self.x, self.y);
        if let Some(l) = self.limit.as_mut() {
            if !l.allows_point(x, y) {
                return;
            }
        }
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
                // The picture is the shared window when sharing one; else the screen.
                let (ox, oy, w, h) = match self.limit.as_mut().and_then(Limit::window) {
                    Some(win) => (win.x, win.y, win.w, win.h),
                    None => {
                        let b = CGDisplayBounds(CGMainDisplayID());
                        (b.origin.x, b.origin.y, b.size.width, b.size.height)
                    }
                };
                let nx = ox + f64::from(x) / 65535.0 * w;
                let ny = oy + f64::from(y) / 65535.0 * h;
                let (dx, dy) = (nx - self.x, ny - self.y);
                self.x = nx;
                self.y = ny;
                self.clamp_to_screen();
                self.moved(dx, dy);
            }
            InputEvent::MouseMoveRel { dx, dy } => {
                // Game mouse: start from where the pointer really is. Games
                // pull it back to the middle of the screen after every move;
                // moving on from our own last position looked to them like
                // a huge jump each time (the camera spun, 0.5.4).
                if let Some(now) = CGEvent::new(None) {
                    let at = CGEvent::location(Some(&now));
                    self.x = at.x;
                    self.y = at.y;
                }
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

    fn set_limit(&mut self, limit: Option<&ShareTarget>) {
        self.limit = limit.filter(|t| !t.is_screen()).map(Limit::new);
        match limit {
            Some(t) if !t.is_screen() => tracing::info!(app = t.label(), "the viewer can only use the shared app"),
            _ => tracing::info!("the viewer can use the whole Mac"),
        }
    }
}
