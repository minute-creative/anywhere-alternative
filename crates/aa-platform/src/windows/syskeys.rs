//! Keeping Windows' own shortcuts for the stream window.
//!
//! In plain words: Windows acts on some keys before any app sees them. The
//! Windows key opens Start, Alt+Tab switches apps, Alt+Esc and Ctrl+Esc
//! too, and Alt+F4 closes the window in front. While the stream window is in
//! front and full screen (or game mouse is on) these should go to the other
//! computer instead, the way they do in Parsec (0.5.7 report: "Alt+Tab
//! changes to another Windows app, Alt+F4 closes the streaming app").
//!
//! How: a low-level keyboard hook (`WH_KEYBOARD_LL`), which Windows calls
//! for every key before acting on it. For those few combinations, when our
//! window is in front, we hand the key to the stream and tell Windows to
//! forget it. Every other key takes the normal path through the window.
//! Ctrl+Alt+Del can never be taken (Windows keeps it, by design).
//!
//! Getting out: Ctrl+Shift+G (game mouse) and Ctrl+Shift+S (settings) are
//! not taken, and leaving full screen stops all of this.

#![allow(unsafe_code, clippy::pedantic)]

use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::{Mutex, OnceLock};

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, VK_CONTROL, VK_ESCAPE, VK_F4, VK_LWIN, VK_RWIN, VK_TAB,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetForegroundWindow, SetWindowsHookExW, HC_ACTION, KBDLLHOOKSTRUCT, LLKHF_ALTDOWN, LLKHF_EXTENDED,
    LLKHF_INJECTED, WH_KEYBOARD_LL, WM_KEYDOWN, WM_SYSKEYDOWN,
};

/// Where taken keys go: (HID usage, pressed).
type Sink = Box<dyn Fn(u16, bool) + Send + Sync>;

static SINK: OnceLock<Mutex<Option<Sink>>> = OnceLock::new();
/// The stream window.
static WINDOW: AtomicIsize = AtomicIsize::new(0);
/// Full screen or game mouse, and the settings panel closed.
static ACTIVE: AtomicBool = AtomicBool::new(false);

/// Start taking the shortcuts for `window` (its raw `HWND`). Call once, on
/// the window's own thread (the hook runs on the thread that set it, which
/// must keep handling window messages: winit's event loop does).
pub fn install(window: isize, sink: Sink) {
    WINDOW.store(window, Ordering::Relaxed);
    let slot = SINK.get_or_init(|| Mutex::new(None));
    let first = slot.lock().map(|mut s| s.replace(sink).is_none()).unwrap_or(false);
    if !first {
        return; // already hooked; only the window or sink changed
    }
    // SAFETY: a standard hook on this thread; `hook` matches HOOKPROC and
    // stays valid for the whole program (a plain function).
    let r = unsafe {
        let module = GetModuleHandleW(windows::core::PCWSTR::null()).ok();
        SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook), module.map(Into::into), 0)
    };
    match r {
        Ok(_) => tracing::info!("system shortcuts go to the other computer while the stream is full screen"),
        Err(e) => tracing::warn!("couldn't keep the Windows key and Alt+Tab for the stream: {e}"),
    }
}

/// Take the shortcuts now (full screen or game mouse, panel closed), or not.
pub fn set_active(on: bool) {
    ACTIVE.store(on, Ordering::Relaxed);
}

/// Should this key, in this state, go to the stream instead of Windows?
fn taken(vk: u32, alt: bool, ctrl: bool) -> bool {
    let vk = vk as u16;
    vk == VK_LWIN.0
        || vk == VK_RWIN.0
        || (vk == VK_TAB.0 && alt)
        || (vk == VK_ESCAPE.0 && (alt || ctrl))
        || (vk == VK_F4.0 && alt)
}

unsafe extern "system" fn hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 && ACTIVE.load(Ordering::Relaxed) {
        // SAFETY: for WH_KEYBOARD_LL with HC_ACTION, lparam points to a
        // KBDLLHOOKSTRUCT that is valid for this call.
        let k = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
        // SAFETY: plain Win32 calls.
        let front = unsafe { GetForegroundWindow() };
        let ctrl = unsafe { GetAsyncKeyState(VK_CONTROL.0 as i32) } < 0;
        let ours = front.0 as isize == WINDOW.load(Ordering::Relaxed) && front != HWND::default();
        let injected = k.flags.0 & LLKHF_INJECTED.0 != 0;
        let alt = k.flags.0 & LLKHF_ALTDOWN.0 != 0;
        if ours && !injected && taken(k.vkCode, alt, ctrl) {
            let extended = k.flags.0 & LLKHF_EXTENDED.0 != 0;
            let down = wparam.0 as u32 == WM_KEYDOWN || wparam.0 as u32 == WM_SYSKEYDOWN;
            if let Some(hid) = crate::hid_scancode::scancode_to_hid(k.scanCode as u16, extended) {
                if let Some(Ok(sink)) = SINK.get().map(Mutex::lock) {
                    if let Some(s) = sink.as_ref() {
                        s(hid, down);
                    }
                }
            }
            return LRESULT(1); // Windows forgets this key
        }
    }
    // SAFETY: passing the event on, as every hook must.
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}
