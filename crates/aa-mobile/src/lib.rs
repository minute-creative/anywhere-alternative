//! Anywhere on a phone: the viewer library behind a plain C interface.
//!
//! The iPhone app is written in Swift (screen, sound, controller, touch).
//! Everything that is the same as on a Mac or PC (finding the host, pairing,
//! encryption, receiving and decoding the picture, sending input) is the
//! `aa-viewer` library, driven through the functions here. Swift sees them
//! through `include/aa_mobile.h`, which lists the same functions in the same
//! order; keep the two in step.
//!
//! Rules of the road:
//! - One connection at a time (`SESSION`). Starting another ends the first.
//! - Nothing here blocks the caller for long except `aa_mobile_discover`
//!   and `aa_mobile_pair`, which the app calls from a background task.
//! - Returned strings are JSON, freed with `aa_mobile_free`. Frames are
//!   borrowed: the pointer is good until the next `aa_mobile_frame`.
//!
//! In plain words: this file is the bridge between the Swift app and the
//! Rust engine. Swift asks "connect to that Mac", "give me the newest
//! picture", "the person pressed X"; Rust does the networking.

#![allow(unsafe_code)] // a C interface: raw pointers and C strings by nature
#![allow(clippy::missing_safety_doc)] // every function has the same contract, stated above
#![allow(clippy::doc_markdown)]

use std::ffi::{c_char, CStr, CString};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, OnceLock};

use aa_core::input::{GamepadKind, GamepadState, InputEvent, MouseButton};
use aa_viewer::link::{self, FrameSlot, LiveStats, ViewerCommand, Wake};
use aa_viewer::reconnect::{self, ConnectPrefs};
use aa_viewer::session::MicStart;
use serde::Serialize;

// ---------------------------------------------------------------------------
// Logging: to `viewer.log` in the data folder and to a buffer the app drains
// (shown on its "Problems" screen).
// ---------------------------------------------------------------------------

static LOG_BUFFER: Mutex<Vec<u8>> = Mutex::new(Vec::new());
static LOG_FILE: Mutex<Option<std::fs::File>> = Mutex::new(None);

struct LogWriter;

impl std::io::Write for LogWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if let Ok(mut b) = LOG_BUFFER.lock() {
            // Keep the unread part bounded (the app may never ask).
            if b.len() > 256 * 1024 {
                b.clear();
            }
            b.extend_from_slice(buf);
        }
        if let Ok(mut f) = LOG_FILE.lock() {
            if let Some(f) = f.as_mut() {
                let _ = f.write_all(buf);
            }
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

static INIT: OnceLock<()> = OnceLock::new();

/// Call once. `data_dir`: a writable folder for keys, pairings and logs.
#[no_mangle]
pub unsafe extern "C" fn aa_mobile_init(data_dir: *const c_char) {
    // SAFETY: the caller passes a NUL-terminated string or null.
    let dir = unsafe { cstr(data_dir) };
    INIT.get_or_init(|| {
        if !dir.is_empty() {
            let _ = std::fs::create_dir_all(&dir);
            // The same switch the desktop uses for tests: every file
            // (keys, pairings) goes under this folder.
            std::env::set_var("AA_DATA_DIR", &dir);
            let path = std::path::Path::new(&dir).join("viewer.log");
            // Keep the previous run's log, like the desktop app does.
            let _ = std::fs::rename(&path, std::path::Path::new(&dir).join("viewer.prev.log"));
            if let Ok(f) = std::fs::File::create(&path) {
                if let Ok(mut g) = LOG_FILE.lock() {
                    *g = Some(f);
                }
            }
        }
        let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into());
        let _ = tracing_subscriber::fmt().with_env_filter(filter).with_ansi(false).with_writer(|| LogWriter).try_init();
        tracing::info!(version = aa_platform::update::current(), "Anywhere mobile started");
    });
}

/// Everything logged since the last call.
#[no_mangle]
pub unsafe extern "C" fn aa_mobile_take_log() -> *mut c_char {
    let text = LOG_BUFFER.lock().map(|mut b| String::from_utf8_lossy(&std::mem::take(&mut *b)).into_owned());
    to_c(&text.unwrap_or_default())
}

// ---------------------------------------------------------------------------
// Finding and pairing
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct FoundJson {
    name: String,
    addr: String,
    key: String,
    paired: bool,
    version: String,
    tailscale: bool,
}

/// Computers sharing right now (this Wi-Fi, and remembered addresses).
#[no_mangle]
pub unsafe extern "C" fn aa_mobile_discover(timeout_ms: u32) -> *mut c_char {
    let found = runtime().block_on(async {
        let search = aa_platform::discover::find_hosts(aa_platform::discover::DEFAULT_PORT);
        let wait = std::time::Duration::from_millis(u64::from(timeout_ms.max(200)));
        match tokio::time::timeout(wait.saturating_add(std::time::Duration::from_secs(3)), search).await {
            Ok(Ok(list)) => list,
            Ok(Err(e)) => {
                tracing::warn!("discovery failed: {e:#}");
                Vec::new()
            }
            Err(_) => Vec::new(),
        }
    });
    let list: Vec<FoundJson> = found
        .iter()
        .map(|f| FoundJson {
            name: f.name.clone(),
            addr: f.addr.to_string(),
            key: f.key.map(|k| aa_core::secure::to_hex(&k)).unwrap_or_default(),
            paired: f.paired(),
            version: f.version.clone(),
            tailscale: f.via_tailscale(),
        })
        .collect();
    json(&list)
}

/// Remembered computers.
#[no_mangle]
pub unsafe extern "C" fn aa_mobile_paired() -> *mut c_char {
    json(&aa_platform::trust::paired_hosts())
}

#[no_mangle]
pub unsafe extern "C" fn aa_mobile_forget(key_hex: *const c_char) {
    aa_platform::trust::forget_host(&unsafe { cstr(key_hex) });
}

/// Pair with the six-digit code. 0 = success, else an error text in `err`.
#[no_mangle]
pub unsafe extern "C" fn aa_mobile_pair(addr: *const c_char, code: *const c_char, err: *mut *mut c_char) -> i32 {
    // SAFETY: C strings from the caller, or null.
    let (addr, code) = unsafe { (cstr(addr), cstr(code)) };
    let result = runtime().block_on(async {
        let target = aa_platform::discover::resolve(Some(&addr)).await?;
        aa_platform::pairing::pair(target, &code).await
    });
    match result {
        Ok(p) => {
            tracing::info!(name = p.name, "paired");
            0
        }
        Err(e) => {
            tracing::warn!("pairing failed: {e:#}");
            if !err.is_null() {
                // SAFETY: `err` is a valid out-pointer from the caller.
                unsafe { *err = to_c(&format!("{e:#}")) };
            }
            1
        }
    }
}

// ---------------------------------------------------------------------------
// The connection
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
struct State {
    /// idle, connecting, connected, reconnecting, ended.
    phase: &'static str,
    reason: String,
    /// Why the picture is paused (host locked…), if it is.
    paused: Option<String>,
    stats: LiveStats,
    max_kbps: u32,
    mute_host: bool,
}

struct Session {
    cmd_tx: tokio::sync::mpsc::Sender<ViewerCommand>,
    frames: FrameSlot,
    state: Arc<Mutex<State>>,
    stats_rx: Mutex<std::sync::mpsc::Receiver<LiveStats>>,
}

static SESSION: Mutex<Option<Arc<Session>>> = Mutex::new(None);
/// The newest frame handed to the app, kept so its pointer stays valid.
static LAST_FRAME: Mutex<Option<aa_platform::DecodedFrame>> = Mutex::new(None);

fn session() -> Option<Arc<Session>> {
    SESSION.lock().ok().and_then(|s| s.clone())
}

/// A small runtime for the short blocking calls (discover, pair). The
/// session has its own, on its own thread, like the desktop viewer.
fn runtime() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().expect("tokio runtime")
    })
}

/// Start connecting; 0 = the session thread is running.
#[no_mangle]
pub unsafe extern "C" fn aa_mobile_connect(
    host: *const c_char,
    resolution: *const c_char,
    fps: u16,
    screen_hz: u16,
    max_mbps: f32,
    mute_host: bool,
    mock: bool,
) -> i32 {
    // SAFETY: C strings from the caller, or null.
    let (host, resolution) = unsafe {
        aa_mobile_disconnect();
        (cstr(host), cstr(resolution))
    };
    let prefs = ConnectPrefs {
        host: (!host.is_empty() && !host.starts_with("key:")).then_some(host.clone()),
        mock,
        resolution: if resolution.is_empty() { "1080".into() } else { resolution },
        fps: if fps == 0 { "auto".into() } else { fps.to_string() },
        screen_hz: (screen_hz > 0).then_some(screen_hz),
        ..ConnectPrefs::default()
    };
    let (cmd_tx, mut cmd_rx) = link::command_channel();
    let frames = FrameSlot::new(|| {});
    let (stats_tx, stats_rx) = std::sync::mpsc::channel();
    let state = Arc::new(Mutex::new(State {
        phase: "connecting",
        max_kbps: (max_mbps.max(0.0) * 1000.0) as u32,
        mute_host,
        ..State::default()
    }));
    let s = Arc::new(Session {
        cmd_tx: cmd_tx.clone(),
        frames: frames.clone(),
        state: Arc::clone(&state),
        stats_rx: Mutex::new(stats_rx),
    });
    if let Ok(mut g) = SESSION.lock() {
        *g = Some(Arc::clone(&s));
    }
    // Settings the host must be told after every (re)connect.
    let status_state = Arc::clone(&state);
    let status_tx = cmd_tx.clone();
    let status: reconnect::StatusFn = Arc::new(move |w: Wake| {
        let mut st = status_state.lock().expect("state");
        match w {
            Wake::Connected => {
                st.phase = "connected";
                st.reason.clear();
                if st.max_kbps > 0 {
                    let _ = status_tx.try_send(ViewerCommand::SetMaxBitrate(st.max_kbps));
                }
                if st.mute_host {
                    let _ = status_tx.try_send(ViewerCommand::SetHostMute(true));
                }
            }
            Wake::Reconnecting => st.phase = "reconnecting",
            Wake::HostStatus(m) => st.paused = m,
            Wake::Frame | Wake::SessionEnded(_) => {}
        }
    });
    let spawned = std::thread::Builder::new()
        .name("aa-session".into())
        .spawn(move || session_thread(&host, &prefs, frames, &mut cmd_rx, stats_tx, status, &state));
    if let Err(e) = spawned {
        tracing::error!("could not start the session thread: {e}");
        return 1;
    }
    0
}

/// Where a "key:<hex>" host is right now: on this Wi-Fi, or any address it
/// was seen at (its Tailscale one when away from home). Tries for ~4 s.
async fn find_by_key(hex: &str) -> anyhow::Result<SocketAddr> {
    let key = aa_core::secure::from_hex(hex).ok_or_else(|| anyhow::anyhow!("bad key"))?;
    for attempt in 0..8usize {
        if let Some(a) = aa_platform::discover::locate(&key).await {
            return Ok(a);
        }
        if let Some(h) = aa_platform::trust::paired_host(&key) {
            if let Some(a) = h.addrs.iter().filter_map(|a| a.parse::<SocketAddr>().ok()).nth(attempt) {
                return Ok(a);
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    anyhow::bail!("that computer is not answering (is it sharing, and on the same Wi-Fi or Tailscale?)")
}

/// The session, on its own thread with its own runtime (like the desktop
/// viewer): find the host, run until the app disconnects or the host is
/// gone for good, then record how it ended.
fn session_thread(
    host: &str,
    prefs: &ConnectPrefs,
    frames: FrameSlot,
    cmd_rx: &mut tokio::sync::mpsc::Receiver<ViewerCommand>,
    stats_tx: std::sync::mpsc::Sender<LiveStats>,
    status: reconnect::StatusFn,
    state: &Mutex<State>,
) {
    let outcome = (|| -> anyhow::Result<()> {
        let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
        let target: SocketAddr = runtime.block_on(async {
            match host.strip_prefix("key:") {
                Some(hex) => find_by_key(hex).await,
                None => aa_platform::discover::resolve(prefs.host.as_deref()).await,
            }
        })?;
        let started = std::time::Instant::now();
        let result = runtime.block_on(reconnect::run_reconnecting(
            prefs,
            target,
            Some(frames),
            cmd_rx,
            Some(stats_tx),
            MicStart::Off,
            Some(status),
            None,
        ));
        reconnect::send_usage(target, started.elapsed(), "iphone");
        result
    })();
    let mut st = state.lock().expect("state");
    st.phase = "ended";
    st.reason = match outcome {
        Ok(()) => String::new(),
        Err(e) => {
            tracing::error!("session failed: {e:#}");
            format!("{e:#}")
        }
    };
}

/// End the session (sends Bye) and forget it.
#[no_mangle]
pub unsafe extern "C" fn aa_mobile_disconnect() {
    let taken = SESSION.lock().ok().and_then(|mut s| s.take());
    if let Some(s) = taken {
        let _ = s.cmd_tx.try_send(ViewerCommand::Quit);
    }
    if let Ok(mut f) = LAST_FRAME.lock() {
        *f = None;
    }
}

#[derive(Serialize)]
#[allow(clippy::struct_excessive_bools)] // it is a status report; these are flags
struct StatusJson {
    state: &'static str,
    reason: String,
    paused: Option<String>,
    fps: u32,
    mbps: f32,
    rtt_ms: f32,
    loss_pct: f32,
    codec: String,
    target_fps: u16,
    target_kbps: u32,
    sharing: String,
    notes: Vec<String>,
    warning: Option<String>,
    host_mac: bool,
    apps_version: u32,
    full_access: bool,
    /// A launcher list was asked for and not every page has arrived.
    apps_pending: bool,
    /// The host's version ("0.6.0"); empty when unknown (old hosts).
    host_version: String,
    /// The host can answer the launcher (0.6 or newer).
    has_launcher: bool,
    /// A game on the host has taken the mouse pointer: send movement, not
    /// positions, and draw no pointer.
    cursor_captured: bool,
}

/// Does a host version string mean "0.6 or newer"? Unknown (empty) means no.
fn launcher_capable(version: &str) -> bool {
    let mut parts = version.trim().split('.').map(|p| p.parse::<u32>().unwrap_or(0));
    let (major, minor) = (parts.next().unwrap_or(0), parts.next().unwrap_or(0));
    major > 0 || minor >= 6
}

#[no_mangle]
pub unsafe extern "C" fn aa_mobile_status() -> *mut c_char {
    let Some(s) = session() else {
        return json(&StatusJson {
            state: "idle",
            reason: String::new(),
            paused: None,
            fps: 0,
            mbps: 0.0,
            rtt_ms: 0.0,
            loss_pct: 0.0,
            codec: String::new(),
            target_fps: 0,
            target_kbps: 0,
            sharing: String::new(),
            notes: Vec::new(),
            warning: None,
            host_mac: false,
            apps_version: 0,
            full_access: false,
            apps_pending: false,
            host_version: String::new(),
            has_launcher: false,
            cursor_captured: false,
        });
    };
    // Newest stats line, if the session produced one since last time.
    if let Ok(rx) = s.stats_rx.lock() {
        let mut latest = None;
        while let Ok(st) = rx.try_recv() {
            latest = Some(st);
        }
        if let Some(st) = latest {
            if let Ok(mut g) = s.state.lock() {
                g.stats = st;
            }
        }
    }
    let st = s.state.lock().map(|g| g.clone()).unwrap_or_default();
    let info = link::host_info();
    json(&StatusJson {
        state: if st.phase.is_empty() { "connecting" } else { st.phase },
        reason: st.reason,
        paused: st.paused,
        fps: st.stats.fps,
        mbps: st.stats.mbps,
        rtt_ms: st.stats.rtt_ms,
        loss_pct: st.stats.loss_pct,
        codec: info.codec.map(|c| format!("{c:?}")).unwrap_or_default(),
        target_fps: info.target_fps,
        target_kbps: info.target_kbps,
        sharing: info.sharing,
        notes: info.notes,
        warning: info.warning,
        host_mac: link::host_is_mac(),
        apps_version: info.apps_version,
        full_access: info.full_access,
        apps_pending: info.apps_pages.0 == 0 || info.apps_pages.1.len() < usize::from(info.apps_pages.0),
        has_launcher: launcher_capable(&info.host_version),
        host_version: info.host_version,
        cursor_captured: link::HOST_CURSOR_CAPTURED.load(std::sync::atomic::Ordering::Relaxed),
    })
}

/// One decoded picture (see the header).
#[repr(C)]
#[derive(Debug)]
pub struct AAFrame {
    pub data: *const u8,
    pub len: usize,
    pub width: u32,
    pub height: u32,
    /// 0 = BGRA, 1 = RGBA, 2 = NV12 (never today; the decoders give RGB).
    pub format: u32,
    pub frame_id: u32,
}

#[no_mangle]
pub unsafe extern "C" fn aa_mobile_frame(out: *mut AAFrame) -> i32 {
    if out.is_null() {
        return 0;
    }
    let Some(s) = session() else { return 0 };
    let Some(frame) = s.frames.take() else { return 0 };
    let aa_platform::FrameBuffer::Cpu(bytes) = &frame.buffer else { return 0 };
    let format = match frame.format {
        aa_core::video::PixelFormat::Bgra8 => 0,
        aa_core::video::PixelFormat::Rgba8 => 1,
        aa_core::video::PixelFormat::Nv12 | aa_core::video::PixelFormat::P010 => 2,
    };
    // SAFETY: `out` is a valid pointer from the caller (checked non-null).
    unsafe {
        *out = AAFrame {
            data: bytes.as_ptr(),
            len: bytes.len(),
            width: frame.resolution.width,
            height: frame.resolution.height,
            format,
            frame_id: frame.frame_id,
        };
    }
    // Keep the bytes alive until the next call replaces them.
    if let Ok(mut g) = LAST_FRAME.lock() {
        *g = Some(frame);
    }
    1
}

#[no_mangle]
pub unsafe extern "C" fn aa_mobile_audio_pull(out: *mut i16, samples: usize) -> usize {
    if out.is_null() || samples == 0 {
        return 0;
    }
    // SAFETY: the caller promises `samples` writable values at `out`.
    let slice = unsafe { std::slice::from_raw_parts_mut(out, samples) };
    link::audio_pull(slice)
}

// ---------------------------------------------------------------------------
// Input
// ---------------------------------------------------------------------------

fn send(ev: InputEvent) {
    if let Some(s) = session() {
        if s.cmd_tx.try_send(ViewerCommand::Input(ev)).is_err() {
            tracing::warn!("input queue full; event dropped");
        }
    }
}

fn command(c: ViewerCommand) {
    if let Some(s) = session() {
        let _ = s.cmd_tx.try_send(c);
    }
}

#[no_mangle]
pub unsafe extern "C" fn aa_mobile_mouse_abs(x: u16, y: u16) {
    send(InputEvent::MouseMoveAbs { x, y });
}

#[no_mangle]
pub unsafe extern "C" fn aa_mobile_mouse_rel(dx: i16, dy: i16) {
    send(InputEvent::MouseMoveRel { dx, dy });
}

#[no_mangle]
pub unsafe extern "C" fn aa_mobile_mouse_button(button: u8, pressed: bool) {
    let button = match button {
        0 => MouseButton::Left,
        1 => MouseButton::Right,
        2 => MouseButton::Middle,
        3 => MouseButton::Back,
        4 => MouseButton::Forward,
        _ => return,
    };
    send(InputEvent::MouseButton { button, pressed });
}

#[no_mangle]
pub unsafe extern "C" fn aa_mobile_mouse_scroll(dx: i16, dy: i16) {
    send(InputEvent::MouseScroll { dx, dy });
}

#[no_mangle]
pub unsafe extern "C" fn aa_mobile_key(hid_usage: u16, pressed: bool) {
    send(InputEvent::Key { hid_usage, pressed });
}

#[no_mangle]
pub unsafe extern "C" fn aa_mobile_gamepad_attach(slot: u8, playstation: bool) {
    let kind = if playstation { GamepadKind::PlayStation } else { GamepadKind::Xbox };
    send(InputEvent::GamepadAttach { slot, kind });
}

#[no_mangle]
pub unsafe extern "C" fn aa_mobile_gamepad_detach(slot: u8) {
    send(InputEvent::GamepadDetach { slot });
}

#[no_mangle]
#[allow(clippy::too_many_arguments)] // one call per controller report
pub unsafe extern "C" fn aa_mobile_gamepad(slot: u8, buttons: u32, lx: i16, ly: i16, rx: i16, ry: i16, lt: u8, rt: u8) {
    let state =
        GamepadState { buttons, left_x: lx, left_y: ly, right_x: rx, right_y: ry, left_trigger: lt, right_trigger: rt };
    send(InputEvent::Gamepad { slot, state });
}

// ---------------------------------------------------------------------------
// Settings while connected
// ---------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn aa_mobile_set_max_mbps(mbps: f32) {
    let kbps = (mbps.max(0.0) * 1000.0) as u32;
    if let Some(s) = session() {
        if let Ok(mut st) = s.state.lock() {
            st.max_kbps = kbps;
        }
    }
    if kbps > 0 {
        command(ViewerCommand::SetMaxBitrate(kbps));
    }
}

#[no_mangle]
pub unsafe extern "C" fn aa_mobile_set_mute_host(muted: bool) {
    if let Some(s) = session() {
        if let Ok(mut st) = s.state.lock() {
            st.mute_host = muted;
        }
    }
    command(ViewerCommand::SetHostMute(muted));
}

// ---------------------------------------------------------------------------
// The launcher
// ---------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn aa_mobile_request_apps() {
    command(ViewerCommand::AppList);
}

#[no_mangle]
pub unsafe extern "C" fn aa_mobile_apps() -> *mut c_char {
    let info = link::host_info();
    // Only a complete list is shown (pages may still be arriving).
    if info.apps_pages.0 == 0 || info.apps_pages.1.len() < usize::from(info.apps_pages.0) {
        return json(&Vec::<aa_core::apps::AppEntry>::new());
    }
    json(&info.apps)
}

#[no_mangle]
pub unsafe extern "C" fn aa_mobile_icon(id: *const c_char, buf: *mut u8, cap: usize) -> usize {
    // SAFETY: C string from the caller; `buf` holds `cap` bytes.
    let Some(png) = link::icon(&unsafe { cstr(id) }) else { return 0 };
    if !buf.is_null() && cap >= png.len() {
        unsafe { std::ptr::copy_nonoverlapping(png.as_ptr(), buf, png.len()) };
    }
    png.len()
}

#[no_mangle]
pub unsafe extern "C" fn aa_mobile_app_action(id: *const c_char, action: *const c_char, show: bool) {
    use aa_core::apps::AppAction;
    // SAFETY: C strings from the caller, or null.
    let (id, action) = unsafe { (cstr(id), cstr(action)) };
    let action = match action.as_str() {
        "open" => AppAction::Open,
        "quit" => AppAction::Quit,
        "hide" => AppAction::Hide,
        other => {
            tracing::warn!(other, "unknown launcher action");
            return;
        }
    };
    command(ViewerCommand::AppAction(id, action, show));
}

/// App mode: show only this app (its bundle id and name) or, with an empty
/// id, the whole screen again. The host obeys only a viewer with Full
/// access (set on its Share page).
#[no_mangle]
pub unsafe extern "C" fn aa_mobile_share(id: *const c_char, name: *const c_char) {
    use aa_core::share::ShareTarget;
    // SAFETY: C strings from the caller, or null.
    let (id, name) = unsafe { (cstr(id), cstr(name)) };
    let target = if id.is_empty() { ShareTarget::Screen } else { ShareTarget::App { id, name } };
    command(ViewerCommand::SetShare(target));
}

// ---------------------------------------------------------------------------
// Clipboard: text copied on the phone goes to the host; text copied on the
// host waits here for the app.
// ---------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn aa_mobile_copy_text(text: *const c_char) {
    // SAFETY: C string from the caller, or null.
    let text = unsafe { cstr(text) };
    if !text.is_empty() {
        command(ViewerCommand::CopyItem(aa_core::clipboard::ClipItem::Text(text)));
    }
}

/// Text the host copied since the last call, or an empty string.
#[no_mangle]
pub unsafe extern "C" fn aa_mobile_take_clip_text() -> *mut c_char {
    match link::take_clip() {
        Some(aa_core::clipboard::ClipItem::Text(t)) => to_c(&t),
        _ => to_c(""),
    }
}

/// Back to sharing the whole screen (after an app was shown alone).
#[no_mangle]
pub unsafe extern "C" fn aa_mobile_show_desktop() {
    command(ViewerCommand::SetShare(aa_core::share::ShareTarget::Screen));
}

#[no_mangle]
pub unsafe extern "C" fn aa_mobile_version() -> *const c_char {
    static V: OnceLock<CString> = OnceLock::new();
    V.get_or_init(|| CString::new(aa_platform::update::current()).unwrap_or_default()).as_ptr()
}

// ---------------------------------------------------------------------------
// C string helpers
// ---------------------------------------------------------------------------

/// A C string to Rust (empty for null). SAFETY: `p` is null or points at a
/// NUL-terminated string that outlives the call.
unsafe fn cstr(p: *const c_char) -> String {
    if p.is_null() {
        return String::new();
    }
    // SAFETY: as above.
    unsafe { CStr::from_ptr(p).to_string_lossy().into_owned() }
}

fn to_c(s: &str) -> *mut c_char {
    // A stray NUL would cut the string; replace rather than fail.
    CString::new(s.replace('\0', " ")).map_or(std::ptr::null_mut(), CString::into_raw)
}

fn json<T: Serialize>(v: &T) -> *mut c_char {
    to_c(&serde_json::to_string(v).unwrap_or_else(|_| "null".into()))
}

#[no_mangle]
pub unsafe extern "C" fn aa_mobile_free(s: *mut c_char) {
    if !s.is_null() {
        // SAFETY: `s` came from `CString::into_raw` in `to_c`.
        drop(unsafe { CString::from_raw(s) });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_without_a_session_is_idle() {
        unsafe {
            let p = aa_mobile_status();
            let s = cstr(p);
            aa_mobile_free(p);
            assert!(s.contains("\"state\":\"idle\""));
            assert_eq!(cstr(aa_mobile_apps()), "[]");
        }
    }

    #[test]
    fn launcher_needs_a_new_enough_host() {
        assert!(launcher_capable("0.6.0"));
        assert!(launcher_capable("1.0.2"));
        assert!(!launcher_capable("0.5.13"));
        assert!(!launcher_capable(""));
    }

    #[test]
    fn strings_round_trip() {
        unsafe {
            let p = to_c("héllo");
            assert_eq!(cstr(p), "héllo");
            aa_mobile_free(p);
            assert_eq!(cstr(std::ptr::null()), "");
        }
    }
}
