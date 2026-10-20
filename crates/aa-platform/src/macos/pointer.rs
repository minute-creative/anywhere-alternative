//! Has a game on this Mac taken the mouse pointer?
//!
//! A game that steers its camera with the mouse hides the pointer and pulls
//! it back to the middle after every move; its menus show the pointer and
//! leave it where it is. The viewer needs to know which, because the same
//! controller button means different keys in play and in menus (owner,
//! 2026-10-20: Elden Ring with a Razer Kishi on the phone — no menu or
//! button worked). Windows has a direct answer (the pointer is hidden and
//! clipped); a Mac has none, so this module watches two signs:
//!
//! 1. **Pulled back.** The input thread reports every mouse move it posts
//!    and where the pointer was found before the next one. A pointer that
//!    keeps turning up somewhere other than where we put it is being
//!    re-centred by the app in front: it holds the mouse. A move that lands
//!    where we put it means the pointer is free again.
//! 2. **Hidden in a full-screen app.** The system's pointer-visible flag,
//!    counted only while the front app's window covers the whole display
//!    (a Mac also hides the pointer while you type in an ordinary window;
//!    that must not count).
//!
//! The first needs the camera stick to have moved since the game started;
//! the second works at rest but depends on an old macOS call. Every change
//! is logged with both signs, so a test session shows which one works.

#![allow(unsafe_code, clippy::pedantic)]

use std::ffi::{c_char, c_void};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, AtomicU8, Ordering};
use std::sync::OnceLock;
use std::time::Instant;

use objc2_core_graphics::{CGDisplayBounds, CGMainDisplayID};

use super::wininfo;

/// Milliseconds since the first call, so instants fit in atomics.
fn now_ms() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_millis() as u64 + 1
}

/// When the pointer was last found pulled back, and by whose app.
static PULLED_AT: AtomicU64 = AtomicU64::new(0);
static PULLED_PID: AtomicI32 = AtomicI32::new(0);
/// Pull-backs in a row (one could be the person's own mouse on the Mac).
static PULLED_RUN: AtomicU8 = AtomicU8::new(0);
/// When a move last landed where it was put.
static FREE_AT: AtomicU64 = AtomicU64::new(0);

/// How far (points) the pointer may sit from where we put it and still
/// count as "where we put it" (rounding, screen edges).
const NEAR: f64 = 2.0;

/// From the input thread, before each mouse move: where the last move put
/// the pointer, where it is now, and how long ago that move was.
pub fn observe(posted: (f64, f64), found: (f64, f64), since_ms: u64) {
    // Long after the last move, anything could have moved it (the person
    // at the Mac, an app): no evidence either way.
    if since_ms > 250 {
        return;
    }
    let off = (found.0 - posted.0).hypot(found.1 - posted.1);
    if off <= NEAR {
        PULLED_RUN.store(0, Ordering::Relaxed);
        FREE_AT.store(now_ms(), Ordering::Relaxed);
    } else if PULLED_RUN.fetch_add(1, Ordering::Relaxed) >= 2 {
        // Three in a row: the app in front is re-centring the pointer.
        PULLED_RUN.store(3, Ordering::Relaxed);
        PULLED_AT.store(now_ms(), Ordering::Relaxed);
        PULLED_PID.store(front().map_or(0, |f| f.0), Ordering::Relaxed);
    }
}

/// The front app's process id, and whether its window fills the display.
fn front() -> Option<(i32, bool)> {
    let list = wininfo::on_screen();
    let pid = wininfo::front_pid(&list)?;
    let d = CGDisplayBounds(CGMainDisplayID());
    let full = list.iter().any(|w| {
        w.pid == pid
            && w.layer == 0
            && w.x <= d.origin.x + 1.0
            && w.y <= d.origin.y + 1.0
            && w.w >= d.size.width - 2.0
            && w.h >= d.size.height - 2.0
    });
    Some((pid, full))
}

/// `CGCursorIsVisible`: deprecated since macOS 10.9 with no replacement,
/// so looked up at run time — if a future macOS drops it, this sign is
/// simply missing instead of the host failing to start.
fn pointer_visible() -> Option<bool> {
    type F = unsafe extern "C" fn() -> u32;
    static FN: OnceLock<Option<F>> = OnceLock::new();
    extern "C" {
        fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
    }
    let f = FN.get_or_init(|| {
        // RTLD_DEFAULT is (void *)-2 on Apple platforms.
        // SAFETY: plain symbol lookup; the signature matches the C header.
        let p = unsafe { dlsym(-2isize as *mut c_void, c"CGCursorIsVisible".as_ptr()) };
        (!p.is_null()).then(|| unsafe { std::mem::transmute::<*mut c_void, F>(p) })
    });
    // SAFETY: a no-argument query.
    f.map(|f| unsafe { f() } != 0)
}

/// The answer, asked about once a second.
pub fn captured() -> bool {
    let front = front();
    let pulled_at = PULLED_AT.load(Ordering::Relaxed);
    // Pulled back more recently than any free move, by the app still in front.
    let pulled =
        pulled_at > FREE_AT.load(Ordering::Relaxed) && front.is_some_and(|f| f.0 == PULLED_PID.load(Ordering::Relaxed));
    let visible = pointer_visible();
    let full = front.is_some_and(|f| f.1);
    // The visible flag may only describe this program's own pointer on
    // some macOS versions (then it never says "hidden"). Once it has said
    // "hidden" in a full-screen app it is known to work, and it is the
    // better sign: it also notices the game opening a menu (pointer shown)
    // without the stick moving. Until then, rely on pulled back.
    static VISIBLE_WORKS: AtomicBool = AtomicBool::new(false);
    if full && visible == Some(false) {
        VISIBLE_WORKS.store(true, Ordering::Relaxed);
    }
    let captured = if VISIBLE_WORKS.load(Ordering::Relaxed) && full { visible == Some(false) } else { pulled };
    static LAST: AtomicBool = AtomicBool::new(false);
    if LAST.swap(captured, Ordering::Relaxed) != captured {
        tracing::info!(
            captured,
            pulled_back = pulled,
            pointer_visible = ?visible,
            front_full_screen = full,
            visible_trusted = VISIBLE_WORKS.load(Ordering::Relaxed),
            "game pointer capture changed"
        );
    }
    captured
}
