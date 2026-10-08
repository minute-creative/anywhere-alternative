# Anywhere Alternative

Low-latency remote desktop and game streaming between macOS and Windows,
both directions, built from scratch in Rust. Personal project.

Read [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) first; it is the
reference for every design decision and the stage plan.

## Install (no building needed)

Download the latest version from the
[Releases page](https://github.com/minute-creative/anywhere-alternative/releases/latest):

- **Mac (Apple Silicon):** `Anywhere-x.y.z-mac.dmg`. Open it and drag
  Anywhere to Applications. The app isn't signed with a paid Apple
  certificate, so the first time macOS refuses to open it: open System
  Settings → Privacy & Security, scroll down and click **Open Anyway**.
  When you start sharing, allow **Screen & System Audio Recording** and
  **Accessibility** for Anywhere when asked.
- **Windows:** `Anywhere-x.y.z-windows-setup.exe`. If SmartScreen warns,
  click **More info → Run anyway**. The installer also opens the firewall
  for Anywhere so other computers can reach it.

Open **Anywhere**: computers that are sharing show up by name; click
Connect. To let others in, click **Start sharing**. The "Optional extras"
list shows the free drivers for controllers and microphone, with a button
to get each.

### Making a new release

Bump `version` in `Cargo.toml`, then push a tag:

```sh
git tag v0.2.0 && git push origin v0.2.0
```

GitHub builds the Mac disk image and the Windows installer (about 20
minutes) and publishes them on the Releases page
(`.github/workflows/release.yml`).

## Status

**Stage 2 in progress.** Windows hosts its real screen (DXGI Desktop
Duplication → hardware H.264/HEVC via Media Foundation, zero-copy) with
keyboard and mouse forwarding, adaptive bitrate, keyframe recovery after
loss, an in-window settings overlay (Ctrl/Cmd+Shift+S), **system audio**
(process-loopback → Opus → adaptive jitter buffer, with a "mute PC
speakers" toggle), **LAN auto-discovery** (no IP to type) and
**VideoToolbox hardware decode** on the Mac. Verified Windows→Mac over
Wi-Fi at 60 fps with audio. Microphone, zero-copy decode, macOS hosting and
controllers are next. Picking this up fresh? Read `docs/HANDOFF.md`.

## Setup

### macOS (host and viewer)

```sh
xcode-select --install
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
# restart the terminal, then:
cargo --version
```

CMake is needed to build the Opus audio codec. With Homebrew:
`brew install cmake`. Without it, install the `.dmg` from
https://cmake.org/download/ and run
`sudo "/Applications/CMake.app/Contents/bin/cmake-gui" --install`.

### Windows (host and viewer)

1. Install **Build Tools for Visual Studio** with the "Desktop development
   with C++" workload.
2. Install Rust from https://rustup.rs (accept defaults).
3. Install CMake (builds the Opus audio codec):
   `winget install --id Kitware.CMake -e`
4. In a new PowerShell: `cargo --version` and `cmake --version`

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

On the viewing machine (Windows or Mac). With no address the viewer finds
the host on the local network by itself:

```powershell
cargo run --release -p aa-viewer -- --fullscreen
```

If several hosts answer, pass part of the PC's name (`-- maitrik-pc`), or
an address (`-- 192.168.x.x` or `192.168.x.x:7700`) to skip discovery.

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
