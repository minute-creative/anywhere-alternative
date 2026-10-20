//! Which text field on this Mac has the keyboard focus, and where it is.
//!
//! The phone has no keyboard of its own on screen while watching, so a
//! click into a search box or a form left the person with nothing to type
//! with (owner, 0.6.0: "when I click any input field I'm not able to
//! directly type anything"). The host asks the Mac's accessibility system
//! a few times a second which element has the focus; when it is a text
//! field, the phone opens its keyboard and zooms the picture to the field.
//!
//! Uses the Accessibility permission the host already has for input. Web
//! browsers built on Chromium (Chrome, Edge, Brave, Electron apps such as
//! Slack and Notion) only describe their pages to accessibility tools that
//! ask for it; the standard way to ask is the `AXManualAccessibility`
//! attribute, set once per app. That makes those apps keep an
//! accessibility tree, which costs them a little memory and CPU while the
//! phone is connected.
//!
//! In plain words: "is the Mac waiting for typing, and where on screen?"

#![allow(unsafe_code, clippy::pedantic)]

use std::ffi::c_void;
use std::sync::Mutex;

use objc2_core_foundation::{CFRetained, CFString};

type CFTypeRef = *const c_void;

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
struct Point {
    x: f64,
    y: f64,
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
struct Size {
    width: f64,
    height: f64,
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
struct Rect {
    origin: Point,
    size: Size,
}

/// `kAXValueCGPointType` and `kAXValueCGSizeType`.
const AX_POINT: u32 = 1;
const AX_SIZE: u32 = 2;

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXIsProcessTrusted() -> u8;
    fn AXUIElementCreateSystemWide() -> CFTypeRef;
    fn AXUIElementCopyAttributeValue(element: CFTypeRef, attribute: CFTypeRef, value: *mut CFTypeRef) -> i32;
    fn AXUIElementSetAttributeValue(element: CFTypeRef, attribute: CFTypeRef, value: CFTypeRef) -> i32;
    fn AXUIElementGetPid(element: CFTypeRef, pid: *mut i32) -> i32;
    fn AXUIElementSetMessagingTimeout(element: CFTypeRef, seconds: f32) -> i32;
    fn AXValueGetValue(value: CFTypeRef, kind: u32, out: *mut c_void) -> u8;
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFRelease(cf: CFTypeRef);
    static kCFBooleanTrue: CFTypeRef;
}

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGMainDisplayID() -> u32;
    fn CGDisplayBounds(display: u32) -> Rect;
}

/// A Core Foundation object we own; released when dropped.
struct Owned(CFTypeRef);

impl Drop for Owned {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: we hold one reference from a Create/Copy call.
            unsafe { CFRelease(self.0) }
        }
    }
}

fn key(name: &str) -> CFRetained<CFString> {
    CFString::from_str(name)
}

fn as_ref(s: &CFRetained<CFString>) -> CFTypeRef {
    let r: &CFString = s;
    (r as *const CFString).cast()
}

/// One attribute of an accessibility element, if it has it.
fn copy(element: CFTypeRef, name: &str) -> Option<Owned> {
    let k = key(name);
    let mut out: CFTypeRef = std::ptr::null();
    // SAFETY: `element` is a live AXUIElement, `k` a live CFString, `out`
    // receives a +1 reference we wrap in `Owned`.
    let err = unsafe { AXUIElementCopyAttributeValue(element, as_ref(&k), &mut out) };
    if err != 0 || out.is_null() {
        if !out.is_null() {
            drop(Owned(out));
        }
        return None;
    }
    Some(Owned(out))
}

/// A string attribute (the element's role, for example).
fn string(element: CFTypeRef, name: &str) -> Option<String> {
    let v = copy(element, name)?;
    // SAFETY: role and subrole attributes are CFStrings.
    let s: &CFString = unsafe { &*(v.0 as *const CFString) };
    Some(s.to_string())
}

fn ax_value<T: Default>(element: CFTypeRef, name: &str, kind: u32) -> Option<T> {
    let v = copy(element, name)?;
    let mut out = T::default();
    // SAFETY: position and size attributes are AXValues of `kind`, and
    // `out` has the matching C layout.
    let ok = unsafe { AXValueGetValue(v.0, kind, (&mut out as *mut T).cast()) };
    (ok != 0).then_some(out)
}

/// Apps already asked to describe themselves (by process id).
static ASKED: Mutex<Vec<i32>> = Mutex::new(Vec::new());

/// Ask a Chromium or Electron app to build its accessibility tree, once.
fn ask_for_accessibility(app: CFTypeRef) {
    let mut pid = 0;
    // SAFETY: `app` is a live AXUIElement; `pid` is a plain out value.
    if unsafe { AXUIElementGetPid(app, &mut pid) } != 0 {
        return;
    }
    let Ok(mut asked) = ASKED.lock() else { return };
    if asked.contains(&pid) {
        return;
    }
    if asked.len() > 64 {
        asked.clear(); // process ids are reused; keep the list short
    }
    asked.push(pid);
    let k = key("AXManualAccessibility");
    // SAFETY: setting a boolean attribute; apps that don't know it refuse
    // with an error we ignore.
    let _ = unsafe { AXUIElementSetAttributeValue(app, as_ref(&k), kCFBooleanTrue) };
}

/// Roles that take typing. Search fields and password fields are text
/// fields with a subrole.
fn takes_text(role: &str) -> bool {
    matches!(role, "AXTextField" | "AXTextArea" | "AXComboBox" | "AXSearchField")
}

/// The focused text field: where it is as fractions of the main display
/// (x, y, width, height; top-left origin), and whether it is a password
/// field. `None` when the focus is not on a text field, or when this
/// program lacks the Accessibility permission.
///
/// Fractions of the main display match the picture when the whole screen
/// or one app is shared; when one window is shared (a crop), the phone's
/// zoom lands slightly off, which is harmless.
pub fn focused_text_field() -> Option<([f32; 4], bool)> {
    // SAFETY: plain query.
    if unsafe { AXIsProcessTrusted() } == 0 {
        return None;
    }
    // SAFETY: returns a +1 reference (or null), owned by `Owned`.
    let system = Owned(unsafe { AXUIElementCreateSystemWide() });
    if system.0.is_null() {
        return None;
    }
    // A busy app must not hold the host up: give up on it after 100 ms.
    // SAFETY: live element.
    unsafe { AXUIElementSetMessagingTimeout(system.0, 0.1) };
    let app = copy(system.0, "AXFocusedApplication")?;
    // SAFETY: live element.
    unsafe { AXUIElementSetMessagingTimeout(app.0, 0.1) };
    ask_for_accessibility(app.0);
    let element = copy(app.0, "AXFocusedUIElement")?;
    let role = string(element.0, "AXRole")?;
    if !takes_text(&role) {
        return None;
    }
    let secure = string(element.0, "AXSubrole").is_some_and(|s| s == "AXSecureTextField");
    let pos: Point = ax_value(element.0, "AXPosition", AX_POINT)?;
    let size: Size = ax_value(element.0, "AXSize", AX_SIZE)?;
    // SAFETY: plain queries.
    let screen = unsafe { CGDisplayBounds(CGMainDisplayID()) };
    let rect = normalise(
        (pos.x, pos.y, size.width, size.height),
        (screen.origin.x, screen.origin.y, screen.size.width, screen.size.height),
    )?;
    Some((rect, secure))
}

/// A rectangle in global points as fractions of `screen`, clipped to it.
/// `None` when it lies outside the screen altogether.
fn normalise(r: (f64, f64, f64, f64), screen: (f64, f64, f64, f64)) -> Option<[f32; 4]> {
    let (sx, sy, sw, sh) = screen;
    if sw <= 0.0 || sh <= 0.0 {
        return None;
    }
    let x0 = ((r.0 - sx) / sw).clamp(0.0, 1.0);
    let y0 = ((r.1 - sy) / sh).clamp(0.0, 1.0);
    let x1 = ((r.0 + r.2 - sx) / sw).clamp(0.0, 1.0);
    let y1 = ((r.1 + r.3 - sy) / sh).clamp(0.0, 1.0);
    if x1 <= x0 || y1 <= y0 {
        return None;
    }
    // Rounded to 1/1000 so a field that hasn't moved compares equal.
    let q = |v: f64| ((v * 1000.0).round() / 1000.0) as f32;
    Some([q(x0), q(y0), q(x1 - x0), q(y1 - y0)])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fields_become_fractions_of_the_screen() {
        let screen = (0.0, 0.0, 1920.0, 1080.0);
        assert_eq!(normalise((960.0, 540.0, 192.0, 27.0), screen), Some([0.5, 0.5, 0.1, 0.025]));
        // Half off the right edge: clipped.
        assert_eq!(normalise((1824.0, 0.0, 192.0, 108.0), screen), Some([0.95, 0.0, 0.05, 0.1]));
        // On another display entirely: nothing.
        assert_eq!(normalise((2000.0, 0.0, 100.0, 20.0), screen), None);
        assert!(takes_text("AXTextField") && !takes_text("AXButton"));
    }
}
