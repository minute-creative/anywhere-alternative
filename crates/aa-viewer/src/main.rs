//! `aa-viewer`: run on the machine you are sitting at.
//!
//! ```text
//!  main thread:   winit event loop ──► wgpu present      (window.rs)
//!                      ▲  frames (latest-frame slot)   │ input events
//!  tokio thread:  UDP ─┴─► reassemble ─► decode thread ◄┘  (session.rs)
//! ```
//!
//! The window must live on the main thread (every OS insists), so the
//! network session runs on its own tokio runtime in a background thread.
//! `--headless` skips the window entirely and only prints statistics; CI
//! and quick network checks use that.

mod audio;
mod ds5;
mod keymap;
mod link;
mod net;
mod overlay;
mod pacing;
mod pads;
mod session;
mod udp;
mod window;

use std::net::SocketAddr;

use session::MicStart;

use anyhow::Context;
use clap::Parser;
use tracing_subscriber::EnvFilter;

#[derive(Debug, Clone, Parser)]
#[allow(clippy::struct_excessive_bools)] // CLI flags are bools by nature
#[command(name = "aa-viewer", about = "Anywhere Alternative viewer", version)]
struct Args {
    /// Who to connect to. Leave it out to find the host automatically on
    /// the local network; or give a computer name (partial is fine), an IP,
    /// or ip:port.
    host: Option<String>,

    /// Local UDP address to bind.
    #[arg(long, default_value = "0.0.0.0:0")]
    bind: SocketAddr,

    /// Use the hardware-free mock decoder (pairs with `aa-host --mock`).
    #[arg(long)]
    mock: bool,

    /// With --mock: expect raw pixels instead of H.264 (pairs with host --mock-raw).
    #[arg(long)]
    mock_raw: bool,

    /// No window: receive, decode, print stats once a second.
    #[arg(long)]
    headless: bool,

    /// Open the window borderless-fullscreen on the current monitor.
    #[arg(long)]
    fullscreen: bool,

    /// Fill the window edge to edge even if the aspect ratio differs
    /// (distorts the picture; default letterboxes instead).
    #[arg(long)]
    stretch: bool,

    /// Headless only: send a synthetic mouse wiggle every second.
    #[arg(long)]
    test_input: bool,

    /// With --mock: copy a test text every 2 s to exercise clipboard sharing.
    #[arg(long)]
    test_clipboard: bool,

    /// Send this machine's microphone to the host from the start (it can
    /// also be switched on and off in the settings panel).
    #[arg(long)]
    mic: bool,

    /// With --mock: send a test tone as the microphone.
    #[arg(long)]
    test_mic: bool,

    /// With --mock: a pretend controller that moves and presses buttons.
    #[arg(long)]
    test_gamepad: bool,

    /// Highest bitrate the host may use, in Mbps (also in the settings panel).
    #[arg(long)]
    max_mbps: Option<f32>,

    /// Extra loudness for the other computer's sound, in dB (0-18).
    #[arg(long)]
    volume_boost: Option<f32>,

    /// Silence the other computer's own speakers while connected.
    #[arg(long)]
    mute_host: bool,

    /// Hide the small statistics line in the corner.
    #[arg(long)]
    hide_stats: bool,

    /// With --mock: a pretend `DualSense` (raw pass-through) that logs what
    /// the host's game sends back to it.
    #[arg(long)]
    test_ds5: bool,

    /// Pair with the host first, using the six-digit code from its Share
    /// page (only needed once per pair of computers).
    #[arg(long)]
    pair: Option<String>,
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        // Colours only in a terminal; log files (the Anywhere app) stay plain.
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stdout()))
        .init();
    let args = Args::parse();

    if args.mock && args.test_clipboard {
        start_test_clipboard("viewer");
    }
    let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
    let host: SocketAddr = runtime.block_on(aa_platform::discover::resolve(args.host.as_deref()))?;
    if let Some(code) = &args.pair {
        let p = runtime.block_on(aa_platform::pairing::pair(host, code))?;
        tracing::info!(name = p.name, "paired; from now on this computer connects without a code");
    }
    let (cmd_tx, cmd_rx) = link::command_channel();
    if args.test_gamepad {
        pads::spawn_test(cmd_tx.clone());
    } else {
        pads::spawn(cmd_tx.clone());
    }
    let pad_out = if args.test_ds5 { Some(ds5::spawn_test(cmd_tx.clone())) } else { ds5::spawn(cmd_tx.clone()) };
    let mic = if args.test_mic {
        MicStart::Tone
    } else if args.mic {
        MicStart::Real
    } else {
        MicStart::Off
    };

    if args.headless {
        // Keep the sender alive: a closed command channel means "window closed".
        let result = runtime.block_on(run_reconnecting(&args, host, None, cmd_rx, None, mic, None, pad_out));
        drop(cmd_tx);
        return result;
    }

    let event_loop = window::build_event_loop()?;
    let proxy = event_loop.create_proxy();
    let frames = link::FrameSlot::new(move || {
        let _ = proxy.send_event(window::Wake::Frame);
    });

    let (stats_tx, stats_rx) = std::sync::mpsc::channel();

    // Session on its own thread; it reconnects by itself if the host goes
    // quiet, and closes the window only when it truly ends.
    let session_frames = frames.clone();
    let session_proxy = event_loop.create_proxy();
    let status_proxy = std::sync::Mutex::new(event_loop.create_proxy());
    let session_args = args.clone();
    std::thread::Builder::new().name("aa-session".into()).spawn(move || {
        let status = move |s: window::Wake| {
            let _ = status_proxy.lock().expect("proxy").send_event(s);
        };
        let result = runtime.block_on(run_reconnecting(
            &session_args,
            host,
            Some(session_frames),
            cmd_rx,
            Some(stats_tx),
            mic,
            Some(std::sync::Arc::new(status)),
            pad_out,
        ));
        let reason = match result {
            Ok(()) => {
                tracing::info!("session ended");
                "session ended".to_string()
            }
            Err(e) => {
                tracing::error!("session failed: {e:#}");
                format!("session failed: {e:#}")
            }
        };
        let _ = session_proxy.send_event(window::Wake::SessionEnded(reason));
    })?;

    let mut app = window::App::new(format!("Anywhere — {host}"), args.fullscreen, args.stretch, frames, cmd_tx);
    app.set_mic_shown(args.mic);
    app.set_preferences(args.max_mbps, args.volume_boost, args.mute_host, !args.hide_stats);
    app.set_stats_receiver(stats_rx);
    event_loop.run_app(&mut app)?;
    Ok(())
}

/// Without a window (tests), how long we keep trying to get back to a host
/// that went quiet. With a window we keep trying until it is closed: the
/// other computer may be restarting after a power cut, or the Wi-Fi coming
/// back, and the person expects the picture to return by itself.
const RECONNECT_HEADLESS: std::time::Duration = std::time::Duration::from_secs(120);

fn backends(args: &Args) -> anyhow::Result<aa_platform::ViewerBackends> {
    if args.mock {
        Ok(aa_platform::mock::viewer_backends(args.mock_raw)?)
    } else {
        aa_platform::viewer_backends().context("real viewer backends unavailable; try --mock")
    }
}

/// Errors that mean "the host went away" rather than "the host said no":
/// silence, or the OS telling us nothing listens there any more (the host
/// program quit, the PC rebooted) or the network itself vanished.
fn is_host_lost(e: &anyhow::Error) -> bool {
    use std::io::ErrorKind as K;
    e.downcast_ref::<session::HostLost>().is_some()
        || e.downcast_ref::<std::io::Error>().is_some_and(|io| {
            matches!(
                io.kind(),
                K::ConnectionRefused | K::ConnectionReset | K::NetworkUnreachable | K::HostUnreachable | K::NetworkDown
            )
        })
}

type StatusFn = std::sync::Arc<dyn Fn(window::Wake) + Send + Sync>;

/// Run sessions until the user quits. If the host goes quiet (asleep,
/// crashed, Wi-Fi blip) we look for it again (it may have a new address)
/// and reconnect, for up to two minutes, instead of freezing on the last
/// picture.
#[allow(clippy::too_many_arguments)]
async fn run_reconnecting(
    args: &Args,
    first_host: SocketAddr,
    frames: Option<link::FrameSlot>,
    mut commands: tokio::sync::mpsc::Receiver<link::ViewerCommand>,
    stats_tx: Option<std::sync::mpsc::Sender<overlay::LiveStats>>,
    mic: MicStart,
    status: Option<StatusFn>,
    pad_out: Option<std::sync::mpsc::Sender<aa_core::ds5::PadMsg>>,
) -> anyhow::Result<()> {
    let mut host = first_host;
    let mut lost_since: Option<std::time::Instant> = None;
    let host_key: std::sync::Arc<std::sync::Mutex<Option<aa_core::secure::PublicKeyBytes>>> = std::sync::Arc::default();
    let mut attempt = 0usize;
    loop {
        let on_connected: Option<Box<dyn Fn() + Send>> =
            status.clone().map(|s| Box::new(move || s(window::Wake::Connected)) as Box<dyn Fn() + Send>);
        let opts = session::Options {
            host,
            bind: args.bind,
            frames: frames.clone(),
            test_input: args.test_input,
            stats_tx: stats_tx.clone(),
            mic,
            pad_out: pad_out.clone(),
            on_connected,
            on_status: status.clone().map(|s| {
                Box::new(move |m: Option<String>| s(window::Wake::HostStatus(m))) as Box<dyn Fn(Option<String>) + Send>
            }),
            host_key_out: Some(std::sync::Arc::clone(&host_key)),
        };
        let connected = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = std::sync::Arc::clone(&connected);
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
        let result = session::run(opts, backends(args)?, &mut commands).await;
        if connected.load(std::sync::atomic::Ordering::Relaxed) {
            // We were back in business; a later loss gets a fresh two minutes.
            lost_since = None;
        }
        let Err(e) = result else { return Ok(()) };
        let lost = is_host_lost(&e);
        let retrying = lost_since.is_some();
        // Before the first connection, errors are reported, not retried
        // (wrong address, host refused us).
        if !lost && !retrying {
            return Err(e);
        }
        if e.downcast_ref::<net::NotPaired>().is_some() {
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
            s(window::Wake::Reconnecting);
        }
        // Back off gently (1 s, then 2 s, at most 5 s) so a long outage
        // doesn't flood the network, but a blip is over in a second.
        attempt += 1;
        tokio::time::sleep(std::time::Duration::from_secs(attempt.min(5) as u64)).await;
        let key = *host_key.lock().expect("key");
        host = next_address(args, host, key, attempt).await;
    }
}

/// Where to try next: the same computer wherever it answers now (found by
/// its key: new home address, or through Tailscale), else each address it
/// was ever seen at in turn, else the one we had.
async fn next_address(
    args: &Args,
    current: SocketAddr,
    key: Option<aa_core::secure::PublicKeyBytes>,
    attempt: usize,
) -> SocketAddr {
    let Some(key) = key else {
        // Never connected yet: the old behaviour (name or discovery).
        return aa_platform::discover::resolve(args.host.as_deref()).await.unwrap_or(current);
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

/// `--test-clipboard`: pretend the user copies a new line of text every
/// 2 s, so a mock session exercises clipboard sharing end to end.
fn start_test_clipboard(side: &'static str) {
    std::thread::spawn(move || {
        let mut n = 0u32;
        let big = std::env::var("AA_TEST_CLIPBOARD_BYTES").is_ok();
        loop {
            // Big items get time to arrive before the next copy replaces them.
            // AA_TEST_CLIPBOARD_MS overrides the interval (clipboard storms).
            let ms = std::env::var("AA_TEST_CLIPBOARD_MS").ok().and_then(|v| v.parse().ok());
            std::thread::sleep(std::time::Duration::from_millis(ms.unwrap_or(if big { 6000 } else { 2000 })));
            n = n.wrapping_add(1);
            // AA_TEST_CLIPBOARD_BYTES=N: copy an N-byte "image" instead of
            // text, to test big transfers.
            let item = match std::env::var("AA_TEST_CLIPBOARD_BYTES").ok().and_then(|v| v.parse::<usize>().ok()) {
                Some(len) => {
                    aa_platform::clipboard::ClipItem::Png(vec![u8::try_from(n % 251).unwrap_or(0); len].into())
                }
                None => aa_platform::clipboard::ClipItem::Text(format!("{side} clip {n}")),
            };
            aa_platform::mock::test_clipboard().copy(item);
        }
    });
}
