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
//!
//! In plain words: this file starts the viewer. It reads the options the
//! Anywhere app passed (which computer, picture size, frame rate, camera and
//! microphone choices), pairs first if a code was given, starts the
//! controller readers, then runs the connection on a background thread and
//! the window on the main thread. If the other computer goes quiet it keeps
//! trying to reconnect (`run_reconnecting`), looking it up again by its key
//! in case its address changed.

use std::net::SocketAddr;

use aa_viewer::reconnect::{self, ConnectPrefs};
use aa_viewer::session::MicStart;
use aa_viewer::{ds5, link, pads, session, window};

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

    /// Picture size: "full" (the other screen's own size), or 1440, 1080
    /// or 720 to save data.
    #[arg(long, default_value = "full")]
    resolution: String,

    /// Frames per second: "auto" (as many as both screens show and the
    /// connection carries, 60 at the least) or a number to hold.
    #[arg(long, default_value = "auto")]
    fps: String,

    /// Camera that calls on the other computer may use ("Anywhere Camera"):
    /// part of its name, "auto" for the first one, or "off".
    #[arg(long, default_value = "auto")]
    camera: String,

    /// Microphone for calls on the other computer ("Anywhere Microphone")
    /// and for the overlay's mic switch: part of its name, "auto" for the
    /// system default, or "off" (calls then get no mic from here).
    #[arg(long, default_value = "auto")]
    call_mic: String,

    /// Send a moving test picture as the camera (tests without a camera).
    #[arg(long)]
    test_camera: bool,
}

/// Turn the camera and microphone choices into what a session needs.
fn call_prefs(args: &Args) -> session::CallPrefs {
    let on = |s: &str| (!s.eq_ignore_ascii_case("off")).then(|| s.to_owned());
    session::CallPrefs { camera: on(&args.camera), test_camera: args.test_camera, mic: on(&args.call_mic) }
}

/// Everything the session needs from the command line.
fn connect_prefs(args: &Args) -> ConnectPrefs {
    ConnectPrefs {
        host: args.host.clone(),
        bind: args.bind,
        mock: args.mock,
        mock_raw: args.mock_raw,
        test_input: args.test_input,
        resolution: args.resolution.clone(),
        fps: args.fps.clone(),
        screen_hz: None,
        calls: call_prefs(args),
    }
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
        // AA_TEST_INPUT: send a mouse move and a key tap every half second,
        // so the compatibility test (scripts/compat) can check that input
        // reaches the host. Only for headless test runs.
        if std::env::var_os("AA_TEST_INPUT").is_some() {
            let tx = cmd_tx.clone();
            std::thread::spawn(move || {
                use aa_core::input::InputEvent;
                for i in 0u16.. {
                    std::thread::sleep(std::time::Duration::from_millis(500));
                    let x = (i % 100) * 600;
                    let events = [
                        InputEvent::MouseMoveAbs { x, y: x },
                        InputEvent::Key { hid_usage: 0x04, pressed: true },
                        InputEvent::Key { hid_usage: 0x04, pressed: false },
                    ];
                    for ev in events {
                        if tx.blocking_send(link::ViewerCommand::Input(ev)).is_err() {
                            return;
                        }
                    }
                }
            });
        }
        // Keep the sender alive: a closed command channel means "window closed".
        let mut cmd_rx = cmd_rx;
        let prefs = connect_prefs(&args);
        let result =
            runtime.block_on(reconnect::run_reconnecting(&prefs, host, None, &mut cmd_rx, None, mic, None, pad_out));
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
    let prefs = connect_prefs(&args);
    std::thread::Builder::new().name("aa-session".into()).spawn(move || {
        let status = move |s: window::Wake| {
            let _ = status_proxy.lock().expect("proxy").send_event(s);
        };
        let started = std::time::Instant::now();
        let mut cmd_rx = cmd_rx;
        let result = runtime.block_on(reconnect::run_reconnecting(
            &prefs,
            host,
            Some(session_frames),
            &mut cmd_rx,
            Some(stats_tx),
            mic,
            Some(std::sync::Arc::new(status)),
            pad_out,
        ));
        reconnect::send_usage(host, started.elapsed(), "desktop");
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
