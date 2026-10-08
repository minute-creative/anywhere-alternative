//! `aa-host`: run on the machine you want to reach.
//!
//! One viewer at a time. Viewers find it on the local network (or through
//! Tailscale), must have paired once with the code it shows, and talk to
//! it over an encrypted link (`net.rs`).
//!
//! Threads:
//!
//! ```text
//!  capture thread ──(encoded frames)──► network task ──UDP──► viewer
//!  input thread   ◄──(input events)──── network task ◄──UDP──┘
//! ```
//!
//! Capture/encode and input injection are blocking OS calls, so each gets a
//! dedicated OS thread; the network loop is async on tokio.

mod net;
mod pipeline;
mod session;
mod udp;

use std::net::SocketAddr;

use aa_core::video::Resolution;
use anyhow::Context;
use clap::Parser;
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(name = "aa-host", about = "Anywhere Alternative host", version)]
#[allow(clippy::struct_excessive_bools)] // CLI switches
struct Args {
    /// UDP address to listen on.
    #[arg(long, default_value = "0.0.0.0:7700")]
    listen: SocketAddr,

    /// Use the hardware-free mock pipeline (test pattern, no real capture).
    #[arg(long)]
    mock: bool,

    /// Mock capture resolution, `WxH`.
    #[arg(long, default_value = "640x360", value_parser = parse_resolution)]
    mock_res: Resolution,

    /// Mock capture frame rate.
    #[arg(long, default_value_t = 60)]
    mock_fps: u16,

    /// With --mock: send raw pixels instead of H.264 (pipeline debugging only).
    #[arg(long)]
    mock_raw: bool,

    /// With --mock: copy a test text every 2 s to exercise clipboard sharing.
    #[arg(long)]
    test_clipboard: bool,

    /// Measure encoder throughput on this machine and exit.
    #[arg(long)]
    bench: bool,

    /// Encoder to use for the real screen: auto (hardware, else software),
    /// hardware, or software.
    #[arg(long, default_value = "auto")]
    encoder: String,

    /// Stop cleanly as soon as this file exists (how the Anywhere app stops
    /// a host it started, even one running as administrator).
    #[arg(long)]
    stop_file: Option<std::path::PathBuf>,

    /// Run as the always-on background service (macOS: started by the
    /// system at the login screen and in the logged-in session, see the
    /// app's "share after a restart"). Logs to a file, waits for the port
    /// if the other session's copy still holds it, never gives up.
    #[arg(long)]
    service: bool,
}

fn parse_resolution(s: &str) -> Result<Resolution, String> {
    let (w, h) = s.split_once('x').ok_or_else(|| "expected WxH".to_string())?;
    Ok(Resolution::new(w.parse().map_err(|e| format!("{e}"))?, h.parse().map_err(|e| format!("{e}"))?))
}

/// Encoder throughput at common resolutions. Software only until hardware
/// backends exist; then each backend reports its own line.
fn bench() -> anyhow::Result<()> {
    let sizes = [
        ("1080p", Resolution::new(1920, 1080)),
        ("1440p", Resolution::new(2560, 1440)),
        ("4K", Resolution::new(3840, 2160)),
    ];
    println!("software H.264 (synthetic moving picture):");
    for (name, res) in sizes {
        let fps = aa_platform::sw::bench_encoder(res, 60)?;
        println!("  {name:>6}: {fps:6.1} fps  ({:.2} ms/frame)", 1000.0 / fps);
    }
    #[cfg(target_os = "windows")]
    {
        use aa_core::video::Codec;
        // The PC's own screen first: that is the size that actually streams.
        let native = aa_platform::windows::screen_resolution().ok();
        let mut hw_sizes: Vec<(String, Resolution)> = Vec::new();
        if let Some(r) = native {
            hw_sizes.push((format!("{}x{}", r.width, r.height), r));
        }
        hw_sizes.extend(sizes.iter().map(|(n, r)| ((*n).to_string(), *r)));
        for codec in [Codec::Hevc, Codec::H264] {
            println!("hardware {codec:?} via Media Foundation (every frame changes: worst case):");
            for (name, res) in &hw_sizes {
                match aa_platform::windows::bench_hardware(codec, *res, 240) {
                    Ok((enc, fps)) => {
                        let verdict = if fps >= 300.0 {
                            "300+ ok"
                        } else if fps >= 120.0 {
                            "120 ok"
                        } else if fps >= 60.0 {
                            "60 ok"
                        } else {
                            "below 60"
                        };
                        println!("  {name:>9}: {fps:6.1} fps  ({:.2} ms/frame)  {verdict}  [{enc}]", 1000.0 / fps);
                    }
                    Err(e) => println!("  {name:>9}: unavailable ({e})"),
                }
            }
        }
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn real_host_backends(encoder: &str) -> anyhow::Result<aa_platform::HostBackends> {
    use aa_platform::windows::EncoderChoice;
    let choice = match encoder {
        "auto" => EncoderChoice::Auto,
        "hardware" | "hw" => EncoderChoice::Hardware,
        "software" | "sw" => EncoderChoice::Software,
        other => anyhow::bail!("unknown --encoder {other}; use auto, hardware or software"),
    };
    Ok(aa_platform::windows::host_backends_with(choice)?)
}

#[cfg(not(target_os = "windows"))]
fn real_host_backends(_encoder: &str) -> anyhow::Result<aa_platform::HostBackends> {
    Ok(aa_platform::host_backends()?)
}

/// Is this the copy running at the login screen (as the system)?
fn at_login_screen() -> bool {
    #[cfg(unix)]
    {
        extern "C" {
            fn geteuid() -> u32;
        }
        // SAFETY: geteuid has no arguments and cannot fail.
        unsafe { geteuid() == 0 }
    }
    #[cfg(not(unix))]
    false
}

fn init_logging(args: &Args) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into());
    if args.service {
        let name = if at_login_screen() { "service-login.log" } else { "service.log" };
        let path = aa_platform::trust::host_dir().join(name);
        if let Ok(file) = std::fs::File::create(&path) {
            tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_ansi(false)
                .with_writer(std::sync::Mutex::new(file))
                .init();
            return;
        }
    }
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        // Colours only in a terminal; log files (the Anywhere app) stay plain.
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stdout()))
        .init();
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    init_logging(&args);
    if args.service {
        tracing::info!(login_screen = at_login_screen(), "sharing service starting");
        // Logging in or out: the other session's copy may still hold the
        // port for a moment. Wait for it rather than failing.
        let mut waited = 0;
        while std::net::UdpSocket::bind(args.listen).is_err() {
            if waited == 0 {
                tracing::info!("port {} is busy; waiting for it", args.listen.port());
            }
            waited += 1;
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
    }

    if args.bench {
        return bench();
    }

    let backends = if args.mock {
        tracing::warn!("using MOCK backends: test pattern, no real screen");
        if args.test_clipboard {
            start_test_clipboard("host");
        }
        aa_platform::mock::host_backends(args.mock_res, args.mock_fps, args.mock_raw)?
    } else {
        match real_host_backends(&args.encoder) {
            Ok(b) => b,
            Err(e) if args.service => {
                // Not allowed to see the screen yet (permissions), or nothing
                // to show: try again in a while rather than spinning.
                tracing::error!("cannot share the screen yet: {e:#}; retrying in 30 s");
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                return Err(e).context("real host backends unavailable");
            }
            Err(e) => return Err(e).context("real host backends unavailable; try --mock"),
        }
    };

    if let Some(path) = args.stop_file.clone() {
        let _ = std::fs::remove_file(&path); // a leftover from last time must not stop us
        std::thread::spawn(move || loop {
            if path.exists() {
                let _ = std::fs::remove_file(&path);
                session::request_stop();
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(300));
        });
    }
    session::run(args.listen, backends).await
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
