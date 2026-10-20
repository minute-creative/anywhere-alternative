//! Phone mode: a phone-shaped screen on this Mac while a phone watches.
//!
//! A Mac screen squeezed onto a phone is unreadable and its buttons are a
//! few points wide (owner, 2026-10-20, on Chrome and Finder from an iPhone
//! 14 Plus with a Razer Kishi). So while the phone is connected this Mac
//! gets an extra, virtual screen with the phone's shape and "Large" text:
//! about 1080 × 500 points, drawn at twice that in pixels so text is sharp.
//! It becomes the main screen (menu bar, Dock and the app in front move
//! there), the stream follows the main screen, and the phone shows it edge
//! to edge. The Mac's own monitor stays on as a second screen (owner's
//! choice). When the phone leaves, the main screen is put back and the
//! virtual one removed; macOS moves its windows home by itself.
//!
//! The virtual screen uses `CGVirtualDisplay`, a part of macOS that Apple
//! has not documented (BetterDisplay and DeskPad use the same one). It is
//! looked up by name at run time: if a macOS update removes it, Phone mode
//! is simply unavailable and the stream shows the normal screen. The screen
//! belongs to this process, so if the host stops for any reason macOS
//! removes it and the Mac goes back to normal.
//!
//! In plain words: "while the phone watches, give the Mac a screen the
//! phone's shape and size, then give it back".

#![allow(unsafe_code, clippy::pedantic)]

use std::ffi::c_void;
use std::sync::Mutex;

use dispatch2::{DispatchQueue, DispatchRetained};
use objc2::msg_send;
use objc2::rc::{Allocated, Retained};
use objc2::runtime::{AnyClass, AnyObject, Bool};
use objc2_core_foundation::{CFRetained, CFString, CGPoint, CGSize};
use objc2_foundation::{NSArray, NSString};

use super::wininfo;

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGMainDisplayID() -> u32;
    fn CGGetActiveDisplayList(max: u32, displays: *mut u32, count: *mut u32) -> i32;
    fn CGDisplayBounds(display: u32) -> Rect;
    fn CGBeginDisplayConfiguration(config: *mut *mut c_void) -> i32;
    fn CGConfigureDisplayOrigin(config: *mut c_void, display: u32, x: i32, y: i32) -> i32;
    fn CGCompleteDisplayConfiguration(config: *mut c_void, option: u32) -> i32;
    fn CGCancelDisplayConfiguration(config: *mut c_void) -> i32;
}

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXUIElementCreateApplication(pid: i32) -> *const c_void;
    fn AXUIElementCopyAttributeValue(
        element: *const c_void,
        attribute: *const c_void,
        value: *mut *const c_void,
    ) -> i32;
    fn AXUIElementSetAttributeValue(element: *const c_void, attribute: *const c_void, value: *const c_void) -> i32;
    fn AXValueCreate(kind: u32, value: *const c_void) -> *const c_void;
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFRelease(cf: *const c_void);
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
struct Rect {
    origin: CGPoint,
    size: CGSize,
}

/// `kCGConfigureForSession`: undone at logout, never written to settings.
const FOR_SESSION: u32 = 1;
/// `kAXValueCGPointType`, `kAXValueCGSizeType`.
const AX_POINT: u32 = 1;
const AX_SIZE: u32 = 2;

/// The virtual screen while it exists, and what to put back afterwards.
struct Active {
    /// Owning this keeps the screen alive; dropping it removes the screen.
    display: Retained<AnyObject>,
    _queue: DispatchRetained<DispatchQueue>,
    id: u32,
    points: (u16, u16),
    /// The main screen before, to make main again.
    old_main: u32,
}

// SAFETY: the Objective-C objects are only touched under the mutex.
unsafe impl Send for Active {}

static ACTIVE: Mutex<Option<Active>> = Mutex::new(None);

fn class(name: &std::ffi::CStr) -> Option<&'static AnyClass> {
    AnyClass::get(name)
}

/// Create the virtual screen at `points` (logical size; pixels are twice
/// that) and make it the main screen. Returns false where it isn't
/// possible (the class is missing, macOS refused); the stream then simply
/// shows the normal screen. Calling it again while active does nothing
/// unless the size changed.
pub fn begin(points: (u16, u16)) -> bool {
    let Ok(mut active) = ACTIVE.lock() else { return false };
    if let Some(a) = active.as_ref() {
        if a.points == points {
            return true;
        }
    }
    if let Some(old) = active.take() {
        finish(old);
    }
    let (w, h) = (u32::from(points.0.max(320)) & !1, u32::from(points.1.max(200)) & !1);
    let Some(created) = create(w, h) else { return false };
    let old_main = unsafe { CGMainDisplayID() };
    if !make_main(created.1, old_main) {
        tracing::warn!("phone mode: the phone screen exists but could not be made the main screen");
    }
    // Give macOS a moment to move the menu bar, then bring the app in front over.
    std::thread::sleep(std::time::Duration::from_millis(600));
    move_front_window(created.1);
    tracing::info!(width = w, height = h, display = created.1, "phone mode on: phone-shaped screen is the main screen");
    *active = Some(Active { display: created.0, _queue: created.2, id: created.1, points, old_main });
    true
}

/// Put the old main screen back and remove the virtual one.
pub fn end() {
    let Ok(mut active) = ACTIVE.lock() else { return };
    if let Some(a) = active.take() {
        finish(a);
    }
}

fn finish(a: Active) {
    // Main first, while both exist, so windows go back to a known screen.
    if a.old_main != a.id && !make_main(a.old_main, a.id) {
        tracing::warn!("phone mode: could not make the Mac's own screen main again");
    }
    drop(a.display);
    tracing::info!("phone mode off: the Mac's own screen is main again");
}

/// The `CGVirtualDisplay` object, its display id and its queue.
fn create(w: u32, h: u32) -> Option<(Retained<AnyObject>, u32, DispatchRetained<DispatchQueue>)> {
    let (Some(desc_c), Some(disp_c), Some(set_c), Some(mode_c)) = (
        class(c"CGVirtualDisplayDescriptor"),
        class(c"CGVirtualDisplay"),
        class(c"CGVirtualDisplaySettings"),
        class(c"CGVirtualDisplayMode"),
    ) else {
        tracing::warn!("phone mode unavailable: this macOS has no CGVirtualDisplay");
        return None;
    };
    let queue = DispatchQueue::new("aa.phone-display", None);
    let name = NSString::from_str("Anywhere phone");
    // SAFETY: messages match the class interfaces BetterDisplay and DeskPad
    // build against; every object is alive for the calls.
    unsafe {
        let desc: Retained<AnyObject> = msg_send![desc_c, new];
        let q = (&*queue as *const DispatchQueue).cast::<AnyObject>();
        let _: () = msg_send![&desc, setQueue: q];
        let _: () = msg_send![&desc, setName: &*name];
        let _: () = msg_send![&desc, setMaxPixelsWide: w * 2];
        let _: () = msg_send![&desc, setMaxPixelsHigh: h * 2];
        // A 6.7" phone held sideways is about 150 × 70 mm.
        let _: () = msg_send![&desc, setSizeInMillimeters: CGSize { width: 150.0, height: 70.0 }];
        let _: () = msg_send![&desc, setVendorID: 0x4157u32]; // "AW"
        let _: () = msg_send![&desc, setProductID: 0x0001u32];
        let _: () = msg_send![&desc, setSerialNum: 0x0001u32];
        let alloc: Allocated<AnyObject> = msg_send![disp_c, alloc];
        let display: Option<Retained<AnyObject>> = msg_send![alloc, initWithDescriptor: &*desc];
        let Some(display) = display else {
            tracing::warn!("phone mode: macOS refused to create the phone screen");
            return None;
        };
        let mode_alloc: Allocated<AnyObject> = msg_send![mode_c, alloc];
        let mode: Option<Retained<AnyObject>> =
            msg_send![mode_alloc, initWithWidth: w, height: h, refreshRate: 60.0f64];
        let mode = mode?;
        let settings: Retained<AnyObject> = msg_send![set_c, new];
        let modes = NSArray::from_retained_slice(&[mode]);
        let _: () = msg_send![&settings, setModes: &*modes];
        // Twice the pixels per point, like a Retina screen: sharp text.
        let _: () = msg_send![&settings, setHiDPI: 1u32];
        let ok: Bool = msg_send![&display, applySettings: &*settings];
        if !ok.as_bool() {
            tracing::warn!("phone mode: macOS refused the phone screen's size");
            return None;
        }
        let id: u32 = msg_send![&display, displayID];
        if id == 0 {
            return None;
        }
        // The new screen takes a moment to appear in the display list.
        for _ in 0..20 {
            if active_displays().contains(&id) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        Some((display, id, queue))
    }
}

fn active_displays() -> Vec<u32> {
    let mut ids = [0u32; 16];
    let mut n = 0u32;
    // SAFETY: a 16-slot buffer and its length.
    if unsafe { CGGetActiveDisplayList(16, ids.as_mut_ptr(), &mut n) } != 0 {
        return Vec::new();
    }
    ids[..n as usize].to_vec()
}

/// Make `main` the main screen (the one at 0,0) and put `other` to its
/// right, top edges level. Undone at logout at the latest.
fn make_main(main: u32, other: u32) -> bool {
    // SAFETY: plain display queries and a configuration we complete or cancel.
    unsafe {
        let width = CGDisplayBounds(main).size.width;
        let mut config: *mut c_void = std::ptr::null_mut();
        if CGBeginDisplayConfiguration(&mut config) != 0 || config.is_null() {
            return false;
        }
        let ok = CGConfigureDisplayOrigin(config, main, 0, 0) == 0
            && (main == other || CGConfigureDisplayOrigin(config, other, width.round() as i32, 0) == 0);
        if !ok {
            CGCancelDisplayConfiguration(config);
            return false;
        }
        CGCompleteDisplayConfiguration(config, FOR_SESSION) == 0
    }
}

/// Move the front app's window onto the phone screen and size it to fill
/// the space under the menu bar (best effort; needs the Accessibility
/// permission the host already has for input).
fn move_front_window(display: u32) {
    let list = wininfo::on_screen();
    let Some(pid) = wininfo::front_pid(&list) else { return };
    // SAFETY: AX calls on an element we own; values are released after use.
    unsafe {
        let b = CGDisplayBounds(display);
        let app = AXUIElementCreateApplication(pid);
        if app.is_null() {
            return;
        }
        let key = |s: &str| -> CFRetained<CFString> { CFString::from_str(s) };
        let as_ref = |s: &CFRetained<CFString>| -> *const c_void {
            let r: &CFString = s;
            (r as *const CFString).cast()
        };
        let mut window: *const c_void = std::ptr::null();
        let k = key("AXFocusedWindow");
        if AXUIElementCopyAttributeValue(app, as_ref(&k), &mut window) == 0 && !window.is_null() {
            // The menu bar is ~25 points tall.
            let pos = CGPoint { x: b.origin.x, y: b.origin.y + 25.0 };
            let size = CGSize { width: b.size.width, height: (b.size.height - 25.0).max(100.0) };
            for (name, kind, value) in [
                ("AXPosition", AX_POINT, (&pos as *const CGPoint).cast::<c_void>()),
                ("AXSize", AX_SIZE, (&size as *const CGSize).cast::<c_void>()),
                // Again: some apps clamp the size while still on the old screen.
                ("AXPosition", AX_POINT, (&pos as *const CGPoint).cast::<c_void>()),
            ] {
                let v = AXValueCreate(kind, value);
                if !v.is_null() {
                    let k = key(name);
                    let _ = AXUIElementSetAttributeValue(window, as_ref(&k), v);
                    CFRelease(v);
                }
            }
            CFRelease(window);
        }
        CFRelease(app);
    }
}

/// Is Phone mode on right now (for the log and the share page)?
pub fn active() -> bool {
    ACTIVE.lock().is_ok_and(|a| a.is_some())
}
