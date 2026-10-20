//! The two one-way channels between the window (main thread) and the
//! network session (tokio thread).
//!
//! Frames go window-ward through a *latest-frame slot*, not a queue: if the
//! window is busy and two frames arrive, the older one is simply replaced.
//! Showing a stale frame is never useful. Input goes session-ward through a
//! bounded channel; events are tiny and must not be dropped, so the channel
//! is generous and `try_send` failure is logged as a bug, not tolerated.

use std::sync::{Arc, Mutex};

use aa_core::input::InputEvent;
use aa_platform::DecodedFrame;
use tokio::sync::mpsc;

/// What the window can tell the session.
#[derive(Debug)]
pub enum ViewerCommand {
    Input(InputEvent),
    /// `DualSense` pass-through (raw reports, see `ds5.rs`).
    Pad(aa_core::ds5::PadMsg),
    /// User changed the bitrate cap in the overlay (kbps).
    SetMaxBitrate(u32),
    /// User toggled "mute PC speakers" in the overlay.
    SetHostMute(bool),
    /// User toggled "black out the PC's screen".
    SetScreenMask(bool),
    /// User toggled "send my microphone to the PC".
    SetMic(bool),
    /// Full access: ask the host what it could share.
    ShareList,
    /// Full access: show this on the host instead.
    SetShare(aa_core::share::ShareTarget),
    /// The launcher: ask the host for its apps and games.
    AppList,
    /// The launcher: open, hide or quit one of them; `true` = then show
    /// only that app (needs full access on the host).
    AppAction(String, aa_core::apps::AppAction, bool),
    /// Something copied on this device (the phone), for the host's clipboard.
    CopyItem(aa_core::clipboard::ClipItem),
    /// The window closed; send `Bye` and exit.
    Quit,
}

/// Events the session thread sends to the window (or the phone app).
#[derive(Debug, Clone)]
pub enum Wake {
    /// A new frame is in the slot; redraw.
    Frame,
    /// The session ended (host gone, error); close the window.
    SessionEnded(String),
    /// The host went quiet; we are trying to get it back.
    Reconnecting,
    /// The host accepted us (first time or after a reconnect).
    Connected,
    /// Why the host's picture is paused, or `None` when it is back.
    HostStatus(Option<String>),
}

/// Live numbers for the stats line; the session updates these.
#[derive(Debug, Clone, Copy, Default)]
pub struct LiveStats {
    pub fps: u32,
    pub mbps: f32,
    pub rtt_ms: f32,
    pub loss_pct: f32,
}

/// Session → window.
#[derive(Clone)]
pub struct FrameSlot {
    latest: Arc<Mutex<Option<DecodedFrame>>>,
    /// Called after a frame is stored, to wake the window's event loop.
    wake: Arc<dyn Fn() + Send + Sync>,
}

impl std::fmt::Debug for FrameSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FrameSlot").finish_non_exhaustive()
    }
}

impl FrameSlot {
    pub fn new(wake: impl Fn() + Send + Sync + 'static) -> Self {
        Self { latest: Arc::new(Mutex::new(None)), wake: Arc::new(wake) }
    }

    /// Store a frame, replacing any unshown one, and wake the window.
    pub fn publish(&self, frame: DecodedFrame) {
        *self.latest.lock().expect("frame slot poisoned") = Some(frame);
        (self.wake)();
    }

    /// Take the newest frame, if a new one arrived since the last take.
    pub fn take(&self) -> Option<DecodedFrame> {
        self.latest.lock().expect("frame slot poisoned").take()
    }
}

/// The host is a Mac (set at connect). Decides how Ctrl and Cmd are
/// mapped; read by the window on every key.
static HOST_IS_MAC: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn set_host_os(os: &str) {
    let mac = os == "macos";
    if HOST_IS_MAC.swap(mac, std::sync::atomic::Ordering::Relaxed) != mac || mac {
        tracing::info!(host = if os.is_empty() { "windows" } else { os }, "host operating system");
    }
}

pub fn host_is_mac() -> bool {
    HOST_IS_MAC.load(std::sync::atomic::Ordering::Relaxed)
}

pub fn command_channel() -> (mpsc::Sender<ViewerCommand>, mpsc::Receiver<ViewerCommand>) {
    mpsc::channel(1024)
}

/// What the host told us about the stream, for the corner display and the
/// settings panel. Written by the session, read by the window each frame.
#[derive(Debug, Clone, Default)]
pub struct HostInfo {
    pub codec: Option<aa_core::video::Codec>,
    /// Frames a second the host is aiming for (the connection decides).
    pub target_fps: u16,
    pub target_kbps: u32,
    /// What is shown ("Whole screen", "Safari").
    pub sharing: String,
    /// We may choose what is shared.
    pub full_access: bool,
    /// The host program's version ("0.6.0"); empty from hosts before 0.5.9.
    pub host_version: String,
    /// The host's list of apps and windows (asked for from the panel).
    pub shareables: Vec<aa_core::share::ShareTarget>,
    /// Things worth knowing that cost quality ("picture is going through
    /// Tailscale's relay").
    pub notes: Vec<String>,
    /// The host's current input warning (also one of `notes`).
    pub warning: Option<String>,
    /// The one-time "update the other computer" note, if due.
    pub update_note: Option<crate::update_note::UpdateNote>,
    /// The host's launcher list (`AppList` asked; pages arrive one at a
    /// time and are shown once all are in).
    pub apps: Vec<aa_core::apps::AppEntry>,
    /// Goes up every time `apps` or the icons change, so a screen can
    /// notice without comparing lists.
    pub apps_version: u32,
    /// Pages of the list still on their way (`pages` total, received so far).
    pub apps_pages: (u16, Vec<u16>),
    /// The host's focused text field (fractions of the picture: x, y,
    /// width, height) and whether it is a password field; `None` when no
    /// field has the focus. Only sent to viewers that asked (the phone).
    pub text_field: Option<([f32; 4], bool)>,
}

/// Launcher icons by app id (PNG), kept outside `HostInfo` because they are
/// big and change once per list.
static ICONS: Mutex<Option<std::collections::HashMap<String, bytes::Bytes>>> = Mutex::new(None);

pub fn set_icons(pack: aa_core::apps::IconPack) {
    if let Ok(mut i) = ICONS.lock() {
        *i = Some(pack.icons.into_iter().collect());
    }
    update_host_info(|i| i.apps_version = i.apps_version.wrapping_add(1));
}

pub fn icon(id: &str) -> Option<bytes::Bytes> {
    ICONS.lock().ok().and_then(|i| i.as_ref().and_then(|m| m.get(id).cloned()))
}

/// What the host copied, on systems where the app owns the clipboard
/// (iOS): kept until the app takes it.
static CLIP_IN: Mutex<Option<aa_core::clipboard::ClipItem>> = Mutex::new(None);

pub fn clip_received(item: aa_core::clipboard::ClipItem) {
    if let Ok(mut c) = CLIP_IN.lock() {
        *c = Some(item);
    }
}

pub fn take_clip() -> Option<aa_core::clipboard::ClipItem> {
    CLIP_IN.lock().ok().and_then(|mut c| c.take())
}

/// Decoded sound waiting to be played, for systems where the Swift/UI side
/// owns the speaker (iOS). 48 kHz, stereo, interleaved 16-bit; at most
/// ~200 ms is kept so a stall never turns into a lasting delay.
static AUDIO_OUT: Mutex<std::collections::VecDeque<i16>> = Mutex::new(std::collections::VecDeque::new());

/// Samples the session decoded (stereo interleaved, 48 kHz).
pub fn audio_push(pcm: &[i16]) {
    const KEEP: usize = 48_000 * 2 / 5; // 200 ms
    if let Ok(mut q) = AUDIO_OUT.lock() {
        q.extend(pcm.iter().copied());
        let extra = q.len().saturating_sub(KEEP);
        if extra > 0 {
            q.drain(..extra);
        }
    }
}

/// Fill `out` with the next samples (silence when there are none);
/// returns how many were real.
pub fn audio_pull(out: &mut [i16]) -> usize {
    let Ok(mut q) = AUDIO_OUT.lock() else {
        out.fill(0);
        return 0;
    };
    let n = q.len().min(out.len());
    for (dst, src) in out.iter_mut().zip(q.drain(..n)) {
        *dst = src;
    }
    out[n..].fill(0);
    n
}

static INFO: Mutex<Option<HostInfo>> = Mutex::new(None);

/// A game on the host has the mouse pointer (see `StreamInfo`); the window
/// turns game mouse on and off to match.
pub static HOST_CURSOR_CAPTURED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn host_info() -> HostInfo {
    INFO.lock().ok().and_then(|i| i.clone()).unwrap_or_default()
}

pub fn update_host_info(f: impl FnOnce(&mut HostInfo)) {
    if let Ok(mut i) = INFO.lock() {
        f(i.get_or_insert_with(HostInfo::default));
    }
}

/// What happened in this viewing, for the anonymous usage note sent when
/// it ends (`aa_platform::telemetry`, only with permission). Counts, never
/// content.
#[derive(Debug, Clone, Default)]
pub struct Summary {
    /// Seconds connected and receiving.
    pub seconds: u64,
    /// Seconds with no new picture although the host wasn't paused (after
    /// the first picture): what the person feels as a freeze.
    pub frozen_seconds: u64,
    /// Sum of frames shown each second (average = this / seconds).
    pub fps_sum: u64,
    pub max_mbps: f32,
    pub reconnects: u32,
    pub game_mouse: bool,
    pub mic: bool,
    pub camera: bool,
}

static SUMMARY: Mutex<Option<Summary>> = Mutex::new(None);

pub fn summary() -> Summary {
    SUMMARY.lock().ok().and_then(|s| s.clone()).unwrap_or_default()
}

pub fn update_summary(f: impl FnOnce(&mut Summary)) {
    if let Ok(mut s) = SUMMARY.lock() {
        f(s.get_or_insert_with(Summary::default));
    }
}
