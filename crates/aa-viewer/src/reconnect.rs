//! Connecting and staying connected: run sessions until the user quits,
//! finding the host again when it goes quiet. Shared by the desktop viewer
//! (`main.rs`) and the iPhone app (`aa-mobile`).
//!
//! In plain words: "connect to this computer, and if it stops answering,
//! look for it again (it may have a new address) and reconnect".

#![allow(clippy::doc_markdown)]

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Context;

use crate::link::{self, FrameSlot, LiveStats, ViewerCommand, Wake};
use crate::session::{self, CallPrefs, MicStart};

/// The most frames a second we ever ask for (the owner's choice).
pub const FPS_LIMIT: u16 = 240;

/// What the person chose for this connection: who, which picture, which
/// devices. The desktop program fills it from command-line options, the
/// phone from its settings screen.
#[derive(Debug, Clone)]
pub struct ConnectPrefs {
    /// A computer name (partial is fine), an IP, or ip:port; `None` finds
    /// the host automatically.
    pub host: Option<String>,
    pub bind: SocketAddr,
    pub mock: bool,
    pub mock_raw: bool,
    pub test_input: bool,
    /// "full", or a height such as "1080" / "720".
    pub resolution: String,
    /// "auto" or a number.
    pub fps: String,
    /// What this screen can show, when the OS can't be asked (the phone
    /// knows its own refresh rate; a desktop passes `None` to ask the OS).
    pub screen_hz: Option<u16>,
    pub calls: CallPrefs,
}

impl Default for ConnectPrefs {
    fn default() -> Self {
        Self {
            host: None,
            bind: "0.0.0.0:0".parse().expect("address"),
            mock: false,
            mock_raw: false,
            test_input: false,
            resolution: "full".into(),
            fps: "auto".into(),
            screen_hz: None,
            calls: CallPrefs::default(),
        }
    }
}

/// Apply the Resolution and Frame rate settings to what we tell the host.
pub fn apply_choices(caps: &mut aa_core::capability::Capabilities, prefs: &ConnectPrefs) {
    let screen = prefs.screen_hz.or_else(aa_platform::display_refresh_hz);
    let limit = caps.max_fps.min(FPS_LIMIT);
    let screen_cap = screen.unwrap_or(limit).clamp(30, limit);
    match prefs.fps.trim().parse::<u16>() {
        Ok(n) if n > 0 => {
            if n > screen_cap {
                tracing::info!(
                    asked = n,
                    screen = screen_cap,
                    "this screen shows at most {screen_cap} frames a second; asking for that"
                );
            }
            caps.max_fps = n.min(screen_cap);
            caps.fixed_fps = true;
        }
        _ => {
            caps.max_fps = screen_cap;
            caps.fixed_fps = false;
        }
    }
    if let Ok(h) = prefs.resolution.trim().trim_end_matches('p').parse::<u32>() {
        // Height decides; width is generous so wide screens keep their shape.
        caps.max_resolution = aa_core::video::Resolution::new((h * 32 / 9).min(caps.max_resolution.width), h);
    }
    // We can resume from a repair frame and report our last good frame —
    // on Apple decoders. Windows' decoders mis-handle a repair frame that
    // refers back past skipped frames: parts of the picture stayed garbled
    // until they changed (PC watching the Mac, 0.5.6). There a loss asks for
    // a fresh keyframe instead: a bigger frame, but always a clean picture.
    caps.loss_recovery = cfg!(any(target_os = "macos", target_os = "ios"));
    tracing::info!(
        max_fps = caps.max_fps,
        fixed = caps.fixed_fps,
        screen_hz = screen,
        max_res = ?caps.max_resolution,
        "asking the host for"
    );
}

/// Without a window (tests), how long we keep trying to get back to a host
/// that went quiet. With a window we keep trying until it is closed: the
/// other computer may be restarting after a power cut, or the Wi-Fi coming
/// back, and the person expects the picture to return by itself.
const RECONNECT_HEADLESS: std::time::Duration = std::time::Duration::from_secs(120);

pub fn backends(prefs: &ConnectPrefs) -> anyhow::Result<aa_platform::ViewerBackends> {
    let mut b = if prefs.mock {
        aa_platform::mock::viewer_backends(prefs.mock_raw)?
    } else {
        aa_platform::viewer_backends().context("real viewer backends unavailable; try --mock")?
    };
    apply_choices(&mut b.capabilities, prefs);
    Ok(b)
}

/// Errors that mean "the host went away" rather than "the host said no":
/// silence, or the OS telling us nothing listens there any more (the host
/// program quit, the PC rebooted) or the network itself vanished.
pub fn is_host_lost(e: &anyhow::Error) -> bool {
    use std::io::ErrorKind as K;
    e.downcast_ref::<session::HostLost>().is_some()
        || e.downcast_ref::<std::io::Error>().is_some_and(|io| {
            matches!(
                io.kind(),
                K::ConnectionRefused | K::ConnectionReset | K::NetworkUnreachable | K::HostUnreachable | K::NetworkDown
            )
        })
}

pub type StatusFn = Arc<dyn Fn(Wake) + Send + Sync>;

/// Run sessions until the user quits. If the host goes quiet (asleep,
/// crashed, Wi-Fi blip) we look for it again (it may have a new address)
/// and reconnect instead of freezing on the last picture: until the window
/// is closed, or for two minutes without a window.
#[allow(clippy::too_many_arguments)]
pub async fn run_reconnecting(
    prefs: &ConnectPrefs,
    first_host: SocketAddr,
    frames: Option<FrameSlot>,
    commands: &mut tokio::sync::mpsc::Receiver<ViewerCommand>,
    stats_tx: Option<std::sync::mpsc::Sender<LiveStats>>,
    mic: MicStart,
    status: Option<StatusFn>,
    pad_out: Option<std::sync::mpsc::Sender<aa_core::ds5::PadMsg>>,
) -> anyhow::Result<()> {
    let mut host = first_host;
    let mut lost_since: Option<std::time::Instant> = None;
    let host_key: Arc<std::sync::Mutex<Option<aa_core::secure::PublicKeyBytes>>> = Arc::default();
    let mut attempt = 0usize;
    loop {
        let on_connected: Option<Box<dyn Fn() + Send>> =
            status.clone().map(|s| Box::new(move || s(Wake::Connected)) as Box<dyn Fn() + Send>);
        let opts = session::Options {
            host,
            bind: prefs.bind,
            frames: frames.clone(),
            test_input: prefs.test_input,
            stats_tx: stats_tx.clone(),
            mic,
            pad_out: pad_out.clone(),
            on_connected,
            on_status: status.clone().map(|s| {
                Box::new(move |m: Option<String>| s(Wake::HostStatus(m))) as Box<dyn Fn(Option<String>) + Send>
            }),
            host_key_out: Some(Arc::clone(&host_key)),
            calls: prefs.calls.clone(),
        };
        let connected = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = Arc::clone(&connected);
        let user_cb = opts.on_connected;
        let opts = session::Options {
            on_connected: Some(Box::new(move || {
                flag.store(true, std::sync::atomic::Ordering::Relaxed);
                if let Some(cb) = &user_cb {
                    cb();
                }
            })),
            ..opts
        };
        let result = session::run(opts, backends(prefs)?, commands).await;
        if connected.load(std::sync::atomic::Ordering::Relaxed) {
            // We were back in business; a later loss starts afresh: a new
            // two minutes (headless) and the short 1 s first wait again.
            // (Before 0.5.8 `attempt` kept counting, so every loss after the
            // first waited the full 5 s before trying.)
            lost_since = None;
            attempt = 0;
        }
        let Err(e) = result else { return Ok(()) };
        let lost = is_host_lost(&e);
        let retrying = lost_since.is_some();
        // Before the first connection, errors are reported, not retried
        // (wrong address, host refused us).
        if !lost && !retrying {
            return Err(e);
        }
        if e.downcast_ref::<crate::net::NotPaired>().is_some() {
            return Err(e);
        }
        if lost_since.is_none() {
            lost_since = Some(std::time::Instant::now());
            tracing::warn!("lost the host; reconnecting");
        }
        if frames.is_none() && lost_since.is_some_and(|t| t.elapsed() > RECONNECT_HEADLESS) {
            return Err(e.context("gave up reconnecting"));
        }
        if let Some(s) = &status {
            s(Wake::Reconnecting);
        }
        link::update_summary(|s| s.reconnects += 1);
        // Back off gently (1 s, then 2 s, at most 5 s) so a long outage
        // doesn't flood the network, but a blip is over in a second.
        attempt += 1;
        tokio::time::sleep(std::time::Duration::from_secs(attempt.min(5) as u64)).await;
        let key = *host_key.lock().expect("key");
        host = next_address(prefs, host, key, attempt).await;
    }
}

/// Where to try next: the same computer wherever it answers now (found by
/// its key: new home address, or through Tailscale), else each address it
/// was ever seen at in turn, else the one we had.
async fn next_address(
    prefs: &ConnectPrefs,
    current: SocketAddr,
    key: Option<aa_core::secure::PublicKeyBytes>,
    attempt: usize,
) -> SocketAddr {
    let Some(key) = key else {
        // Never connected yet: the old behaviour (name or discovery).
        return aa_platform::discover::resolve(prefs.host.as_deref()).await.unwrap_or(current);
    };
    if let Some(found) = aa_platform::discover::locate(&key).await {
        if found != current {
            tracing::info!(addr = %found, "found it again at a new address");
        }
        return found;
    }
    let known: Vec<SocketAddr> = aa_platform::trust::paired_host(&key)
        .map(|h| h.addrs.iter().filter_map(|a| a.parse().ok()).collect())
        .unwrap_or_default();
    if known.is_empty() {
        current
    } else {
        known[attempt % known.len()]
    }
}

/// The anonymous "a viewing ended" note (only with the person's permission;
/// see `aa_platform::telemetry`). Counts and kinds only: how long, how
/// smooth, how it was connected, which features were used.
pub fn send_usage(host: SocketAddr, wall: std::time::Duration, platform: &str) {
    use serde_json::json;
    let s = link::summary();
    if s.seconds == 0 {
        return; // never got going: nothing worth counting
    }
    let info = link::host_info();
    let connection = if aa_platform::tailscale::is_tailscale_ip(host.ip()) {
        "tailscale"
    } else if aa_platform::lan::is_local_network(host.ip()) {
        "home network"
    } else {
        "internet"
    };
    aa_platform::telemetry::send(
        "stream_session",
        &[
            ("minutes", json!(wall.as_secs() / 60)),
            ("avg_fps", json!(s.fps_sum / s.seconds.max(1))),
            ("frozen_seconds", json!(s.frozen_seconds)),
            ("reconnects", json!(s.reconnects)),
            ("max_mbps", json!((s.max_mbps * 10.0).round() / 10.0)),
            ("codec", json!(info.codec.map(|c| format!("{c:?}")))),
            ("host_os", json!(if link::host_is_mac() { "macos" } else { "windows" })),
            ("connection", json!(connection)),
            ("viewer", json!(platform)),
            ("game_mouse", json!(s.game_mouse)),
            ("mic", json!(s.mic)),
            ("camera", json!(s.camera)),
        ],
    );
}
