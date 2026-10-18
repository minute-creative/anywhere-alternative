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
    /// User toggled "send my microphone to the PC".
    SetMic(bool),
    /// Full access: ask the host what it could share.
    ShareList,
    /// Full access: show this on the host instead.
    SetShare(aa_core::share::ShareTarget),
    /// The window closed; send `Bye` and exit.
    Quit,
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
    /// The host's list of apps and windows (asked for from the panel).
    pub shareables: Vec<aa_core::share::ShareTarget>,
    /// Things worth knowing that cost quality ("picture is going through
    /// Tailscale's relay").
    pub notes: Vec<String>,
}

static INFO: Mutex<Option<HostInfo>> = Mutex::new(None);

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
