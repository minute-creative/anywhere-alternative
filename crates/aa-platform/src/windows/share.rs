//! Sharing one app or one window on Windows.
//!
//! Windows has no "capture just this app" switch that keeps everything on
//! the GPU at 240 frames a second, so we do it ourselves: the whole screen
//! is captured as before, then only the parts where the shared app's
//! windows are visible are copied into the picture; the rest stays black.
//! Windows on top of the shared app (another program, a notification) are
//! cut out too, so nothing that wasn't chosen is ever sent. For one window
//! the picture is also cropped to it.
//!
//! An app is identified by its program file (`chrome.exe`), so all of its
//! windows, menus and pop-ups count, and it is still the same app after a
//! restart.

#![allow(unsafe_code, clippy::pedantic)]

use aa_core::share::ShareTarget;
use windows::core::BOOL;
use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM, POINT, RECT};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetAncestor, GetClassNameW, GetForegroundWindow, GetWindow, GetWindowLongPtrW, GetWindowRect,
    GetWindowTextW, GetWindowThreadProcessId, IsIconic, IsWindowVisible, WindowFromPoint, GA_ROOT, GWL_EXSTYLE,
    GW_OWNER, WS_EX_TOOLWINDOW,
};

/// Click-through overlays (game overlays, screen dimmers) don't hide
/// anything a person would see as "on top", so they don't cut holes.
const WS_EX_TRANSPARENT: u32 = 0x20;

/// Our own programs, never offered for sharing.
const OURS: [&str; 3] = ["anywhere.exe", "aa-host.exe", "aa-viewer.exe"];

/// The program file of a process, lower case (`chrome.exe`).
pub fn exe_of_pid(pid: u32) -> Option<String> {
    // SAFETY: query-only handle, closed below; buffer sized as passed.
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 520];
        let mut len = buf.len() as u32;
        let r = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, windows::core::PWSTR(buf.as_mut_ptr()), &mut len);
        let _ = CloseHandle(h);
        r.ok()?;
        let path = String::from_utf16_lossy(&buf[..len as usize]);
        Some(path.rsplit(['\\', '/']).next().unwrap_or(&path).to_lowercase())
    }
}

fn pid_of(hwnd: HWND) -> u32 {
    let mut pid = 0u32;
    // SAFETY: valid out-pointer.
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    pid
}

pub fn exe_of(hwnd: HWND) -> Option<String> {
    exe_of_pid(pid_of(hwnd))
}

/// "chrome.exe" → "Chrome", for lists.
fn pretty(exe: &str) -> String {
    let stem = exe.strip_suffix(".exe").unwrap_or(exe);
    let mut c = stem.chars();
    c.next().map_or_else(String::new, |f| f.to_uppercase().collect::<String>() + c.as_str())
}

fn text(hwnd: HWND) -> String {
    let mut buf = [0u16; 256];
    // SAFETY: buffer passed with its length.
    let n = unsafe { GetWindowTextW(hwnd, &mut buf) };
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

fn class(hwnd: HWND) -> String {
    let mut buf = [0u16; 128];
    // SAFETY: as above.
    let n = unsafe { GetClassNameW(hwnd, &mut buf) };
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

/// The window's visible frame (without the invisible resize border).
pub fn frame_of(hwnd: HWND) -> Option<RECT> {
    let mut r = RECT::default();
    // SAFETY: out-pointer sized as passed.
    let ok = unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            (&mut r as *mut RECT).cast(),
            std::mem::size_of::<RECT>() as u32,
        )
    }
    .is_ok()
        || unsafe { GetWindowRect(hwnd, &mut r) }.is_ok();
    (ok && r.right > r.left && r.bottom > r.top).then_some(r)
}

/// On screen and drawn: visible, not minimised, not "cloaked" (Windows
/// hides windows on other virtual desktops and suspended Store apps that way).
fn showing(hwnd: HWND) -> bool {
    // SAFETY: plain queries on a window handle from EnumWindows.
    unsafe {
        if !IsWindowVisible(hwnd).as_bool() || IsIconic(hwnd).as_bool() {
            return false;
        }
        let mut cloaked = 0u32;
        let _ = DwmGetWindowAttribute(hwnd, DWMWA_CLOAKED, (&mut cloaked as *mut u32).cast(), 4);
        cloaked == 0
    }
}

fn ex_style(hwnd: HWND) -> u32 {
    // SAFETY: plain query.
    unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32 }
}

/// Every top-level window, front to back (EnumWindows' order).
fn top_level() -> Vec<HWND> {
    unsafe extern "system" fn each(hwnd: HWND, lp: LPARAM) -> BOOL {
        // SAFETY: `lp` is the Vec passed below, alive for the whole call.
        let list = unsafe { &mut *(lp.0 as *mut Vec<HWND>) };
        list.push(hwnd);
        BOOL(1)
    }
    let mut list: Vec<HWND> = Vec::new();
    // SAFETY: the callback only pushes into `list`.
    let _ = unsafe { EnumWindows(Some(each), LPARAM(&mut list as *mut Vec<HWND> as isize)) };
    list
}

/// Apps and their windows, for the Share page and full-access viewers.
pub fn shareable() -> Vec<ShareTarget> {
    let mut out = Vec::new();
    for hwnd in top_level() {
        if !showing(hwnd) || ex_style(hwnd) & WS_EX_TOOLWINDOW.0 != 0 {
            continue;
        }
        // Owned windows are dialogs and pop-ups of another window.
        // SAFETY: plain query.
        if unsafe { GetWindow(hwnd, GW_OWNER) }.is_ok_and(|o| !o.is_invalid()) {
            continue;
        }
        let title = text(hwnd);
        let cls = class(hwnd);
        if title.is_empty() || matches!(cls.as_str(), "Progman" | "WorkerW" | "Shell_TrayWnd") {
            continue;
        }
        let Some(exe) = exe_of(hwnd) else { continue };
        if OURS.contains(&exe.as_str())
            || frame_of(hwnd).map_or(true, |r| r.right - r.left < 120 || r.bottom - r.top < 80)
        {
            continue;
        }
        let name = pretty(&exe);
        out.push(ShareTarget::App { id: exe, name: name.clone() });
        out.push(ShareTarget::Window { id: hwnd.0 as usize as u64, app: name, title });
    }
    out
}

/// A process of the shared app (the one owning a visible window), for its
/// sound. Child processes count too (browsers play sound from them).
pub fn pid_for(target: &ShareTarget) -> Option<u32> {
    match target {
        ShareTarget::Screen => None,
        ShareTarget::Window { id, .. } => Some(pid_of(HWND(*id as usize as *mut std::ffi::c_void))).filter(|&p| p != 0),
        ShareTarget::App { id, .. } => top_level()
            .into_iter()
            .filter(|h| showing(*h))
            .find(|h| exe_of(*h).is_some_and(|e| e.eq_ignore_ascii_case(id)))
            .map(pid_of),
    }
}

/// Is this window still open?
pub fn window_exists(id: u64) -> bool {
    top_level().iter().any(|h| h.0 as usize as u64 == id)
}

fn intersect(a: RECT, b: RECT) -> Option<RECT> {
    let r = RECT {
        left: a.left.max(b.left),
        top: a.top.max(b.top),
        right: a.right.min(b.right),
        bottom: a.bottom.min(b.bottom),
    };
    (r.right > r.left && r.bottom > r.top).then_some(r)
}

/// `a` with `b` cut out: up to four rectangles.
fn subtract(a: RECT, b: RECT) -> Vec<RECT> {
    let Some(i) = intersect(a, b) else { return vec![a] };
    let mut out = Vec::with_capacity(4);
    if i.top > a.top {
        out.push(RECT { left: a.left, top: a.top, right: a.right, bottom: i.top });
    }
    if i.bottom < a.bottom {
        out.push(RECT { left: a.left, top: i.bottom, right: a.right, bottom: a.bottom });
    }
    if i.left > a.left {
        out.push(RECT { left: a.left, top: i.top, right: i.left, bottom: i.bottom });
    }
    if i.right < a.right {
        out.push(RECT { left: i.right, top: i.top, right: a.right, bottom: i.bottom });
    }
    out
}

/// What to copy out of the screen picture for one share choice, in the
/// screen's own pixel coordinates (0,0 = its top-left).
#[derive(Debug, Clone, PartialEq)]
pub struct Region {
    /// For one window: the part of the screen the picture is cut to.
    pub crop: Option<RECT>,
    /// The visible parts of the shared app (everything else is black).
    pub keep: Vec<RECT>,
}

/// Work out the region for `target` on the screen at `desktop` (virtual
/// desktop coordinates). `None` for the whole screen.
pub fn region_for(target: &ShareTarget, desktop: RECT) -> Option<Region> {
    let (app_exe, window) = match target {
        ShareTarget::Screen => return None,
        ShareTarget::App { id, .. } => (Some(id.to_lowercase()), None),
        ShareTarget::Window { id, .. } => (None, Some(*id)),
    };
    let window_pid = window.map(|id| pid_of(HWND(id as usize as *mut std::ffi::c_void)));
    let mut covers: Vec<RECT> = Vec::new();
    let mut keep: Vec<RECT> = Vec::new();
    let mut crop = None;
    for hwnd in top_level() {
        if !showing(hwnd) {
            continue;
        }
        let Some(r) = frame_of(hwnd).and_then(|r| intersect(r, desktop)) else { continue };
        let ours = match (&app_exe, window) {
            (Some(exe), _) => exe_of(hwnd).is_some_and(|e| &e == exe),
            // The window itself, plus its own menus and pop-ups (same process).
            (None, Some(id)) => hwnd.0 as usize as u64 == id || Some(pid_of(hwnd)) == window_pid,
            _ => false,
        };
        if window.is_some_and(|id| hwnd.0 as usize as u64 == id) {
            crop = Some(r);
        }
        if ours {
            let mut pieces = vec![r];
            for c in &covers {
                pieces = pieces.into_iter().flat_map(|p| subtract(p, *c)).collect();
            }
            keep.extend(pieces);
            // Our own windows also hide what is under them (even ours).
            covers.push(r);
        } else if ex_style(hwnd) & WS_EX_TRANSPARENT == 0 {
            covers.push(r);
        }
    }
    if window.is_some() && crop.is_none() {
        // Closed or minimised: show nothing rather than the screen.
        return Some(Region { crop: Some(RECT { left: 0, top: 0, right: 2, bottom: 2 }), keep: Vec::new() });
    }
    // To the screen's own coordinates; crops get even sizes for the encoder.
    let shift = |r: RECT| RECT {
        left: r.left - desktop.left,
        top: r.top - desktop.top,
        right: r.right - desktop.left,
        bottom: r.bottom - desktop.top,
    };
    let crop = crop.map(|c| {
        let c = shift(c);
        RECT { right: c.left + ((c.right - c.left) & !1).max(2), bottom: c.top + ((c.bottom - c.top) & !1).max(2), ..c }
    });
    Some(Region { crop, keep: keep.into_iter().map(shift).collect() })
}

/// Would a click at this screen point (virtual desktop coordinates) land
/// in the shared app or window?
pub fn point_allowed(target: &ShareTarget, x: i32, y: i32) -> bool {
    // SAFETY: plain queries.
    let root = unsafe { GetAncestor(WindowFromPoint(POINT { x, y }), GA_ROOT) };
    if root.is_invalid() {
        return false;
    }
    belongs(target, root)
}

/// Is the shared app the one in front (so typing goes to it)?
pub fn foreground_allowed(target: &ShareTarget) -> bool {
    // SAFETY: plain query.
    let fg = unsafe { GetForegroundWindow() };
    !fg.is_invalid() && belongs(target, fg)
}

fn belongs(target: &ShareTarget, hwnd: HWND) -> bool {
    match target {
        ShareTarget::Screen => true,
        ShareTarget::App { id, .. } => exe_of(hwnd).is_some_and(|e| e.eq_ignore_ascii_case(id)),
        ShareTarget::Window { id, .. } => {
            let w = HWND(*id as usize as *mut std::ffi::c_void);
            hwnd == w || pid_of(hwnd) == pid_of(w)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(l: i32, t: i32, rr: i32, b: i32) -> RECT {
        RECT { left: l, top: t, right: rr, bottom: b }
    }

    #[test]
    fn cutting_holes() {
        // A window over the middle of another leaves four strips.
        let parts = subtract(r(0, 0, 100, 100), r(25, 25, 75, 75));
        let area: i32 = parts.iter().map(|p| (p.right - p.left) * (p.bottom - p.top)).sum();
        assert_eq!(area, 100 * 100 - 50 * 50);
        assert_eq!(subtract(r(0, 0, 10, 10), r(20, 20, 30, 30)), vec![r(0, 0, 10, 10)]);
        assert!(subtract(r(0, 0, 10, 10), r(0, 0, 10, 10)).is_empty());
        assert_eq!(pretty("chrome.exe"), "Chrome");
    }
}
