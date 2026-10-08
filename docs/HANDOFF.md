# Handoff: picking this project up in a fresh session

Read this first, then `docs/ARCHITECTURE.md` for the design, `docs/JOURNAL.md`
for the full history of what was tried, what failed and why, and `README.md`
for setup. Everything below is true as of the last commit on `main`.

## Who you are working with

- The owner has **no coding background**. You write all code and explain
  the *why* of each step so they learn as they go. Keep explanations short
  and concrete; avoid jargon without a one-line gloss.
- They test on real hardware and paste terminal output back. Give them
  copy-paste commands **with the folder included** (they have been bitten
  by running `cargo` from the wrong directory):
  - PC (Windows 11, PowerShell): `cd $HOME\anywhere-alternative`
  - Mac (Apple Silicon M4 Pro, Terminal): `cd ~/anywhere-alternative`
- The repo is **private** and must stay private. Push directly to `main`.
- When something fails on their machine, ask for the exact log lines rather
  than guessing.

## Their setup

| Machine | Role today | Notes |
|---------|-----------|-------|
| Windows 11 25H2, Intel Core Ultra with Arc 130T, 2880×1800 @ 60 Hz (panel supports 120 Hz+, owner says) | host | Quick Sync H.264 **and** HEVC encoders via Media Foundation; WASAPI process-loopback audio works; "Anywhere Alternative" UDP 7700 firewall rule added |
| Mac mini M4 Pro | viewer | VideoToolbox hardware decode; cpal output |
| Network | Wi-Fi, same router | Ping between them measured **36–187 ms** — very poor. Most "60 fps isn't steady" and "audio crackles" reports trace to this. Ethernet on the PC is the standing recommendation. |

Their router reassigns the PC's address often (seen 192.168.1.3 → .5); the
viewer now discovers the host by broadcast so no address is needed.

## What works (verified by the owner on real hardware)

- Windows host → Mac viewer over Wi-Fi: video (hardware H.264, zero-copy
  DXGI → Quick Sync), keyboard + mouse, settings overlay (Cmd+Shift+S),
  adaptive bitrate, keyframe recovery after loss.
- System audio PC → Mac ("audio is working").
- Firewall, DPI scaling, stuck-modifier and colour-range issues all fixed.

## Blocker as of 2026-10-08

**GitHub Actions is not running**: jobs are refused with "recent account
payments have failed or your spending limit needs to be increased". The
owner was asked to fix billing (org minute-creative → Settings → Billing
and plans). Until it runs, the Mac/Windows-only code from these commits is
**uncompiled**: audio devices + mic (viewer/host parts; aa-platform parts
did compile), controllers (`windows/gamepad.rs`, viewer `pads.rs`),
auto-reconnect. First job once CI is back: push an empty commit or re-run,
read the error comments, fix.

## Shipped but NOT yet confirmed by the owner (ask for results first)

Picture:
1. **HEVC end to end** (the first try was black: host sent H.264 labelled
   HEVC). PC log `windows host: ... codec=Hevc`, then `encoder already
   matches the negotiated codec codec=Hevc`.
2. **Colour (they reported "washed out / grey")**: BGRA→NV12 on the GPU
   video processor with explicit BT.709 video range. PC log `encoder colour
   path input=NV12...`; Mac log `stream colour`. Ask for both lines if it
   still looks off.
3. **Pixelation**: RTT-aware bitrate controller, higher start + probe, video
   FEC. Should look sharp within ~4 s and stay sharp on Wi-Fi.
4. **120+ fps**: they said the PC panel supports 120 Hz+ but it ran at 60.
   They must set it in Windows display settings; `refresh_hz` in the PC log.
   Viewer shows at most the Mac monitor's refresh rate (model unknown).
   Run `aa-host --bench` for Quick Sync throughput at native size.

Everything else (all simulated end to end, see ARCHITECTURE §10):
5. Audio 256 kbps, +6 dB boost slider with limiter; output follows the
   Mac's current device (Bluetooth/wired switch live).
6. Mic Mac→PC: overlay checkbox or `--mic`. PC needs **VB-CABLE**
   (vb-audio.com/Cable); apps then pick "CABLE Output". macOS will ask for
   Microphone permission for Terminal.
7. Clipboard both ways (text, images ≤32 MB) and Cmd+C/V fix.
8. Discovery: beacons + every-adapter asking + remembered last host.
9. Controllers: PC needs **ViGEmBus** (github.com/nefarius/ViGEmBus/releases)
   for Xbox/other pads (→ virtual Xbox 360). **DualSense: full
   pass-through** (2026-10-08): PC also needs **usbip-win2**
   (github.com/vadimgrn/usbip-win2/releases) so the host can plug in a
   virtual USB DualSense mirroring the real one: adaptive triggers, light
   bar, LEDs, touchpad, gyro, rumble, and haptics (haptics need the pad
   on a USB cable on the Mac; over Bluetooth they become rumble). Without
   usbip-win2 the DualSense still works as a basic DualShock 4 with
   rumble. Untested on real hardware yet. Rumble for non-DualSense pads
   back to the Mac: still not built.
10. Auto-reconnect after PC sleep/restart/Wi-Fi drop (title shows it).

Their answers so far: PC refresh supports 120 Hz+; colours "washed out /
grey"; want all accessories (mic, headphones, controllers, other USB).

## Agreed next steps, in order

1. CI back → fix whatever the uncompiled code trips on.
2. Owner retest of items above; collect `refresh_hz`, `--bench`, `stream
   colour`, `encoder colour path` lines.
3. Owner test of DualSense pass-through: install usbip-win2 on the PC,
   pad on USB then Bluetooth on the Mac; check the PC log lines
   "DualSense plugged in" / "the game is sending sound/haptics" and the
   Mac's "DualSense connected (full pass-through)". If Windows shows a
   driver problem for the sound part, `AA_DS5_NO_AUDIO=1` on the host.
   Then: rumble back for non-DualSense pads (Mac GameController
   framework, `GCController.haptics`, via objc2).
4. **Other USB devices (owner wants them)**. Honest scope: generic USB
   passthrough *from a Mac* needs user-space access to the raw device,
   which macOS only allows with Apple's restricted VM entitlement
   (`com.apple.vm.device-access`, what UTM/Parallels use). Not feasible for
   an unsigned tool. Practical route, per device class:
   - Webcam → stream it like the mic; Windows 11 virtual camera API
     (`MFCreateVirtualCamera`, 22H2+) presents it to apps. No driver.
   - Drives/files → file transfer/shared folder, not USB.
   - Drawing tablets → pen events (pressure/tilt) over the input channel.
   - Anything else: ask which devices they actually mean first.
5. Mac as host: **built 2026-10-08, CI green, not yet run on the Mac.**
   ScreenCaptureKit picture + system sound, VideoToolbox HEVC/H.264,
   Quartz input, caffeinate. Owner test: `aa-host` on the Mac, viewer on
   the PC (H.264, software decode for now) or another Mac. Grant Screen
   Recording + Accessibility to Terminal. Added the same day: PC viewer
   GPU decode (Media Foundation/DXVA, H.264 + HEVC if the HEVC extension
   is installed), NV12 drawn by shader on every viewer, Mac-host
   controllers (virtual DualSense via IOHIDUserDevice, needs `sudo`), mic
   into the Mac via BlackHole 2ch. Still open: DualSense extras
   (touchpad/gyro/haptics) on a Mac host (it gets generic state only),
   lock-screen behaviour, rumble back for non-DualSense pads.
6. Zero-copy Mac decode (IOSurface → Metal), multi-monitor, encryption +
   internet stage.

## How development works here

- You are usually on a **Linux container that cannot compile the Windows
  or macOS code**. `cargo clippy --all-targets && cargo test` locally
  checks the portable crates; the platform code is checked by **GitHub
  Actions** on push (ubuntu / macos / windows). On failure, CI posts the
  compiler errors as a **commit comment**; read it with
  `gh api repos/minute-creative/anywhere-alternative/commits/<sha>/comments --jq '.[].body'`.
  A full run takes ~15 min (Windows is slowest). Expect one or two rounds
  of API-drift fixes per new platform file.
- Smoke test that always works locally:
  `aa-host --mock` in one process, `aa-viewer --headless --mock` (no
  address → discovery) in another; expect `fps=60 loss=0.00%`.
- Clippy runs with `pedantic` and `-D warnings` on CI. FFI modules carry
  `#![allow(unsafe_code, clippy::pedantic)]` and SAFETY comments.
- Code style: `rustfmt.toml` (max width 120). Commit messages explain the
  *why* in the body; the owner reads them.
- Keep `docs/ARCHITECTURE.md` (and its mirror in the claude.ai Project,
  `claude/ARCHITECTURE.md`) current when a design decision changes, and add
  to §9 "Lessons" whenever a bug costs a debugging round.

## Where things are

```
crates/aa-core        protocol, wire format, control messages, audio header,
                      bitrate controller — no OS code, no unsafe
crates/aa-platform    traits + backends:
  audio.rs            Opus enc/dec, Player (adaptive jitter buffer), traits
  sw.rs               OpenH264 software encode/decode (fallback everywhere)
  windows/            DXGI capture, MF encoder (H264/HEVC), SendInput,
                      WASAPI process/endpoint loopback, endpoint mute
  macos/decoder.rs    VideoToolbox decoder (objc2-* crates)
  mock.rs             hardware-free pipeline for tests/CI
crates/aa-host        session.rs (UDP loop, negotiation, control),
                      pipeline.rs (capture/encode/audio/input threads)
crates/aa-viewer      session.rs (receive, reassemble, decode thread, stats),
                      window.rs (winit+wgpu), overlay.rs (egui settings),
                      audio.rs (sink), discover.rs (LAN broadcast)
```

Key cross-cutting facts:
- Codec is chosen at negotiation; `HostBackends.encoder_factory` and
  `ViewerBackends.decoder` (a factory) build the engines on demand.
- One `SeqCounter` per socket, shared by every sending task (see Lessons).
- Discovery and streaming share UDP 7700 (`Discover` / `Here` control
  messages), so one firewall rule covers both.
