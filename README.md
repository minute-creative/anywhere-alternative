# Anywhere Alternative

Low-latency remote desktop and game streaming between macOS and Windows,
both directions, built from scratch in Rust. Personal project.

Read [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) first; it is the
reference for every design decision and the stage plan.

## Status

**Stage 1 in progress.** The protocol core, platform abstraction and both
binaries exist and stream end-to-end with the hardware-free mock pipeline,
including a real viewer window (`winit` + `wgpu`, vsync-locked) with
keyboard and mouse forwarding, and real H.264 video via the software
codec (OpenH264). **Windows can host its real screen** (DXGI Desktop
Duplication + `SendInput`, software-encoded for now). macOS capture is next.

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
git clone https://github.com/minute-creative/anywhere-alternative
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

A window opens showing the host's moving colour test pattern; your mouse
and keyboard inside it are sent to the host (the mock host just logs them;
run it with `RUST_LOG=debug` to see).

**In the viewer window**, press **Ctrl+Shift+S** (Windows) or **⌘⇧S** (Mac)
for the settings overlay: fullscreen, stretch-to-fill, bitrate cap, and
live stats. While it is open, keyboard and mouse stay local. `--fullscreen`
and `--stretch` set the initial state from the command line.

Add `--headless` instead to skip the window and print one stats line per
second:

```
stream fps=60 mbps=444.5 rtt_ms=1.57 assembly_ms=3.41 loss=0.00% dropped=0
```

The mock host streams real H.264 (software-encoded). To send raw pixels
instead for pipeline debugging, pass `--mock-raw` to **both** sides.

## Real screen (Windows host)

On the Windows PC:

```powershell
cargo run --release -p aa-host
```

On the viewing machine (Windows or Mac; use the host's LAN IP):

```powershell
cargo run --release -p aa-viewer -- 192.168.x.x:7700
```

Viewing the host from *itself* works for a quick look (you'll see the
infinite-mirror effect) but mouse moves inside the viewer window will move
the real cursor, which fights you. A second machine is the real test.

To measure how fast this machine can encode (software codec for now):

```sh
cargo run --release -p aa-host -- --bench
```

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
