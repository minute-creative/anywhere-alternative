# Anywhere Alternative

Low-latency remote desktop and game streaming between macOS and Windows,
both directions, built from scratch in Rust. Personal project.

Read [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) first; it is the
reference for every design decision and the stage plan.

## Status

**Stage 1 in progress.** The protocol core, platform abstraction and both
binaries exist and stream end-to-end with the hardware-free mock pipeline.
Real macOS capture/encode and the viewer window are the next commits.

## Setup

### macOS (host and viewer)

```sh
xcode-select --install
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
# restart the terminal, then:
cargo --version
```

### Windows (viewer now, host in stage 5)

1. Install **Build Tools for Visual Studio** with the "Desktop development
   with C++" workload.
2. Install Rust from https://rustup.rs (accept defaults).
3. In a new PowerShell: `cargo --version`

### Both

```sh
git clone https://github.com/<your-username>/anywhere-alternative
cd anywhere-alternative
cargo build
```

First build takes a few minutes (it compiles dependencies). Later builds
are seconds.

## Smoke test (works on any OS, no hardware)

Terminal 1:

```sh
cargo run -p aa-host -- --mock
```

Terminal 2:

```sh
cargo run -p aa-viewer -- 127.0.0.1:7700 --mock
```

You should see one line per second like:

```
stream fps=60 mbps=444.5 rtt_ms=1.57 assembly_ms=3.41 loss=0.00% dropped=0
```

The mock codec sends raw pixels (no compression), so the bitrate is huge;
that is expected. It exists to test the pipeline, not to be used.

Across two machines on the same LAN: run the host with
`--listen 0.0.0.0:7700` and give the viewer the host's LAN IP.
If the firewall asks, allow it.

## Development

```sh
cargo fmt
cargo clippy --all-targets   # must be zero warnings
cargo test
RUST_LOG=debug cargo run -p aa-viewer -- <host-ip>:7700
```

## Layout

```
crates/aa-core      protocol, wire format, negotiation (no OS code, fully tested)
crates/aa-platform  capture/encode/decode/input traits + per-OS backends + mock
crates/aa-host      host binary
crates/aa-viewer    viewer binary
docs/               architecture and decisions
```
