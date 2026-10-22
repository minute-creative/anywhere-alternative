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

## Day 5, evening — pairing, encryption, Tailscale, always-on (v0.4.0)

Owner: "confirm it works for every combination, far away, stays connected;
a Mac that lost power should be reachable without a password if it was
paired before." Honest answer first: only Windows host → Mac viewer had
run on hardware; nothing worked off the home network; and there was no
lock at all (anyone on the Wi-Fi could connect, nothing encrypted).

Asked before building. Choices: Tailscale now (they asked the cost
difference: Tailscale $0 for up to 6 people; own server $0 on Oracle's free
tier or ~$5/month, plus upkeep), login screen reached through Anywhere,
code once then remembered.

Built: SPAKE2 pairing + 3-DH handshake + ChaCha20-Poly1305 per datagram
(`aa_core::secure`, tamper/replay/garbage tests), trust files, Tailscale
peer discovery, endless reconnect by host key, `RESET` for a restarted
host, Mac launch agent for login screen + `pmset autorestart`, Windows
start-at-sign-in, app UI (pair card, code on Share, paired lists).

Found while testing: a repeated `INIT` must get the *same* answer, or the
two sides end up with different keys (the host must not make a new
one-time key for a retransmit); same for the pairing confirm (keep it
answerable after success, or a lost OK strands the viewer).

Then "same applies to windows": a SYSTEM service that starts the host in
the session on the screen and follows the input desktop, so the Windows
sign-in screen can be seen and typed into from afar (0.4.1). CI green on
the first try (API signatures checked against the windows-rs source
first, since Windows code can't be compiled here).

## Day 5, night — feedback on 0.4.1 (v0.4.2)

Owner tested PC watching Mac on Wi-Fi: two cursors (the Mac drew its
cursor into the video; turned off), blur and glitches (asked for logs and
the Mac's display resolution before guessing), cropped help text. Asked
for add-ons by default and automatic updates; chose fully automatic.
Windows updates go through the SYSTEM service (it fetches the release
itself, so the app's request file can't make it run anything else); Mac
replaces its own bundle. Writing GitHub secrets for a fixed Mac signing
certificate was blocked in this session; left a script for the owner.

## Day 5, late night — mute on Mac host, blocky still screens (v0.4.3)

Owner: muting "the PC" from the viewer didn't mute the Mac (no Mac
SpeakerControl existed; overlay always said "PC"). Screenshot of a still
screen showed blocky edges: frames only come when the screen changes, so
the last rushed frame after movement stayed. Host now re-encodes the last
picture at 150/400 ms and sends a fresh keyframe at 900 ms of stillness
(mock: 1 real + 3 refinement frames per still second, all decode). Also
a manual shutdown isn't a power cut (autorestart only covers power loss);
asked for `service.log` and to check aa-host's own permissions after the
Mac restart test failed. Disk filled up mid-build (29 GB target dir):
`rm -rf target/debug/incremental` between rounds.

## Day 5, night — still blurry on 0.4.3 (v0.4.4)

Owner: Mac screen still blurry on the PC; Share page showed "encode
failed: encoded frame: OSStatus -1". -1 is our own code for "VideoToolbox
dropped the frame" (low-latency rate control over budget). We treated it
as a failure: forced a keyframe, and a keyframe of a whole Retina screen
squeezed into one frame's budget is blurry. 0.4.3's third refinement step
(a forced keyframe at 900 ms) had the same flaw. Fix: drops are
`PlatformError::FrameSkipped` (no keyframe, not counted), refinement is
six P-frame re-encodes over 1.3 s, no keyframe. Not tried yet:
`MaxAllowedFrameQP` during refinement (may make VT drop frames instead).
Also: BlackHole has no .pkg on GitHub; "Install add-ons" now uses the
link from formulae.brew.sh (fallback BlackHole2ch-0.7.1.pkg).

## Day 5, near midnight — permissions lost on update; signed 0.4.5

After 0.4.4 the Mac lost Accessibility and Local Network ("No route to
host", os error 65, on every beacon = macOS 15 Local Network privacy, not
the network). Cause: ad-hoc signature changes every release. Owner ran
make-signing-cert.sh and added the secrets; 0.4.5 is the first signed
release (verified by unpacking the dmg with 7z). Added
NSLocalNetworkUsageDescription to Info.plist.

## Day 5, midnight — blur and freezes explained by the Mac's log (v0.4.6)

host.log/service.log from the Mac: display 1920x1080 at **200 Hz**, so
the encoder ran with fps=200 (per-frame budget = bitrate/200) and the
pacer squeezed each frame into ~3.75 ms. Results: VideoToolbox dropped
frames nonstop at low bitrate (dozens of "OSStatus -1" a second in 0.4.3
= frozen picture) and ~14% loss bursts at flat RTT (5 ms), each of which
halved the bitrate: 45 -> 22 -> 11 -> 5 -> 2 Mbps, slow climb, repeat.
Fixes: Mac capture/encode capped at 60 fps (`AA_MAC_MAX_FPS`); loss
without rising delay halves only at 25%+, else x0.85; runs of encoder
skips are logged as "picture frozen". Also seen: "send failed: No route
to host" on video sends = Local Network permission missing on the Mac.

## Day 6, 00:30 — Mac sharing silently not running after the signed reinstall

PC couldn't reach the Mac on LAN or Tailscale. `pgrep aa-host` empty,
nothing on :7700, firewall off; service.log ended at 00:09 with no error
or "asked to stop". `launchctl print` after the owner toggled Login Items
→ Allow in the Background: "submitted by smd", managed_by
ServiceManagement, BTM uuid, runs = 1, never exited. Reading: macOS
Background Task Management saw the new signature (first certificate-
signed build) and disabled/killed the agent; KeepAlive can't help while
BTM has it off. Re-enabling it + `launchctl kickstart -k
gui/$(id -u)/com.minutecreative.anywhere.host` brought it back. Should
not recur now that the signature is stable. TODO: app should notice
"always on, but no service running" and say so with a Login Items button.

## Day 6, 01:00 — Mac virtual controller refused even as root (stage 1b answered)

Owner ran `sudo aa-host --service` (euid 0, logs to service-login.log),
connected from the PC with a DualSense: padhub saw the pad, then
`IOHIDUserDeviceCreateWithProperties` returned NULL → "permission
denied"; `hidutil list` shows no 054c device. So on this macOS, root is
NOT enough. Per Apple DTS on the developer forums (threads 840311,
822647, 845599): virtual HID devices need CoreHID `HIDVirtualDevice` +
the `com.apple.developer.hid.virtual.device` entitlement, requested via
Capability Requests (paid Apple Developer account; one report of a
request stuck 2.5 months). A restricted entitlement needs a Developer ID
cert + provisioning profile, not our self-signed cert. The DriverKit
virtual-device entitlement is a dead key per Apple. Options for the
owner: (a) Apple Developer Program + request, (b) map the pad to
keyboard/mouse on the Mac (limited), (c) no controllers on a Mac host.
Also: 0.4.6 holds 68-80 Mbps on the LAN (soft cuts working).

## Day 6, 01:40 — other-room Wi-Fi sawtooth (v0.4.7)

PC is a laptop (Intel Arc 130T); in another room over Wi-Fi. Viewer log:
RTT spikes 50-730 ms, loss bursts 15-75%, stutters climbing, audio
jitter buffer grown to 300 ms. Host log: 30 s sawtooth, 4 -> 80 Mbps at
+10%/s, RTT rises from ~50 Mbps, loss burst, stacked cuts to 2-5 Mbps.
The link carried ~35 Mbps. Fixes in aa-core control_flow: trim x0.85
when queueing delay > 30 ms (or 4x quiet RTT) even without loss; remember
the choke bitrate and grow 2%/s within 80% of it (forget after 30 clean
s); after a cut, only 25%+ loss may cut on the next report. Simulated
35 Mbps link: settles ~35, lowest 33. Not yet seen on the real link.
Also noted: the PC's microphone is "CABLE Output (VB-Audio)" (probably
the wrong device; it sends whatever plays into the cable).

## Day 14 — "runs an older Anywhere" with 0.4.7 on both (v0.4.8)

Owner (2026-10-16): Connect to 192.168.1.6 says it runs an older
Anywhere, though both computers have 0.4.7. The note appears when a host
answers `Here` without its key, which every host since 0.4.0 sends — so
the *process* answering on .6 was old, even if the files were new. Most
likely the always-on sharing process started before the update and kept
running the old code (a manual drag-install on the Mac never kickstarts
the agent; the app-driven update does).

Done:
- `Here` and `Beacon` carry the host's `version`; the app's note now says
  "sharing with an older Anywhere (0.3.0; this one is 0.4.8)" and tells
  the person to turn Share off and on if it is already updated.
- The always-on host writes `service-version` at start; the app compares
  it with its own every 30 s and, on a Mac, kickstarts the agent itself
  (no password); on Windows it says what to do (restarting a SYSTEM
  service needs a prompt). Pre-0.4.8 hosts write nothing, so this helps
  from the next update on.
- Diagnosis commands for the owner: `sudo lsof -nP -iUDP:7700` (Mac),
  `Get-Process -Id (Get-NetUDPEndpoint -LocalPort 7700).OwningProcess`
  (PC) — shows which program holds the sharing port and when it started.
- Cause confirmed: `lsof` showed `aa-host` PID 589 (boot-time) still on
  :7700 after the drag-install; `launchctl kickstart -k` replaced it.

## Day 14, later — blur at 5 ms RTT, 18% loss, 0.1 Mbps (v0.4.8)

Owner's overlay, PC watching the Mac mini, both on Wi-Fi, no cable
possible, AirDrop must stay on: `8 fps 0.1 Mbps 5 ms 18.2% loss`, plus
red smears (a graphics decoder painting frames whose references were
lost). Flat delay + heavy loss = the radio dropping packets, not a full
link; the controller was trimming ×0.85 every other second for it
(5–25% loss at flat RTT counted as "lossy") and walked the stream to the
floor, where the picture is blur by definition.

Three fixes, all verified with a new `AA_SIMULATE_LOSS=18` viewer switch
against the mock host (before: 10–18 fps, ~10 frames dropped per second):
- **Radio loss never trims the bitrate** below the 25% "severe" line; above
  it, one ×0.85 then a 5-report hold. Light radio loss (<5%) still grows.
- **Adaptive FEC**: parity ratio = 1.5 × smoothed loss, 10–50%, set by the
  controller and carried to the sender through `PipelineControl`.
- **Selective resend** (`Kind::Resend`): the host keeps the last 64
  frames' datagrams (≤12 MB); the viewer asks for exactly the missing
  slices/parity as soon as a frame's packets stop arriving (a newer frame
  started, or 2 ms quiet) and again after one RTT, up to 3 times. The
  reassembler now delivers in order and holds a complete frame for up to
  2.5 × RTT (8–40 ms) while an older one is being resent — a P-frame is
  useless without its predecessor anyway. Keyframes never wait. Frames
  seen only through parity ask for "all data" (`0x7FFF`). Result: 59 fps,
  0 dropped at 18% loss on the mock link; at 1080p the same 27 fps the
  clean container link manages. Stats line gains `resent=`.

## Day 15 — Caps Lock does nothing on the Mac (v0.4.8)

Owner: Caps Lock pressed on the PC doesn't take on the Mac. Cause: Caps
Lock on a Mac is a *state* in the HID system, not a key; a Quartz
`kVK_CapsLock` key event is ignored. Fix: the Mac host opens the
`IOHIDSystem` service once and flips the lock with
`IOHIDSetModifierLockState` on each press (release ignored). The reverse
direction had a quieter bug: a Mac *viewer* reports Caps Lock as
"pressed" when the light goes on and "released" when it goes off, and a
Windows host toggles on presses, so it would stick on; the viewer now
sends a whole tap for every change. Untested on hardware (CI only).

## Day 16 — "highest quality everywhere", one app at a time (v0.5.0)

Owner: fps never above 150-200, quality dropping, wants the highest
quality, consistent at 100 km or 1000 km; share one app or window so
nothing slips; best compression; resolution and fps as settings; 240 max.
Honest framing given: distance adds only delay; the ceiling is the
sharing side's upload speed, Wi-Fi, and whether Tailscale goes direct or
through its relay. Software can only use what the path carries and keep
it steady. Answers: steady over sharp, fps gives way first down to 60,
then sharpness; app and window both; full access = pick + full control;
alone = whole screen; ship all at once.

What was built (CI-compiled only; the container cannot build Mac/PC code
and rustup cannot download those targets here):
- Frame-rate ladder priced by `sharp_kbps` (bits/pixel/frame at 60 fps:
  H.264 0.16, HEVC 0.11, AV1 0.09, × (fps/60)^0.75). Mock check: 60 → 200
  at a clean 80 Mbps; first version climbed one rung per 5 s (35 s to
  200), changed to "straight to what fits after 3 clean seconds".
- AV1 PC→Mac. Mac has no AV1 encoder, and software AV1 on an M4 at 1080p
  would take most of the CPU for ~60 fps and look no better than the
  media engine's HEVC: told the owner and left out.
- Mac LTR repairs; per-app capture, sound and input limits on both OSes.
- I told the owner the PC path copied frames through the CPU; it doesn't
  (DXGI → GPU video processor → MFT is already zero-copy). Corrected.
- Mac encoder already used low-latency rate control (since 0.4.3); the
  Mac→PC data saving comes from LTR repairs (no keyframe bursts), app
  sharing, and the steadier bitrate.
- A parallel session had pushed 0.4.8 (resend, adaptive FEC from the
  controller) an hour before: rebased onto it, kept its FEC and resend,
  dropped my loss→FEC formula, a repair frame now also ends waiting for
  older frames in the reassembler.

## Day 16, later — camera and microphone for calls (v0.5.1)

Owner: pick camera and mic in Google Meet and other apps on the watched
computer, both directions. Checked options for the Mac side first: a real
virtual camera there needs Apple's paid developer account (camera
extensions must be signed by a registered team); owner declined, so the
free route is an add-on in the browser that adds the devices to web pages.
OBS (virtual camera, heavy) and NDI (known bugs) were offered; owner
picked the add-on. PC side: Windows 11 allows a virtual camera without a
driver (MFCreateVirtualCamera + a media source DLL), so the PC gets one
for every app, plus the add-on too.

Built it end to end. Tested the browser half in headless Chromium against
mock host/viewer with a test picture and test tone: everything flowed
except the H.264 decode, which open-source Chromium can't do (Chrome,
Edge and Safari can). Found a keyframe-request storm when decoding fails
(every frame asked for one); now checks decoder support first and asks at
most twice a second. The Windows DLL is CI-compiled only.

## Day 17 — code review and comments (v0.5.8)

Owner: go through syntax, logic, architecture and comments so the code is
easy to debug and change; comments plus real bug fixes; plain-language
summary per file plus technical notes; ship as 0.5.8. Another chat had
already written docs/CODE-GUIDE.md and commented aa-core and aa-host; this
pass did the platform code, viewer, app, camera DLL and add-on, and
checked core and host for logic.

The main find came from comparing the pre-review build with the new one
under `AA_SIMULATE_LOSS=15`: OpenH264 reported "reference lost" while the
viewer showed `dropped=0`. A frame with every packet lost never enters the
reassembler, so in-order delivery silently skipped it. Fixed by checking
frame-number continuity in the decode thread (see HANDOFF 0.5.8 for the
full list). Process slip: a comparison build was put in `target-old/`
inside the repo and `git add -A` committed it; caught before the push
completed and amended out. Build comparisons go outside the repo.

Later the owner reported lag and freezes watching the Mac (Elden Ring
over 100 Mbps, YouTube freezing). Read the PC's viewer.log through the
device link: the last session joined the Mac and sat at 0 fps for 30 s.
Cause: the host only encodes when the screen hands it a changed picture,
and a still Mac hands out none. Added `ScreenCapture::refresh` and a
`AA_MOCK_STILL` mock to test it. The 100 Mbps came from the controller
probing up to the user's 80 Mbps cap regardless of need; capped at twice
the sharp rate. Also found the Mac capture queue kept the oldest frames.

## Day 20 — Anywhere on the iPhone (2026-10-20)

The owner had been trying to run PC/Steam games on the iPhone itself
(a separate experiment, "iOSDeck": Linux VMs in UTM with FEX and Steam);
it ran, but Steam doesn't fit in the memory iOS gives one app. So the
goal became: stream the Mac mini's games to the iPhone with Anywhere,
from anywhere, with a Razer Kishi.

The viewer already decodes with VideoToolbox and draws with wgpu/Metal,
both of which exist on iOS, so the plan was to reuse it rather than write
an iPhone app. Steps: split `aa-viewer` into a library, build it as a
static library for `aarch64-apple-ios`, hand-write a minimal Xcode
project around it, read settings from the iOS Settings app, controllers
from GameController. First build linked after one fix
(`IPHONEOS_DEPLOYMENT_TARGET`), first phone run streamed at 60 fps over
Tailscale, but only in the top-left corner, and the pairing code kept
coming back. Cross-tested fixes in the iPhone Simulator on the Mac
(which found a third bug: GPU limits) before reinstalling on the phone.
See HANDOFF "iPhone viewer" and ARCHITECTURE §9.

## Things we deliberately did not do (yet)

- No WebRTC, no TCP, no QUIC: latency budget.
- No pacing: see above.
- No audio/video sync logic yet: `ts_ms` is on every audio packet, unused.
- No Mac host, no controllers, no clipboard, no internet path: in order in
  `HANDOFF.md`.
