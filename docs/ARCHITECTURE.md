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

Packet kinds: Video, Audio, Input, Control (JSON, rare), Ack, Nack, Ping,
Pong, VideoFec, Clipboard, ClipboardAck, Mic, Pad (DualSense pass-through,
`aa-core/src/ds5.rs`). Stage 3 wraps every datagram in encryption below this layer.

Input events are hand-packed (a mouse move is 5 bytes). Keys are sent as
USB HID usage IDs so Mac and PC keyboards agree on what a key means.
Gamepad state is a 15-byte snapshot in DualSense layout.

### Audio

System audio rides the same socket as `Kind::Audio` datagrams. Why these
choices (all in `aa-core/src/audio.rs` and `aa-platform/src/audio.rs`):

- **Opus, 48 kHz stereo, 10 ms frames, 256 kbps, complexity 10, fullband,
  `Signal::Music`.** Opus is what every real-time system uses (WebRTC,
  Discord, game streaming): transparent at this bitrate (0.6% of a 40 Mbps
  video stream, so not worth saving) and encodes a frame in well under a millisecond. 10 ms frames
  keep the chain (frame + network + playout buffer) near 30 ms, below the
  point where picture and sound visibly part.
- **Inband FEC on, and used.** Each packet carries a low-rate copy of the
  previous one; on a gap the viewer rebuilds the last missing frame from
  the next packet (`recover_previous`) and only guesses the rest.
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
- **Loudness:** the viewer applies a volume boost (default +6 dB, overlay
  slider 0–18 dB) through a peak limiter: gain drops instantly when a block
  would pass 0.97 full scale and recovers over ~0.5 s, ramped per block.
  PC audio is mastered with headroom, so quiet scenes get louder and loud
  ones never clip. A 5 ms fade-in after every buffer refill removes the
  restart click.
- **Follows the output device:** the player checks once a second whether
  the default output changed (Bluetooth headphones connected, cable pulled)
  or its stream errored, and reopens there. It plays 48 kHz stereo where the
  device allows, else converts to whatever the device wants (Bluetooth
  hands-free is 16/24 kHz mono) in `playout.rs`, which is device-free and
  unit-tested.
- **Microphone (viewer → host):** off by default; overlay checkbox or
  `--mic`. The viewer captures the default input (followed like the
  output), converts to 48 kHz stereo, Opus 96 kbps, `Kind::Mic`. The host
  plays it into a virtual microphone cable (VB-CABLE "CABLE Input" or
  "Steam Streaming Microphone"); apps pick the cable's mic side. Without
  one installed the host logs what to install. `--test-mic` sends a tone.
- **Late audio packets are dropped**, never played out of order, and don't
  count the following packets as lost (same bug as the video loss tracker).
- **Playback:** `cpal` on the default output, Mac and Windows. Linux builds
  decode and count but have no player (ALSA headers aren't in CI).

### Forward error correction (video)

Each frame's slices are followed by `Kind::VideoFec` parity packets: 10%
of the slice count, at least one. Parity *k* is the XOR of data slices
k, k+G, k+2G… (G = parity count), prefixed with the XOR of their lengths so
a short last slice rebuilds to its exact size. Interleaved stripes mean a
burst of up to G consecutive losses hits G different stripes and is still
fully recoverable. The reassembler rebuilds as soon as a stripe is missing
exactly one slice and has its parity. Simulated Wi-Fi (0.5% loss + stalls,
120 fps): dropped frames 110 → 10 over 20 s, stutters halved.

### Colour path (Windows host)

The capture is BGRA (full-range sRGB). The D3D11 video processor converts
it to NV12 with the colour spaces stated explicitly (input
`RGB_FULL_G22_NONE_P709`, output `YCBCR_STUDIO_G22_LEFT_P709`), and the
encoder's output type says BT.709 video range. Header and pixels agree by
construction instead of depending on what the driver does with BGRA. If
the GPU has no video processor the encoder takes BGRA as before (logged as
`encoder colour path`).

### Clipboard

Both ways, text and images (PNG). A worker thread polls the OS change
counter (`GetClipboardSequenceNumber`, `NSPasteboard.changeCount`) 4×/s and
reads only when it moves; after pasting a remote item it records the new
counter so the item does not bounce back. Items travel as `Kind::Clipboard`
pieces (transfer id in `frame_id`), paced at 350 pieces per 100 ms
(~33 Mbps). The receiver answers `ClipboardAck`: empty when whole, or a
list of missing pieces once the flow has paused 150 ms, and only those are
resent. If the receiver is silent the whole transfer repeats (0.6/1/2/4 s).
4 MB: ~1 s direct; arrives whole at 1% loss. Only copies made *after* connecting are shared. Limit 32 MB.
Unencrypted on the LAN until stage 3 adds encryption.

### Mac host (2026-10-08)

`aa-platform/src/macos/`: `capture.rs` (ScreenCaptureKit `SCStream` on the
main display, NV12 video range BT.709, cursor shown, frames only on
change, ≤3840 wide unless `AA_MAC_MAX_WIDTH`), `encoder.rs`
(`VTCompressionSession`, low-latency rate control when available,
real-time, no reordering, keyframes on request, AVCC → Annex-B with
parameter sets before keyframes), `audio.rs` (second, audio-only SCStream,
48 kHz stereo float → 10 ms i16 frames, own process excluded), `input.rs`
(CGEvent with modifier flags on every event, click counting via
`hid_mac::ClickCounter`, drag events, line vs pixel scrolling;
`hid_mac.rs` key table is portable and tested). Keep-awake spawns
`/usr/bin/caffeinate -d -i -u -w <pid>`. The Welcome message now carries
`host_os`; the viewer maps Ctrl↔Cmd by (viewer OS, host OS).
Permissions: Screen Recording (capture + sound) and Accessibility (input)
for whichever app runs `aa-host`.
Controllers (`macos/gamepad.rs`): `IOHIDUserDeviceCreateWithProperties`
with the DualSense descriptor, one per slot; `GamepadState` →
`ds5::state_to_usb_input`; feature reports from `ds5dev::default_feature`;
rumble parsed from output reports. macOS only permits this for root (or
an Apple-granted entitlement), so `sudo ./target/release/aa-host`.
Mic: the viewer's mic plays into "BlackHole" (like VB-CABLE on Windows).

### Viewer decode and presentation (2026-10-08)

Windows viewer: `windows/decoder.rs`, Media Foundation sync decoder MFT
with a D3D11 device manager (DXVA), low-latency mode, NV12 output copied
from the decoder texture through a staging texture; processor mode if the
GPU path fails; OpenH264 last. All CPU decoders now emit tightly packed
NV12 and the window converts in `nv12.wgsl` (BT.709 video range, then
sRGB→linear because the surface is sRGB). Mac VT decoder still emits BGRA.

### Controllers

Viewer (`pads.rs`): `gilrs` reads every controller (DualSense, DualShock,
Xbox, Switch Pro…, USB or Bluetooth), slots 0–3 lowest-free. It sends
`GamepadAttach { slot, kind }` (PlayStation if Sony vendor id / name),
full `Gamepad` snapshots on change at 250 Hz, and `GamepadDetach`. Plug and
unplug notices repeat every 2 s (unplug for 7 s) because input is plain
UDP; the host treats repeats as no-ops. Snapshots, not button events, so a
lost packet can never leave a button stuck.

Host (Windows, `windows/gamepad.rs`): ViGEmBus via `vigem-client`.
PlayStation → virtual DualShock 4 (PlayStation icons in games), anything
else → Xbox 360. Mapping in `padmap.rs` (portable, tested). Slots 0–3 are
the viewer's pads, 4–7 DualSenses in fallback mode (below). Without
ViGEmBus the host offers no gamepad and the viewer drops controller events
with a log line. `--test-gamepad` on a mock viewer drives a pretend pad.

#### DualSense: full pass-through (2026-10-08)

ViGEm cannot be a DualSense, and the DualSense's features (adaptive
triggers, haptics, light bar, player/mic LEDs, touchpad, gyro) live in
bytes a button snapshot drops. So a DualSense is not read through gilrs:

```
Mac: hidapi reads the real pad ──Kind::Pad Input (64-byte USB report)──►  PC: virtual DualSense
     (USB or Bluetooth; BT 0x31       Feature 0x05/0x09/0x20 every 2 s       (USB/IP server in aa-host,
      reports CRC-checked and                                                usbip-win2 attaches it:
      converted to USB layout)    ◄──Output (48-byte report: rumble,        Windows sees a real USB
                                     triggers, lights), sent twice           DualSense, VID 054C PID 0CE6)
     haptics → DualSense sound   ◄──Audio (Opus, speaker pair +
     card (USB) or → rumble (BT)     haptics pair, 10 ms, silence skipped)
```

- `aa-core::ds5`: message format, BT↔USB conversion (CRC-32 seeds 0xA1
  in / 0xA2 out), report → generic pad state, haptics → rumble envelope.
- `aa-platform::usbip`: a small USB/IP server (devlist, import,
  CMD_SUBMIT/UNLINK, control, interrupt, isochronous). Interrupt-IN is held
  until a new report arrives (8 ms keep-alive), like hardware.
  Isochronous completions are paced at 1 packet/ms so the PC's audio
  engine runs at real time. Hostile input tested (`edges.rs`).
- `aa-platform::ds5dev`: the virtual pad. Descriptors match the real one
  (report sizes checked by a test that parses the report descriptor like
  Windows does). Composite like the real pad: interfaces 0–2 USB Audio
  Class 1 (4-ch 48 kHz speaker+haptics out on EP1, 2-ch mic in on EP2),
  3 HID (EP 0x84 in / 0x03 out). `AA_DS5_NO_AUDIO=1` drops the sound card
  half if a PC's driver dislikes it.
- `aa-platform::padhub`: lifecycle per slot: wait ≤1 s for the real
  feature reports (calibration/serial/firmware) → plug in → mirror →
  unplug on Detach, viewer change, or 10 s silence. If usbip-win2 is
  missing or never connects (6 s), the pad becomes a **generic** DualShock 4
  through ViGEm (slot 4+n) with rumble sent back as a DualSense rumble
  report. Mock host has a loopback "driver + game" (`mock::loopback_attach`).
- Viewer `ds5.rs`: hidapi (exclusive on macOS, so the Mac itself ignores
  the pad; shared if that fails), one thread per pad, 2 ms reads. While it
  holds a DualSense, `pads.rs` (gilrs) hands that pad over.
- Haptics on the Mac need the pad **on a USB cable** (macOS then shows it
  as a 4-channel sound device, `PadSpeaker`); over Bluetooth they are
  imitated with the rumble motors (`haptics_to_rumble`).
- Not done: DualSense Edge identity (presents as a standard DualSense),
  the pad's microphone (silence is sent), the pad's own speaker over BT.

### Codec choice

`Codec::ALL` is preference order (AV1, HEVC, H.264); `negotiate` takes the
first codec both sides list. Each side lists what it can *really* do:
Windows adds HEVC only if a Media Foundation HEVC encoder exists; the Mac
adds HEVC only if `VideoToolbox` opens. The engines are built from factories
after Welcome (`HostBackends::encoder_factory`, `ViewerBackends::decoder`),
and the host swaps encoders on the capture thread via
`PipelineControl::codec`.

### Discovery

Three routes at once, first answer wins (`aa_platform::lan`, viewer
`discover.rs`):
1. **Beacon:** the host sends `Beacon { name, port }` every second on every
   adapter: subnet broadcast (real netmask), all-ones broadcast and multicast
   239.255.77.1, to UDP 7701. The viewer only listens, so the PC firewall's
   handling of *incoming* broadcasts is irrelevant.
2. **Ask:** `Discover` to every adapter's subnet broadcast + all-ones;
   hosts answer `Here`.
3. **Memory:** the last host that accepted us (`last-host` in the user's
   app-data folder) is asked directly, and used outright if nothing answers.
If macOS refuses the sends (Local Network privacy) the error says which
setting to switch on. Test switches on the host: `AA_SIMULATE_FIREWALL`
(ignore `Discover`), `AA_SIMULATE_NO_BEACON`.

## 7. Connectivity (stage 3)

Built 2026-10-08 (0.4.0), Tailscale first; our own server later.

- **Device keys.** Each computer has an X25519 key for connecting
  (`viewer.key`) and one for sharing (`host.key`). No accounts.
- **Pairing once.** The sharing computer shows a six-digit code. SPAKE2
  (password-authenticated key exchange) turns it into a shared secret only
  if both typed the same code; a listener can't test guesses offline, and
  the host takes one new attempt per 2 s and changes the code after five
  wrong ones or one success. Inside that secret the two swap and remember
  each other's public keys.
- **Every connection.** `INIT` (viewer static + one-time key) → `RESP`
  (host static + one-time key + proof). Keys = HKDF over three
  Diffie-Hellmans (one-time×one-time, viewer one-time×host static, viewer
  static×host one-time) and all four public keys. Each side rejects a
  partner it never paired with. Then every datagram is
  `[0xAE][counter u64][ChaCha20-Poly1305]`, 25 bytes extra (1225 still fits
  Tailscale's 1280 MTU), with a 2048-packet replay window. Only `Discover`
  travels in clear, so computers can still be listed by name.
- **Far away.** Tailscale (free, WireGuard underneath) makes every computer
  reachable at a 100.x address through any router. Anywhere asks
  `tailscale status --json` for online peers and asks each `Discover`;
  `Here` carries the host key and all its addresses so a pairing made at
  home also knows the Tailscale address. The viewer finds a lost host
  again by key, wherever it now answers, and keeps trying while its
  window is open.
- **Later, our own server** (signalling, hole-punching, relay) for friends
  without Tailscale. The handshake and sealing above stay as they are.
- **After a power cut (Mac).** A launch agent limited to the Aqua and
  LoginWindow sessions runs `aa-host --service` at the login screen (as
  root) and in the logged-in session; `pmset autorestart 1` turns the Mac
  on when power returns. FileVault's pre-boot screen can't be reached by
  any app.

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
- **Random loss is not congestion.** Treating every lost packet as "link
  full" walked the bitrate to the 2 Mbps floor on 0.5% Wi-Fi loss and kept
  it there: blocky picture for no gain. Light loss now cuts only when RTT
  sits above the session's quietest RTT (a queue is filling); 5%+ loss
  always cuts. Sessions also start at 0.05 bpp and probe +50%/s until the
  first real loss, so a clean LAN is sharp in ~4 s instead of ~30.
- **macOS drops key-up while Cmd is held.** Cmd+C reached the PC as Ctrl
  down + C down, and C never came up (stuck/repeating). While Cmd is held
  the viewer now sends every other key as an immediate press+release.
- **Big transfers need selective repeat.** Repeating a whole 4 MB
  clipboard image until one copy is flawless never finishes at 1% loss
  (~3,400 pieces). Ask for the missing pieces instead.
- **A count that can go down must be subtracted with care.** After late
  packets started returning a loss, the per-second delta underflowed to
  ~2^64; the report claimed 100% loss. Saturating subtraction.
- **The viewer must notice a vanished host.** It used to freeze on the
  last picture forever. Now 4 s of silence, or "connection refused/reset",
  means lost: it rediscovers and reconnects for up to 2 min, re-sending the
  user's settings; the host lets the same machine (same IP, new port) take
  over at once instead of answering "busy".
- **A host nobody sits at must heal itself.** Owner: "when the screen
  freezes or I close the window, the app crashes and gets stuck". Causes
  found: (1) Windows reports a closed viewer as `ConnectionReset` on the
  next receive, and `recv?` ended the host; (2) a resolution or refresh
  change left the encoder at the old size, failing forever; (3) GPU reset
  / sleep-resume left capture retrying a dead device; (4) display sleep
  and PC sleep froze the stream. Now: network errors are logged and
  survived; the capture thread rebuilds the encoder on any size/rate
  change, re-grabs the screen (re-picking the display) on any capture
  error, and rebuilds the whole pipeline on device loss or long failure;
  the PC is kept awake with its display on while streamed; the viewer is
  told (window title) when the screen is locked or off. Frame ids are
  assigned by the capture thread, because each new encoder restarts at 0
  and the viewer drops non-increasing ids (froze after the first rebuild
  in simulation). Mock: `AA_SIMULATE_CAPTURE_FAULTS=1`.
  Still unsolved: the lock screen and UAC prompts can't be captured or
  typed into by a normal program; that needs the host to run as a
  Windows service (SYSTEM) like Parsec/Sunshine.
- **A late packet is not two lost packets.** The loss tracker used to
  move its "expected next" back to a late packet's number, so everything
  after it counted as lost again. On a reordering link (5% of packets
  1.5 ms late, nothing lost) it reported ~10% loss and the controller cut
  bitrate by a quarter. Late packets now cancel one loss and never move the
  counter backwards.
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

### Gaming simulation, 2026-10-07 (mock pipeline, 2-core cloud box)

Synthetic full-motion 1280×720 source, software H.264 both ends, a UDP
relay injecting delay/jitter/loss/stalls. Real hardware encode/decode is
far faster; this tests the transport and pacing, not the GPUs.

| Link | Target | Avg fps | Min fps | Gap p99 | Worst gap | Loss |
|---|---|---|---|---|---|---|
| clean | 60 | 60.0 | 59 | 22 ms | 37 ms | 0.1% |
| clean | 120 | 120.1 | 113 | 16 ms | 31 ms | 0.2% |
| clean | 240 / 300 | 142 / 145 | 100–106 | 13 ms | 31 ms | CPU-bound |
| Wi-Fi (4±2 ms, 0.5% loss, 40 ms stall / 5 s) | 60 | 58.6 | 57 | 27 ms | 53 ms | 0.6% |
| same | 120 | 117.4 | 111 | 18 ms | 44 ms | 0.8% |
| bad Wi-Fi (8±6 ms, 2% loss, 80 ms stall / 2 s) | 120 | 111.6 | 103 | 24 ms | 84 ms | 2.4% |

Controller + FEC rerun (same Wi-Fi link, 120 fps, 20 s): bitrate climbs
to the 80 Mbps cap instead of collapsing to 2 Mbps; dropped frames 110 → 10;
p99 frame gap 22 → 15 ms. Congested link (6 Mbps bottleneck, 80 ms queue):
backs off to ~2–3 Mbps and holds.

Ceilings found: capture fps = host display refresh (DXGI duplication);
viewer shows at most its display refresh (Fifo vsync).

### Full use-case test pass (mock pipeline)

Run: 2026-10-07T19:10Z

## Video under network conditions (1280×720, software codec, 2-core box)

```
m_clean_60             fps avg   60.0 min   59 | gap p50  16.6 p99  23.1 worst   44.9 ms | stutters/s  0.5 | loss 0.00% | rtt   0.5 | mbps   3.8
m_clean_120            fps avg  121.5 min  119 | gap p50   8.1 p99  14.1 worst   29.4 ms | stutters/s  0.5 | loss 0.00% | rtt   0.3 | mbps   7.6
m_clean_240            fps avg  145.8 min  124 | gap p50   6.6 p99  13.1 worst   78.2 ms | stutters/s 23.3 | loss 0.00% | rtt   0.2 | mbps   9.1
m_wifi_60              fps avg   60.0 min   60 | gap p50  16.6 p99  22.8 worst   51.0 ms | stutters/s  0.4 | loss 0.72% | rtt  11.0 | mbps   3.7
m_wifi_120             fps avg  124.5 min  109 | gap p50   8.3 p99  16.6 worst   39.5 ms | stutters/s  1.1 | loss 0.70% | rtt  11.0 | mbps   7.7
m_badwifi_120          fps avg  124.2 min  114 | gap p50   7.6 p99  20.4 worst   90.9 ms | stutters/s  4.3 | loss 2.17% | rtt  22.6 | mbps   7.6
m_congested6M_60       fps avg   60.0 min   60 | gap p50  16.3 p99  27.1 worst   62.2 ms | stutters/s  0.9 | loss 0.00% | rtt  11.8 | mbps   3.3
m_reorder5_120         fps avg  124.9 min  108 | gap p50   8.0 p99  14.1 worst   23.3 ms | stutters/s  0.8 | loss 0.03% | rtt  11.1 | mbps   7.8
m_freeze500ms_60       fps avg   59.7 min   56 | gap p50  15.5 p99  23.7 worst  518.6 ms | stutters/s  0.5 | loss 0.00% | rtt  41.9 | mbps   3.7
```

- m_wifi_120: dropped=12 fec_fixed=74
- m_badwifi_120: dropped=78 fec_fixed=220
- m_freeze500ms_60: dropped=56 fec_fixed=0

## Use cases

| Case | Result | Detail |
|---|---|---|
| Discovery: normal | ✅ pass | found (how="answered"), connected |
| Discovery: firewall | ✅ pass | found (how="announced"), connected |
| Discovery: nobeacon | ✅ pass | found (how="answered"), connected |
| Discovery: everything blocked | ✅ pass | connected via remembered address |
| Clipboard text, both ways, 2% loss | ✅ pass | Mac→PC 6/6, PC→Mac 6/6 (last may be in flight) |
| Clipboard 4 MB image, 1% loss | ✅ pass | 3/3 whole after the selective-repeat fix (~1 s direct) |
| Microphone Mac→PC, 1% loss | ✅ pass | frames=992 concealed=10 (≈100 frames/s) |
| Controller (DualSense) 1% loss | ✅ pass | plugged once as PlayStation, 2251 updates (≈250/s) |
| Mouse input reaches host | ✅ pass | count=1 |
| PC program restarted mid-session | ✅ pass | Mac reconnected by itself, stream fps=60 |
| Mac app restarted (crash) and reconnects | ✅ pass | host let the same machine take over at once |
| Second computer while one is connected | ✅ pass | politely refused: host busy |
| Soak 90 s (video 120 fps + mic + pad + clipboard, Wi-Fi) | ✅ pass | memory growth host 0 MB, viewer 0 MB between 20 s and 90 s |
    soak                   fps avg  120.5 min   79 | gap p50   8.5 p99  15.3 worst   77.7 ms | stutters/s  0.8 | loss 0.51% | rtt  11.4 | mbps   7.5


### Failure patterns (from fuzzing and chaos runs, 2026-10-08)

Every bug the hostile tests found fits one of these. Check new code
against the list; each has a regression test.

1. **One-shot message over UDP.** Anything that must arrive (Hello,
   controller plug/unplug) is repeated until answered, and the receiver
   treats repeats as no-ops. *Found:* a single lost Hello failed the whole
   connection at 30% loss (now resent every 300 ms for 6 s; host re-answers
   repeats without restarting the stream).
2. **Numbers that wrap or restart.** Sequence numbers, frame ids and
   transfer ids are compared by distance (`is_newer`, TCP-style), start at
   random where a restart could collide, and "already seen" memories
   expire. *Found:* clipboard ids restarting at 1 after a reconnect could be
   mistaken for repeats and dropped; frame ids would stop after a wrap.
3. **State a peer can create without limit.** Cap everything keyed by
   incoming data. *Found:* unfinished clipboard transfers piled up without
   bound (13,678 in one fuzz run; now max 2).
4. **Trusting driver values.** Sample rates, channel counts and buffer
   sizes are range-checked. *Found:* a "1 Hz" microphone produced 100k
   frames from 2 samples; odd-channel buffers left samples unwritten (noise).
5. **Transient error treated as fatal.** Long-running loops log and carry
   on; only the user (or "host said bye") ends a session. *Found:* closed
   viewer → `ConnectionReset` → host exited; junk during the handshake
   aborted it.
6. **Facts captured once at startup.** Resolution, refresh rate, display
   and GPU device are re-read and rebuilt on change. *Found:* encoder stuck
   at the old size forever; frame ids restarting with each new encoder.
7. **Subtracting counters that can go down.** Saturating arithmetic.
   *Found:* loss delta wrapped to ~2^64 and reported 100% loss.
8. **Takeover rules.** A *live* viewer is never replaced; only a silent one
   (1.5 s) from the same computer. *Found:* two viewer apps on one Mac
   stole the stream from each other forever.

Test assets: `crates/aa-core/tests/fuzz.rs` (`AA_FUZZ_ROUNDS` for longer
soaks), `crates/aa-platform/tests/edges.rs`, `tools/sim/matrix.sh` (14 use
cases), `tools/sim/chaos.sh` (12 attacks: garbage floods, corruption,
duplication, 30% loss, blackout, crash loops, clipboard storm, soak,
DualSense on a lossy link, DualSense without the PC driver).
Not covered here: Mac/Windows-only code paths (capture, encoders, VT,
ViGEm, cpal devices, hidapi, usbip-win2 itself); those need the owner's
machines or CI.

### DualSense pass-through, simulated (2026-10-08)

Mock host with a loopback stand-in for usbip-win2 plus a pretend game;
`aa-viewer --test-ds5` as the controller.
- Clean link: 243 input reports/s reach the game (all distinct); rumble +
  adaptive-trigger reports come back ≤1 frame later, each sent twice.
- Haptics: 4-channel USB audio from the game → ~180 B Opus per 10 ms
  (~145 kbit/s) only while non-silent; real-time paced completions
  (40 ms of audio completes in ~40 ms).
- 5% loss + 30 ms jitter: 188 reports/s, effects and haptics still
  arrive; when the viewer vanished the virtual pad was unplugged.
- Driver missing (`AA_SIMULATE_NO_USBIP=1`): generic PlayStation pad in
  slot 4 within ~0 s of the failure; game rumble reaches the controller.
- Pending real hardware: usbip-win2 attach on the PC, Windows/Steam
  recognising the pad, haptics through Windows' USB audio driver, hidapi
  on the Mac (USB and Bluetooth).

## 11. Coding standards

- `cargo fmt`, `cargo clippy --all-targets` with pedantic lints: zero warnings.
- `cargo test` green on Linux CI for every commit.
- `unsafe` only in `aa-platform`, each block with a `// SAFETY:` comment.
- Hot path (anything per-frame or per-input-event): no allocation, no
  locks, no `format!`, no logging above `trace`.
- Every backend has a `probe()` that reports what the hardware can do and a
  clear error when a permission is missing (Screen Recording, Accessibility).
- Public items have doc comments explaining *why*, not just *what*.
