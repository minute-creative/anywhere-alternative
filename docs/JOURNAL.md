# Build journal

A chronological record of what we did, what broke, and what we learned.
`docs/HANDOFF.md` says where things stand; this says how they got there,
so nobody re-tries a dead end. Newest at the bottom.

Legend: ✅ worked · ❌ failed/abandoned · 🔁 replaced · 📏 measured

---

## Day 0 — decisions (2026-10-03)

**Goal set by the owner:** a Parsec-class remote desktop + game streaming
tool between a Mac and a Windows PC, both directions, built from scratch
(Parsec is paid; Moonlight/Sunshine was rejected as a fork), for personal
use and friends. Targets: as close to 0 ms as physics allows; 4K60 locked
and native 1080p240; stream rate locks to the viewer's refresh (host's if
slower); every GPU family; Steam/CrossOver games from a Mac host; After
Effects and browser work; DualSense controllers; audio, clipboard, file
transfer, multi-monitor, WoL; internet with pairing codes and a small
relay later.

**Choices made and why**
- ✅ **Rust.** Memory-safe systems language with first-class bindings to
  every OS media API we need; one codebase for both platforms.
- ✅ **UDP with our own framing**, not WebRTC or TCP. TCP retransmits stall
  the picture; WebRTC adds a stack we would have to fight for low latency.
  12-byte header, ≤1200-byte datagrams (fits any path MTU), zero-wait
  reassembly, NACK for keyframes.
- ✅ **Workspace of four crates**: `aa-core` (protocol, no OS code,
  `forbid(unsafe)`), `aa-platform` (traits + per-OS backends), `aa-host`,
  `aa-viewer`. Backends sit behind traits so the pipeline never knows which
  OS it is on; a hardware-free *mock* backend lets CI and the Linux dev
  container exercise the whole path.
- ✅ **Hardware codecs by default, software as fallback.** Codec order of
  preference AV1 > HEVC > H.264; H.264 via OpenH264 is the universal
  fallback.
- ✅ **Windows first for the real host** even though the plan said Mac
  first — the owner's PC was the machine in front of them, and the Windows
  media APIs (DXGI + Media Foundation) are well documented.

**Setup detours (all resolved)**
- ❌ `git` not recognised on the PC → install Git for Windows.
- ❌ "not a git repository" / "origin does not appear to be a git repo" /
  push rejected → clone fresh, `git push --force` once to align histories.
- ❌ `cargo` "could not find Cargo.toml in system32" — several times.
  Lesson: **always give commands that `cd` into the repo first.**
- ❌ Mac: no Homebrew, no `gh`; GitHub auth failed → personal access token
  with `repo` scope; owner chose a **private** repo.
- ❌ Mac clone: "destination path already exists" → use the existing folder.

**Tooling that paid off immediately**
- ✅ **CI on three OSes** (ubuntu/macos/windows). The dev container is
  Linux and cannot compile Windows or macOS code, so CI is the only
  compiler for the platform crates.
- ✅ **CI posts compiler errors as a commit comment on failure** — the
  Actions log blob was not reachable from the container; the comment is.
  Every platform file since has gone through 1–3 rounds of "push, read
  comment, fix API drift".

## Day 1 — first pixels (2026-10-03)

- ✅ Mock pipeline end to end at 60 fps on localhost ("I can see gradient").
- ✅ Viewer window: winit + wgpu, vsync (Fifo) locked, keyboard and mouse
  forwarded as USB HID usages so Mac and PC keyboards agree.
- ❌ wgpu 30 API drift (InstanceDescriptor, surface texture, present) →
  read the crate source in the registry rather than guess.
- ✅ Software H.264 (OpenH264) as the mock stream's codec and the fallback
  everywhere.
- ✅ Windows host: DXGI Desktop Duplication capture + `SendInput`.
- 📏 Real screen 2880×1800@60 through **software** H.264: **1–10 fps**.
  CPU bench 28 fps at 1080p. Not viable; hardware encode is mandatory.

## Day 2 — hardware encode and the first real sessions (2026-10-06)

- ✅ **Media Foundation hardware encoder** (Quick Sync on the owner's Arc
  130T), zero-copy: the DXGI texture goes straight into the encoder via a
  DXGI device manager, BGRA in, no CPU touch. Async MFT unlock, CBR, no
  B-frames, event polling with a deadline.
- ❌ First output returned `MF_E_TRANSFORM_STREAM_CHANGE` → accept the
  encoder's proposed output type and continue (standard MF dance).
- 📏 **Locked 60 fps, 0 % loss, ~31 Mbps, <1 ms assembly** Win→Win on LAN.
  Bench: 110 fps @1080p, 73 @1440p.
- ❌ Mac viewer showed a **black window** then nothing → the session had
  died silently; now the window closes with the reason
  (`Wake::SessionEnded`).
- ❌ "no answer from host" — many times. Causes found over the day:
  host not running yet; wrong folder; `7700~` typo in the address;
  **Windows firewall** (fixed with an explicit UDP 7700 inbound rule);
  port already in use (10048) from a stale host; and later the PC's
  **address changing** between sessions.
- ❌ Mirroring worked but **clicks didn't register** → input channel was
  fine; the issue was DPI: Windows reported virtualised coordinates on a
  150 %-scaled display. Fix: declare per-monitor DPI awareness first,
  move with `SetCursorPos`, send clicks with absolute position.
- ❌ Decoder errors (`Native:18/2`) after any packet loss → **gate decoding
  on keyframes** and throttle NACKs to one per 150 ms.
- ❌ Enter opened new tabs, Esc did nothing, clicking the video opened
  tabs → a **stuck modifier** on the host (macOS swallows Cmd key-up on
  Cmd+Tab). Fix: track held keys, release all on focus loss and on
  disconnect, map Cmd→Ctrl on the Mac.
- ❌ Washed-out colours → full-range BT.709 metadata on the encoder and
  sRGB textures/surface in the viewer.
- ❌ Picture not edge-to-edge → `--stretch`, later a checkbox.
- ✅ Owner asked for settings "visually, not flags" → **egui overlay**
  (Ctrl/Cmd+Shift+S): fullscreen, stretch, max Mbps, stats.

### Wi-Fi: the hard part

- 📏 Loss 12–27 % on the owner's Wi-Fi at 31 Mbps.
- 🔁 **Paced sends** (spread each frame across the frame interval) +
  AIMD adaptive bitrate + encoder watchdog. Bitrate control stayed; pacing
  did not:
  - ❌ Pacing inside the receive loop **froze the session** (RTT stuck,
    fps 0): sending starved packet intake → moved sending to a dedicated
    task.
  - ❌ Pacing then raised assembly time to 10–14 ms and made loss *worse*
    on Wi-Fi (consecutive frames overlapping on air) → **pacing removed**,
    burst sends restored. Bitrate is the lever, not spacing.
- ❌ **Phantom 98–99 % loss with `dropped=0`** → two independent sequence
  counters (video task and control task) looked like gaps to the viewer,
  and the bitrate controller pinned the stream to the floor → one atomic
  `SeqCounter` shared by every sender.
- 📏 Ping between the two machines: **36–187 ms** (Mac→PC ping shows 100 %
  loss only because Windows blocks ICMP by default; PC→Mac is the real
  number). This is the ceiling on everything; Ethernet on the PC is the
  standing advice.
- Owner moved on before confirming steady 60 fps after the seq fix.

## Day 3 — audio, discovery, hardware decode, HEVC (2026-10-06 → 07)

### Audio
- ✅ **WASAPI loopback → Opus (48 kHz stereo, 10 ms, 128 kbps, inband FEC)
  → viewer playback via cpal** with packet-loss concealment. Silence is not
  sent.
- ❌ `opus` crate needs **CMake** to build libopus → owner installed CMake
  on both machines (README updated).
- ❌ cpal 0.18 and windows-rs API drift (`SampleRate` is a `u32`,
  `StreamConfig` by value, `description()` not `name()`, enum flag casts,
  packed-struct field access) → fixed from CI comments.
- ✅ Owner: **"audio is working."**
- ✅ Owner asked for **mute PC speakers while streaming** → overlay toggle
  → `SetHostMute` → `IAudioEndpointVolume`, restored on every disconnect
  path.
- ❌ On the owner's device muting the PC **also silenced the stream**:
  endpoint loopback taps after the software volume → 🔁 **per-process
  loopback** (`ActivateAudioInterfaceAsync`, exclude our own process tree)
  taps before volume/mute; endpoint loopback kept as fallback.
- ❌ Audio **crackled** → fixed 30 ms buffer vs. 50–180 ms delay swings →
  🔁 **adaptive jitter buffer** (start 60 ms, +20 ms per underrun, cap
  300 ms, refill before resuming, shrink after 20 s calm). Stats line
  shows depth and underruns.
- ❌ Host crashed with `STATUS_HEAP_CORRUPTION` right after
  `tap=Process` (2026-10-07): the bindings' `PROPVARIANT` runs
  `PropVariantClear` on drop and freed our stack-allocated activation
  params → `ManuallyDrop`. The tap itself had activated fine.
- Pending owner confirmation: mute now leaves the Mac playing; crackle gone.

- 2026-10-07 owner: audio works (PC muted, Mac plays) but wants better
  quality and more volume. Changes: Opus 128 → 256 kbps, complexity 10,
  fullband, Music signal; viewer now uses the FEC copy on loss instead of
  only guessing; viewer volume boost (+6 dB default, slider to +18) behind
  a peak limiter; 5 ms fade-in after refills. Pending owner listen test.
  Note: the Mac mini's built-in speaker is small; headphones or external
  speakers are the fair test of quality.

### Discovery
- ❌ "no answer from host" once more — the PC's address had changed
  (.3 → .5) → ✅ **LAN auto-discovery**: viewer broadcasts `Discover` on
  UDP 7700, host answers `Here { name }`. Same port, so the existing
  firewall rule covers it. Viewer takes no address, a name, an IP, or
  ip:port.

### Hardware decode on the Mac
- 🔁 First attempt with hand-written C bindings was cut off and discarded;
  rebuilt on the `objc2-*` framework crates (generated from Apple's SDK).
- ✅ **VideoToolbox decoder**: Annex-B → format description from SPS/PPS,
  length-prefixed slices, synchronous decode to BGRA. Compiled on macOS CI
  first try. Falls back to OpenH264 if the session cannot open.
- Pending owner confirmation on the M4 Pro.

### HEVC
- ✅ **Codec chosen at negotiation**: encoder and decoder factories build
  the engine after Hello/Welcome instead of at startup. Windows offers HEVC
  when an MF HEVC encoder exists (Main 4:2:0 8-bit profile set); the Mac
  offers HEVC when VideoToolbox opens. Negotiation already preferred HEVC,
  so Win→Mac should now run at roughly half the bitrate.
- ❌ 2026-10-07 first real run: black screen on the Mac, fps 2–12,
  `keyframe arrived; resuming skipped=1` after nearly every frame, 0% loss.
  PC log showed only the Quick Sync **H.264** encoder and no switch line.
  Cause: Windows put HEVC at `codecs[0]` but loaded H.264; the capture
  thread treats `codecs[0]` as the loaded codec, so it never switched.
- ✅ Fix: Windows opens the HEVC encoder at startup and only offers HEVC if
  it opens (else H.264 only). Capture thread logs the codec on every
  connect and withholds video instead of sending a mislabelled stream.
- Pending owner re-test.

### Docs
- ✅ `HANDOFF.md`, `CLAUDE.md`, this journal; ARCHITECTURE §9 lessons and
  §10 measurements kept current; mirrored to the claude.ai Project.

---

## Day 4 — picture quality from simulation (2026-10-07)

Owner reported washed-out colours, a pixelated picture, and wants 120+
fps (PC panel supports 120 Hz+, currently set to 60).
- Colour: our own BGRA→NV12 conversion on the GPU video processor with
  explicit BT.709 video range (convert.rs); encoder header says the same.
  Pending owner check of the Mac's `stream colour` line.
- Pixelation: the simulation showed the bitrate controller collapsing to
  2 Mbps under 0.5% random loss. Fixed by RTT-aware congestion detection,
  higher start rate and a probe phase. Plus video FEC (10% XOR stripes).
- fps: stream follows the PC refresh rate (rounded), capped at 300 in
  negotiation; viewer advertises 300.

## Day 5 — DualSense, everything (2026-10-08)

Owner made the repo public (CI free again) and asked for DualSense
haptics and controller feedback. ViGEm can't be a DualSense, and gilrs
only sees buttons, so we went raw:
- Studied VIIPER (GPL, study only) for descriptors and feature reports;
  wrote our own USB/IP server + virtual DualSense in Rust, served on
  localhost and attached by usbip-win2 (free, signed driver).
- Mac reads the real pad with hidapi (USB + Bluetooth; BT reports
  CRC-checked and converted), forwards reports untouched; game output
  reports come back and are written to the pad.
- Haptics are USB audio: the virtual pad is a composite device with a
  4-channel sound card; isochronous transfers paced at real time; haptic
  pair sent as Opus; played on the pad's own sound card (USB) or turned
  into rumble (Bluetooth).
- Fallback when usbip-win2 is missing: generic DualShock 4 + rumble.
- CI caught `SlotTable` needing a manual Default (gilrs ids).
- Found while writing: hidapi's `BusType` has no `PartialEq` (use
  `matches!`); `is_none_or` is newer than our MSRV 1.80.
- Simulated end to end, chaos suite 12/12.

## Day 5, later — Mac as host (2026-10-08)

Commit history rewritten so every commit shows only the owner; CLAUDE.md
now says so for future sessions.
Built the Mac host from the objc2 bindings' sources (read locally, since
nothing Mac compiles here): ScreenCaptureKit capture and sound,
VideoToolbox encode, Quartz input, caffeinate. CI caught two things:
an Objective-C object sent through a channel needs a Send wrapper, and
framework names in docs need backticks. Green on all three systems on
the third push. Real-Mac test pending.

## Day 5, evening — the app, installers, first release (2026-10-08)

Owner asked for the whole thing as an installable app with free hosting.
Chose native installers over a browser viewer (a browser adds latency and
can't do controllers, mic or hardware decode the same way). Built the
Anywhere app (connect list + share button + plain-words status + extras
checklist), icon, .dmg and Windows installer, and a release workflow.
Release CI lessons: Git Bash rewrites `/DName=value` arguments into paths
(MSYS_NO_PATHCONV=1), and Inno Setup is already on GitHub's Windows
machines. v0.2.0 published on GitHub Releases.

## Day 5, late — light, polished app (2026-10-08, v0.3.0)

Owner asked for a polished light-mode app holding every setting, fast.
Redesigned with a sidebar (Connect / Share / Settings / Extras), Inter
font (converted from the npm `inter-ui` woff2 with fonttools, OFL),
white cards, switches. Viewer accepts start-up preferences by flag.
Measured idle CPU under Xvfb/llvmpipe: an egui Spinner forced endless
redraws (~160%); replaced by a 1 Hz blink (~7%). Found by running the
viewer window under Xvfb: the overlay never cleared egui texture deltas.

## Things we deliberately did not do (yet)

- No WebRTC, no TCP, no QUIC: latency budget.
- No pacing: see above.
- No audio/video sync logic yet: `ts_ms` is on every audio packet, unused.
- No Mac host, no controllers, no clipboard, no internet path: in order in
  `HANDOFF.md`.
