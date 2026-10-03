//! `aa-host`: run on the machine you want to reach.
//!
//! Stage 1 scope: one viewer at a time, LAN, no encryption, no discovery.
//! You tell the viewer the host's IP. Everything beyond that is a later stage
//! and slots in without changing this crate's shape:
//!
//! * stage 3 wraps the UDP socket in an encrypted transport and swaps the
//!   "listen on a port" for "register with the signalling server";
//! * stage 4 adds audio/clipboard/file channels as more packet kinds.
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

    /// Measure encoder throughput on this machine and exit.
    #[arg(long)]
    bench: bool,
}

fn parse_resolution(s: &str) -> Result<Resolution, String> {
    let (w, h) = s.split_once('x').ok_or_else(|| "expected WxH".to_string())?;
    Ok(Resolution::new(w.parse().map_err(|e| format!("{e}"))?, h.parse().map_err(|e| format!("{e}"))?))
}

/// Encoder throughput at common resolutions. Software only until hardware
/// backends exist; then each backend reports its own line.
fn bench() -> anyhow::Result<()> {
    println!("encoder throughput (software H.264, synthetic moving picture):");
    for (name, res) in [
        ("1080p", Resolution::new(1920, 1080)),
        ("1440p", Resolution::new(2560, 1440)),
        ("4K", Resolution::new(3840, 2160)),
    ] {
        let fps = aa_platform::sw::bench_encoder(res, 60)?;
        println!("  {name:>6}: {fps:6.1} fps  ({:.2} ms/frame)", 1000.0 / fps);
    }
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    let args = Args::parse();

    if args.bench {
        return bench();
    }

    let backends = if args.mock {
        tracing::warn!("using MOCK backends: test pattern, no real screen");
        aa_platform::mock::host_backends(args.mock_res, args.mock_fps, args.mock_raw)?
    } else {
        aa_platform::host_backends().context("real host backends unavailable; try --mock")?
    };

    session::run(args.listen, backends).await
}
