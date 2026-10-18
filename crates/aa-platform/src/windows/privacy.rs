//! Screen mask: black out the PC's own monitor while someone works on it
//! from another computer.
//!
//! In plain words: the owner uses the Mac to work on the PC and doesn't
//! want people near the PC to see what's on it. With the mask on, the PC's
//! monitor shows black with one small line ("This PC is in use remotely"),
//! while the picture sent to the viewer stays normal.
//!
//! How: one black window over every monitor, always on top, that Windows
//! is told to leave out of screen captures (`WDA_EXCLUDEFROMCAPTURE`,
//! Windows 10 2004 and later). Screen capture sees straight through it, so
//! the viewer sees the real desktop; the monitor shows the black window.
//! Mouse clicks pass through it (`WS_EX_TRANSPARENT`), so the viewer's
//! clicks reach the apps underneath. If Windows refuses to hide it from
//! capture, the mask is not shown at all: it would black out the viewer too.
//!
//! The window lives on its own thread with its own message loop. A
//! once-a-second timer keeps it on top and the right size (monitors can
//! change), and closes it when the mask is switched off.
//!
//! Limits: the PC's own mouse pointer still shows on the black screen, and
//! Start or notification pop-ups may appear over it briefly.

#![allow(unsafe_code, clippy::pedantic)]

use std::sync::atomic::{AtomicBool, Ordering};

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateFontW, DeleteObject, DrawTextW, EndPaint, GetStockObject, SelectObject, SetBkMode, SetTextColor,
    BLACK_BRUSH, CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET, DT_CENTER, DT_SINGLELINE, DT_VCENTER,
    FW_NORMAL, HBRUSH, HGDIOBJ, OUT_DEFAULT_PRECIS, PAINTSTRUCT, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW, GetSystemMetrics, PostQuitMessage,
    RegisterClassW, SetLayeredWindowAttributes, SetTimer, SetWindowDisplayAffinity, SetWindowPos, ShowWindow,
    TranslateMessage, HWND_TOPMOST, LWA_ALPHA, MSG, SM_CXSCREEN, SM_CXVIRTUALSCREEN, SM_CYSCREEN, SM_CYVIRTUALSCREEN,
    SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, SWP_NOACTIVATE, SWP_SHOWWINDOW, SW_SHOWNOACTIVATE, WDA_EXCLUDEFROMCAPTURE,
    WM_DESTROY, WM_PAINT, WM_TIMER, WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
    WS_EX_TRANSPARENT, WS_POPUP,
};

/// What the viewer asked for.
static WANT: AtomicBool = AtomicBool::new(false);
/// The mask's thread is running (its window is up or being made).
static RUNNING: AtomicBool = AtomicBool::new(false);

/// Switch the mask on or off. Cheap; safe to call from any thread.
pub fn set(on: bool) {
    let was = WANT.swap(on, Ordering::SeqCst);
    if on && !was {
        tracing::info!("screen mask on: this PC's monitor goes black, the viewer still sees it");
    } else if !on && was {
        tracing::info!("screen mask off");
    }
    if on && !RUNNING.swap(true, Ordering::SeqCst) {
        let started = std::thread::Builder::new().name("aa-mask".into()).spawn(|| {
            run();
            RUNNING.store(false, Ordering::SeqCst);
        });
        if started.is_err() {
            RUNNING.store(false, Ordering::SeqCst);
        }
    }
    // Off: the window's timer notices within a second and closes it.
}

/// All monitors together, in screen pixels: (x, y, width, height).
fn all_screens() -> (i32, i32, i32, i32) {
    // SAFETY: plain metric queries.
    unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    }
}

fn run() {
    // Be on the desktop that is on the screen (the host runs as SYSTEM and
    // follows the sign-in screen too).
    super::session::follow_input_desktop();
    // SAFETY: standard window creation on this thread; `proc` matches
    // WNDPROC and is a plain function; the window is destroyed by its timer.
    unsafe {
        let Ok(module) = GetModuleHandleW(PCWSTR::null()) else { return };
        let class = WNDCLASSW {
            lpfnWndProc: Some(proc),
            hInstance: module.into(),
            hbrBackground: HBRUSH(GetStockObject(BLACK_BRUSH).0),
            lpszClassName: w!("AnywhereScreenMask"),
            ..Default::default()
        };
        RegisterClassW(&class); // 0 on the second run: already registered, fine
        let (x, y, cx, cy) = all_screens();
        let hwnd = match CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            w!("AnywhereScreenMask"),
            w!("Anywhere screen mask"),
            WS_POPUP,
            x,
            y,
            cx,
            cy,
            None,
            None,
            Some(module.into()),
            None,
        ) {
            Ok(h) => h,
            Err(e) => {
                tracing::warn!("screen mask: couldn't make the window: {e}");
                return;
            }
        };
        // Leave it out of every capture first; without that, don't show it.
        if let Err(e) = SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE) {
            tracing::warn!("screen mask needs Windows 10 (2004) or newer, not shown: {e}");
            let _ = DestroyWindow(hwnd);
            return;
        }
        let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), 255, LWA_ALPHA);
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        SetTimer(Some(hwnd), 1, 1000, None);
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

unsafe extern "system" fn proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_TIMER => {
            if WANT.load(Ordering::SeqCst) {
                // Stay on top of anything opened since, covering every monitor.
                let (x, y, cx, cy) = all_screens();
                let _ = SetWindowPos(hwnd, Some(HWND_TOPMOST), x, y, cx, cy, SWP_NOACTIVATE | SWP_SHOWWINDOW);
            } else {
                let _ = DestroyWindow(hwnd);
            }
            LRESULT(0)
        }
        WM_PAINT => {
            paint(hwnd);
            LRESULT(0)
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// Black (the window's background) plus one dim line in the middle of the
/// main monitor, so nobody thinks the PC crashed.
unsafe fn paint(hwnd: HWND) {
    let mut ps = PAINTSTRUCT::default();
    let dc = BeginPaint(hwnd, &mut ps);
    // The main monitor starts at screen (0, 0); the window starts at the
    // top-left of all monitors.
    let (x, y, _, _) = all_screens();
    let mut main = RECT {
        left: -x,
        top: -y,
        right: -x + GetSystemMetrics(SM_CXSCREEN),
        bottom: -y + GetSystemMetrics(SM_CYSCREEN),
    };
    let font = CreateFontW(
        -36,
        0,
        0,
        0,
        FW_NORMAL.0 as i32,
        0,
        0,
        0,
        DEFAULT_CHARSET,
        OUT_DEFAULT_PRECIS,
        CLIP_DEFAULT_PRECIS,
        CLEARTYPE_QUALITY,
        0,
        w!("Segoe UI"),
    );
    let old = SelectObject(dc, HGDIOBJ(font.0));
    SetBkMode(dc, TRANSPARENT);
    SetTextColor(dc, COLORREF(0x0070_7070));
    let mut text: Vec<u16> = "This PC is in use remotely".encode_utf16().collect();
    DrawTextW(dc, &mut text, &mut main, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
    SelectObject(dc, old);
    let _ = DeleteObject(HGDIOBJ(font.0));
    let _ = EndPaint(hwnd, &ps);
}
