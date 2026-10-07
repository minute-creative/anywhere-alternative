# Anywhere Alternative — Architecture

Low-latency remote desktop and game streaming between macOS and Windows,
in both directions, for personal use by a small group of friends.

This document is the shared reference. When a decision here changes, change
it here first, then in code.

## 1. Goals and non-goals

**Goals**

- Feels local on LAN: our own overhead (capture → encode → send → decode →
  present) under 10 ms, so total latency ≈ network RTT + one display frame.
- Two hard performance targets:
  - **4K at a locked 60 fps**: every frame on exactly one display refresh,
    never doubled or skipped. A software commitment; frame pacing in the
    presenter is a stage-1 requirement.
  - **1080p at 240 fps**: requires a 240 Hz display on *both* ends and a host
    encoder that sustains 240 frames/s (~4 ms per frame). NVENC/AMF/QuickSync
    do; Apple VideoToolbox must be benchmarked per chip (`aa-host bench`,
    stage 1). Everything per-frame is budgeted for 4 ms from the start.
- Steam and CrossOver games playable from a Mac host with a PS controller.
- After Effects / design work usable at full resolution and colour fidelity.
- Works on every GPU we and our friends have: Apple Silicon, NVIDIA, AMD,
  Intel Arc / Core Ultra.
- Works over the internet from anywhere, without opening router ports.

**Non-goals (for now)**

- Multi-user sessions, accounts, billing, enterprise anything.
- Linux (the core compiles there for CI only).
- Mobile viewers.

## 2. The physics we cannot beat

| Leg                              | Typical          | Who controls it |
|----------------------------------|------------------|-----------------|
| Capture (compositor → us)        | 0.5–2 ms         | OS              |
| Encode (hardware, low-latency)   | 2–5 ms           | us + GPU vendor |
| Network, same LAN                | 1–3 ms           | nobody          |
| Network, same city               | 5–20 ms          | nobody          |
| Network, India ↔ Europe/US       | 120–250 ms       | nobody          |
| Decode (hardware)                | 2–5 ms           | us + GPU vendor |
| Present (one display frame)      | 4 ms @ 240 Hz … 16 ms @ 60 Hz | display |

"Zero latency" over the internet is not a software problem. Our job is to
make every leg we control as small as it can be and to never add buffering.

Upload bandwidth at the host is the other hard limit. Rough needs (HEVC):

| Resolution @ fps | Bitrate for games | For design/text |
|------------------|-------------------|-----------------|
| 1080p60          | 10–20 Mbps        | 15–30 Mbps      |
| 1440p120         | 30–50 Mbps        | 40–70 Mbps      |
| 4K60             | 40–60 Mbps        | 60–100 Mbps     |

## 3. System shape

```text
┌───────────────────── host machine ─────────────────────┐
│  capture ─► encode ─► slice ─► UDP ─────────────────────┼──┐
│     ▲          ▲                                        │  │
│  display    bitrate ctl ◄── acks/nacks ◄── UDP ◄────────┼──┼──┐
│  input injection ◄── input events ◄──────── UDP ◄───────┼──┼──┤
└─────────────────────────────────────────────────────────┘  │  │
                                                             │  │
          signalling server (stage 3): pairing, STUN, relay  │  │
                                                             ▼  │
┌───────────────────── viewer machine ───────────────────┐     │
│  UDP ─► reassemble ─► decode ─► present (wgpu)          │     │
│  keyboard / mouse / gamepad ─► encode ─► UDP ───────────┼─────┘
└─────────────────────────────────────────────────────────┘
```

Three threads on each side, by design:

- **Network** (async, tokio): owns the socket, never blocks.
- **Capture/encode** (host) or **decode/present** (viewer): blocking OS and
  GPU calls on a dedicated OS thread at elevated priority.
- **Input**: its own thread so a mouse move is never queued behind a frame.

Queues between threads have capacity 1–2 and drop on overflow. A late frame
is worth nothing; we would rather skip it than show it late.

## 4. Crates

| Crate         | What it owns                                          | OS code? |
|---------------|-------------------------------------------------------|----------|
| `aa-core`     | Protocol, wire format, negotiation, stats, config     | **none** (`#![forbid(unsafe_code)]`) |
| `aa-platform` | Traits + one backend module per OS/vendor; mock       | yes      |
| `aa-host`     | The host binary: session loop + pipeline threads      | no       |
| `aa-viewer`   | The viewer binary: session loop + decode + window     | no       |
| `aa-signal`   | (stage 3) tiny server: pairing, STUN, relay           | no       |

Rule: `aa-core` is unit-tested and runs in CI on Linux. If a change there
needs `#[cfg(target_os)]`, it belongs in `aa-platform` behind a trait.

### Platform traits

```rust
trait ScreenCapture  { fn next_frame(&mut self, timeout) -> Option<CapturedFrame>; … }
trait VideoEncoder   { fn encode(&mut self, &CapturedFrame, force_keyframe) -> EncodedFrame;
                       fn set_bitrate_kbps(&mut self, u32); fn request_intra_refresh(&mut self); }
trait VideoDecoder   { fn decode(&mut self, frame_id, &Bytes) -> Option<DecodedFrame>; }
trait InputInjector  { fn inject(&mut self, InputEvent); }
trait VirtualGamepad { fn update(&mut self, slot, GamepadState); fn poll_rumble(&mut self) -> Option<Rumble>; }
```

Backends:

| Trait          | macOS                          | Windows                                      |
|----------------|--------------------------------|----------------------------------------------|
| Capture        | ScreenCaptureKit               | DXGI Desktop Duplication                     |
| Encode         | VideoToolbox                   | NVENC → AMF → QuickSync (oneVPL) → Media Foundation, first that opens wins |
| Decode         | VideoToolbox                   | Media Foundation / D3D11 Video               |
| Input          | CGEvent                        | SendInput                                    |
| Virtual gamepad| IOKit user-space HID (spike); DriverKit fallback | ViGEm (virtual DualShock 4 or Xbox 360) |

The `mock` backend implements all five with no hardware so the whole
pipeline runs anywhere. `aa-host --mock` + `aa-viewer --mock` is the smoke test.

## 5. Video pipeline decisions

- **Zero-copy GPU path.** Capture hands an `IOSurface` / `ID3D11Texture2D`
  straight to the encoder. The CPU never touches pixels. `FrameBuffer::Gpu`
  carries the handle; `FrameBuffer::Cpu` exists only for mock and debugging.
- **Codec preference:** AV1 > HEVC > H.264, negotiated from what both sides
  do in hardware. Software codecs are never offered.
- **Encoder settings** (every backend): real-time mode, no B-frames, periodic
  intra-refresh instead of keyframes, CBR-ish with a hard cap, slice output
  as soon as available where the API allows.
- **Loss recovery:** viewer NACKs a frame it gave up on → host starts an
  intra-refresh cycle. No retransmission of video; by the time a retransmit
  arrives the frame is stale.
- **Frame rate is the viewer's display refresh rate, locked.** The viewer
  reports its refresh rate in the handshake; the host captures at exactly
  that rate (both capture APIs accept a frame interval), so no frame is
  encoded that nobody will see. If the host display is slower than the
  viewer, the stream runs at the host's rate and the viewer shows each
  frame for a whole number of refreshes (60 fps on a 240 Hz screen = 4
  refreshes per frame, no judder). A monitor or mode change on either
  side renegotiates mid-session.
- **No jitter buffer.** A frame is decoded the instant its last slice
  arrives and presented at the next vsync. Smoothness comes from frame
  pacing in the presenter (stage 1, second commit), not from buffering.
- **Adaptive bitrate** (stage 2): drive from loss ratio and frame-assembly
  time; drop bitrate fast, raise it slowly, never above the user's cap.
- **Static screen = zero bandwidth.** Capture returns `None` when nothing
  changed; we send nothing.
- **Profiles:** `Speed` (games), `Balanced`, `Quality` (4:4:4 chroma where
  the codec allows; for text and design work).

## 6. Wire format

UDP only. Every datagram ≤ 1200 bytes with a 12-byte header
(`kind, flags, seq, frame_id, slice_index, slice_count`). See
`aa-core/src/wire.rs` for the exact layout and the reassembler's policy.

Packet kinds: Video, Audio (stage 2), Input, Control (JSON, rare), Ack,
Nack, Ping, Pong. Stage 3 wraps every datagram in encryption below this layer.

Input events are hand-packed (a mouse move is 5 bytes). Keys are sent as
USB HID usage IDs so Mac and PC keyboards agree on what a key means.
Gamepad state is a 15-byte snapshot in DualSense layout.

### Audio

System audio rides the same socket as `Kind::Audio` datagrams. Why these
choices (all in `aa-core/src/audio.rs` and `aa-platform/src/audio.rs`):

- **Opus, 48 kHz stereo, 10 ms frames, 128 kbps.** Opus is what every
  real-time system uses (WebRTC, Discord, game streaming): transparent at
  this bitrate and encodes a frame in well under a millisecond. 10 ms frames
  keep the chain (frame + network + playout buffer) near 30 ms, below the
  point where picture and sound visibly part.
- **Inband FEC on.** Each packet carries a low-rate copy of the previous one,
  so a single Wi-Fi loss costs nothing audible.
- **Packet-loss concealment, not silence.** The player tracks `frame_no`;
  a gap of up to 5 frames is filled by asking Opus to synthesise from what
  came before. Beyond that it was a real pause and we resync.
- **Silence is not sent.** WASAPI flags silent buffers; the host skips them,
  so an idle desktop costs zero audio bandwidth.
- **Jitter buffer ~30 ms that trims itself.** Audio needs *some* buffer (a
  late sample is a click, unlike a late video frame which is just skipped).
  If a network burst delivers more than 80 ms we drop the oldest so latency
  never creeps up over a long session.
- **Capture:** Windows uses WASAPI loopback on the default output (no
  virtual cable or driver). Mac host capture comes with ScreenCaptureKit in
  the Mac-host stage.
- **Playback:** `cpal` on the default output, Mac and Windows. Linux builds
  decode and count but have no player (ALSA headers aren't in CI).

### Codec choice

`Codec::ALL` is preference order (AV1, HEVC, H.264); `negotiate` takes the
first codec both sides list. Each side lists what it can *really* do:
Windows adds HEVC only if a Media Foundation HEVC encoder exists; the Mac
adds HEVC only if `VideoToolbox` opens. The engines are built from factories
after Welcome (`HostBackends::encoder_factory`, `ViewerBackends::decoder`),
and the host swaps encoders on the capture thread via
`PipelineControl::codec`.

### Discovery

`ControlMessage::Discover` is broadcast by the viewer to 255.255.255.255:7700
(and the /24 subnet broadcast); any host answers `Here { name }`. Same
socket and port as streaming, so no extra listener or firewall rule.

## 7. Connectivity (stage 3)

- Each machine has a device key. Pairing = typing a short code once;
  the host keeps an allow-list. No accounts.
- A small signalling server (one cheap VPS, India first) brokers:
  device lookup, ICE-style hole-punching (STUN), and a relay for the
  minority of connections that cannot punch through.
- Direct path is the default. Relay is the exception and the only thing
  that costs bandwidth money.
- Encryption: Noise-style handshake with the device keys, then AEAD on
  every datagram. No plaintext ever leaves the machine, even on LAN.

## 8. Stages and their pass/fail tests

| # | Deliverable | Pass when |
|---|-------------|-----------|
| 1 | Mac host → Windows/Mac viewer, LAN, keyboard + mouse, real window; `bench` command | 1080p60 at < 20 ms glass-to-glass (phone camera at 240 fps); encoder fps measured on each machine |
| 1b | macOS virtual-controller spike | Steam sees a virtual DualSense **or** we know we need DriverKit |
| 2 | DualSense end-to-end + rumble; audio; adaptive bitrate | 30 min of a game without noticing |
| 3 | Pairing, signalling, hole-punch, relay, encryption | works from a phone hotspot |
| 4 | Clipboard, file transfer, multi-monitor, wake-on-LAN | daily-driveable for design work |
| 5 | Windows host: DXGI + NVENC/AMF/QSV/MF, SendInput, ViGEm | stage-1 test passes Win→Mac |
| 6 | HDR, 4K60 locked, 1080p240, batched UDP sends, auto-start daemon/service, installers | 4K60 with zero dropped/doubled frames over 10 min; 1080p240 on 240 Hz hardware both ends; friends can install it unassisted |

## 9. Lessons from real-network testing

Recorded because each one cost a debugging round and is easy to reintroduce.

- **Never send from inside the receive loop.** Anything that takes time
  (pacing, large bursts) starves packet intake; keepalives get lost and the
  session times out. Sending has its own task.
- **One sequence counter per socket.** The viewer's loss tracker assumes a
  single monotonic sequence. Two tasks with independent counters read as
  ~99% loss, and the bitrate controller then pins the stream to the floor.
- **Don't pace frames across the interval on Wi-Fi.** Consecutive frames
  overlap on the air and collide. Bursts were never the problem; bitrate is
  the lever.
- **Gate decoding on keyframes.** After any gap, feeding P-frames to the
  decoder yields an error per frame until the next keyframe anyway.
- **Release held input on focus loss and disconnect.** macOS swallows
  key-up for Cmd during Cmd+Tab; a stuck Ctrl on the host turns every click
  into Ctrl+click.
- **Declare DPI awareness before touching the screen.** Otherwise Windows
  reports virtualised sizes and injected positions land in the wrong place.
- **Endpoint loopback captures after the speaker mute.** On devices with
  software volume, muting the PC also muted the stream. Per-process
  loopback (`ActivateAudioInterfaceAsync`, exclude our own tree) taps
  before volume/mute and is the default; endpoint loopback is the fallback.
- **A fixed audio buffer is wrong on every link.** 30 ms crackled on Wi-Fi;
  300 ms would lag on Ethernet. Grow on underrun, shrink when calm.
- **Addresses change; names don't.** Home routers reassign addresses on
  reconnect. Broadcast discovery on the port we already own costs nothing
  and removes a whole class of "no answer from host".
- **Pick the codec after negotiation, not at startup.** Both ends build
  their encoder/decoder from a factory once Hello/Welcome has decided.
- **windows-rs `PROPVARIANT` frees its payload on drop.** A `VT_BLOB`
  pointing at stack memory → `STATUS_HEAP_CORRUPTION` at exit of scope.
  Wrap in `ManuallyDrop` when the blob is borrowed, not owned.
- **The loaded encoder must be `codecs[0]`.** Windows offered HEVC first
  but had opened H.264; the session took `codecs[0]` as "what is loaded",
  saw HEVC == HEVC, never switched, and sent H.264 labelled HEVC. The Mac
  failed every frame (black screen, `keyframe arrived; resuming` spam).
  Now the host opens HEVC at startup and only offers it if that works; the
  capture thread logs the codec on every connect and sends nothing rather
  than mislabelled video if a switch fails.

## 10. Measured so far

| Date | Machine | Path | Result |
|------|---------|------|--------|
| 2026-10-06 | Windows 11 25H2, Intel Core Ultra, Arc 130T, 2880×1800@60 | DXGI → software H.264 (OpenH264) | 1–10 fps; CPU bench 28 fps @1080p |
| 2026-10-06 | same | DXGI → Quick Sync via Media Foundation, zero-copy | locked 60 fps, 0% loss, ~31 Mbps, <1 ms assembly; bench 110 fps @1080p, 73 @1440p |
| 2026-10-07 | same → Mac mini M4 Pro, Wi-Fi | + WASAPI loopback → Opus 128 kbps | audio plays; crackled with a fixed 30 ms buffer (link ping 36–187 ms) → adaptive buffer shipped |
| 2026-10-07 | same | process-loopback tap, host mute, LAN discovery, VideoToolbox decode, HEVC negotiation | all compile on CI; owner verification pending (see docs/HANDOFF.md) |

## 11. Coding standards

- `cargo fmt`, `cargo clippy --all-targets` with pedantic lints: zero warnings.
- `cargo test` green on Linux CI for every commit.
- `unsafe` only in `aa-platform`, each block with a `// SAFETY:` comment.
- Hot path (anything per-frame or per-input-event): no allocation, no
  locks, no `format!`, no logging above `trace`.
- Every backend has a `probe()` that reports what the hardware can do and a
  clear error when a permission is missing (Screen Recording, Accessibility).
- Public items have doc comments explaining *why*, not just *what*.
