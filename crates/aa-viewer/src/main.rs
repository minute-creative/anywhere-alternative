//! `aa-viewer`: run on the machine you are sitting at.
//!
//! Stage 1 scope: connect to a host by IP, receive, reassemble and decode
//! frames, send keyboard/mouse. Presentation (an actual window drawn with
//! `wgpu`) is the next commit; this one proves the pipeline and prints
//! live statistics once a second so we can measure before we draw.
//!
//! ```text
//!  UDP ──► network task ──(complete frames)──► decode thread ──► [window: next commit]
//!   ▲                                                              │
//!   └──────────────(input events)──────────────────────────────────┘
//! ```

mod session;
mod udp;

use std::net::SocketAddr;

use anyhow::Context;
use clap::Parser;
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(name = "aa-viewer", about = "Anywhere Alternative viewer", version)]
struct Args {
    /// Host address, e.g. 192.168.1.20:7700
    host: SocketAddr,

    /// Local UDP address to bind.
    #[arg(long, default_value = "0.0.0.0:0")]
    bind: SocketAddr,

    /// Use the hardware-free mock decoder (pairs with `aa-host --mock`).
    #[arg(long)]
    mock: bool,

    /// Send a synthetic mouse wiggle every second to exercise the input path.
    #[arg(long)]
    test_input: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    let args = Args::parse();

    let backends = if args.mock {
        tracing::warn!("using MOCK decoder");
        aa_platform::mock::viewer_backends()
    } else {
        aa_platform::viewer_backends().context("real viewer backends unavailable; try --mock")?
    };

    session::run(args.host, args.bind, backends, args.test_input).await
}
