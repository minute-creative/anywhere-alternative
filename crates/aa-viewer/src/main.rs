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
mod discover;
mod keymap;
mod link;
mod overlay;
mod session;
mod udp;
mod window;

use std::net::SocketAddr;

use anyhow::Context;
use clap::Parser;
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
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
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    let args = Args::parse();

    let backends = if args.mock {
        tracing::warn!("using MOCK decoder");
        aa_platform::mock::viewer_backends(args.mock_raw)?
    } else {
        aa_platform::viewer_backends().context("real viewer backends unavailable; try --mock")?
    };

    let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
    let host: SocketAddr = runtime.block_on(discover::resolve(args.host.as_deref()))?;
    let (cmd_tx, cmd_rx) = link::command_channel();

    if args.headless {
        // Keep the sender alive: a closed command channel means "window closed".
        let result = runtime.block_on(session::run(host, args.bind, backends, None, cmd_rx, args.test_input, None));
        drop(cmd_tx);
        return result;
    }

    let event_loop = window::build_event_loop()?;
    let proxy = event_loop.create_proxy();
    let frames = link::FrameSlot::new(move || {
        let _ = proxy.send_event(window::Wake::Frame);
    });

    let (stats_tx, stats_rx) = std::sync::mpsc::channel();

    // Session on its own thread; if it ends (host gone, error), close the window.
    let session_frames = frames.clone();
    let session_proxy = event_loop.create_proxy();
    let bind = args.bind;
    std::thread::Builder::new().name("aa-session".into()).spawn(move || {
        let result =
            runtime.block_on(session::run(host, bind, backends, Some(session_frames), cmd_rx, false, Some(stats_tx)));
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
    app.set_stats_receiver(stats_rx);
    event_loop.run_app(&mut app)?;
    Ok(())
}
