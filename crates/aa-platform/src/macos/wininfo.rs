//! Which windows are on screen, where, and whose (Quartz window list).
//!
//! Used to keep a viewer inside one shared app: a click counts only if the
//! window under it belongs to that app, and typing only while that app is
//! in front. Reading the list costs well under a millisecond; callers still
//! cache it for a moment because mouse moves come hundreds of times a second.

#![allow(unsafe_code, clippy::pedantic)]

use std::ffi::c_void;

use objc2_core_foundation::{CFDictionary, CFRetained, CFString, CGRect};
use objc2_core_graphics::{
    kCGWindowBounds, kCGWindowLayer, kCGWindowName, kCGWindowNumber, kCGWindowOwnerName, kCGWindowOwnerPID,
    CGRectMakeWithDictionaryRepresentation, CGWindowListCopyWindowInfo, CGWindowListOption,
};

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    pub(super) fn CFArrayGetCount(array: *const c_void) -> isize;
    pub(super) fn CFArrayGetValueAtIndex(array: *const c_void, index: isize) -> *const c_void;
    fn CFDictionaryGetValue(dict: *const c_void, key: *const c_void) -> *const c_void;
    fn CFNumberGetValue(number: *const c_void, kind: isize, out: *mut c_void) -> u8;
}

/// `kCFNumberSInt64Type`.
const SINT64: isize = 4;

#[derive(Debug, Clone, Copy)]
pub struct WinInfo {
    pub id: u32,
    pub pid: i32,
    /// 0 for ordinary windows; menus, pop-ups and panels sit higher.
    pub layer: i64,
    /// In global display points (top-left origin), like mouse positions.
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl WinInfo {
    pub fn contains(&self, px: f64, py: f64) -> bool {
        px >= self.x && py >= self.y && px < self.x + self.w && py < self.y + self.h
    }
}

/// A number stored in a CF dictionary (raw pointer from a CF array).
pub(super) fn number(dict: *const c_void, key: &CFString) -> Option<i64> {
    // SAFETY: `dict` is a live CFDictionary from the window list; keys are
    // static CFStrings; the value, if present, is a CFNumber.
    unsafe {
        let v = CFDictionaryGetValue(dict, (key as *const CFString).cast());
        if v.is_null() {
            return None;
        }
        let mut out = 0i64;
        (CFNumberGetValue(v, SINT64, (&mut out as *mut i64).cast()) != 0).then_some(out)
    }
}

/// Every window on screen, front to back.
pub fn on_screen() -> Vec<WinInfo> {
    let option = CGWindowListOption::OptionOnScreenOnly | CGWindowListOption::ExcludeDesktopElements;
    let Some(list) = CGWindowListCopyWindowInfo(option, 0) else { return Vec::new() };
    let arr = CFRetained::as_ptr(&list).as_ptr() as *const c_void;
    let mut out = Vec::new();
    // SAFETY: `arr` is the CFArray we hold; its values are CFDictionaries.
    unsafe {
        for i in 0..CFArrayGetCount(arr) {
            let d = CFArrayGetValueAtIndex(arr, i);
            if d.is_null() {
                continue;
            }
            let (Some(id), Some(pid)) = (number(d, kCGWindowNumber), number(d, kCGWindowOwnerPID)) else { continue };
            let layer = number(d, kCGWindowLayer).unwrap_or(0);
            let b = CFDictionaryGetValue(d, (kCGWindowBounds as *const CFString).cast());
            let mut r = CGRect::default();
            if b.is_null() || !CGRectMakeWithDictionaryRepresentation(Some(&*(b as *const CFDictionary)), &mut r) {
                continue;
            }
            out.push(WinInfo {
                id: id as u32,
                pid: pid as i32,
                layer,
                x: r.origin.x,
                y: r.origin.y,
                w: r.size.width,
                h: r.size.height,
            });
        }
    }
    out
}

/// The app in front: the owner of the frontmost ordinary window.
pub fn front_pid(list: &[WinInfo]) -> Option<i32> {
    list.iter().find(|w| w.layer == 0).map(|w| w.pid)
}

/// The window under a point (topmost first, any layer, so an open menu
/// counts as its app's).
pub fn at(list: &[WinInfo], x: f64, y: f64) -> Option<WinInfo> {
    list.iter().copied().find(|w| w.contains(x, y) && w.w > 1.0 && w.h > 1.0)
}

/// A text value stored in a CF dictionary (raw pointer from a CF array).
fn text(dict: *const c_void, key: &CFString) -> Option<String> {
    // SAFETY: `dict` is a live CFDictionary from the window list; the value,
    // if present, is a CFString owned by the dictionary.
    unsafe {
        let v = CFDictionaryGetValue(dict, (key as *const CFString).cast());
        (!v.is_null()).then(|| (*(v as *const CFString)).to_string())
    }
}

/// The front app's name, for picking a game's controls. A Windows game
/// through CrossOver runs as a Wine process whose name says little
/// ("wine64-preloader"), so the window's title ("ELDEN RING™") is added
/// when it differs. Empty when nothing ordinary is in front.
pub fn front_app_name() -> String {
    let option = CGWindowListOption::OptionOnScreenOnly | CGWindowListOption::ExcludeDesktopElements;
    let Some(list) = CGWindowListCopyWindowInfo(option, 0) else { return String::new() };
    let arr = CFRetained::as_ptr(&list).as_ptr() as *const c_void;
    // SAFETY: `arr` is the CFArray we hold; its values are CFDictionaries.
    unsafe {
        for i in 0..CFArrayGetCount(arr) {
            let d = CFArrayGetValueAtIndex(arr, i);
            if d.is_null() || number(d, kCGWindowLayer).unwrap_or(1) != 0 {
                continue;
            }
            let owner = text(d, kCGWindowOwnerName).unwrap_or_default();
            let title = text(d, kCGWindowName).unwrap_or_default();
            return if title.is_empty() || owner.contains(&title) { owner } else { format!("{owner} · {title}") };
        }
    }
    String::new()
}
