# Anywhere: a plain-language guide to the code

This guide is for anyone who has to understand, debug or change Anywhere,
including someone who does not write code. It explains what each part does,
how a connection works from start to finish, and where to look when
something goes wrong. Deeper design reasons live in `ARCHITECTURE.md`; the
history of what was tried lives in `JOURNAL.md`; the current state and next
steps live in `HANDOFF.md`.

## 1. Words used everywhere

| Word | Meaning |
|---|---|
| **Host** | The computer being shared and controlled (the "sharing" side). |
| **Viewer** | The computer you sit at, watching and controlling the host. |
| **Session** | One connection from a viewer to a host, from hello to goodbye. |
| **Frame** | One picture of the screen. 60 frames a second means 60 pictures. |
| **Encoder / encode** | Squeezes a picture into a small amount of data (H.264, HEVC, AV1 are the three ways). The graphics chip does this. |
| **Decoder / decode** | The reverse, on the viewer: data back into a picture. |
| **Keyframe** | A complete picture that needs nothing before it. Bigger, but a clean restart point. |
| **P-frame** | A small "what changed since the last picture" frame. Needs the frames before it. |
| **Repair frame** | A P-frame built only on a picture the viewer confirmed it has (Mac hosts only). |
| **Packet / datagram** | One small piece of data sent over the network (about 1,200 bytes). A frame is cut into many. |
| **UDP** | The kind of network sending used: fast, but pieces can go missing, so Anywhere checks and asks again. |
| **Resend / NACK** | The viewer saying "piece X of frame Y never arrived, send it again" / "I lost a frame". |
| **FEC** | Extra "parity" pieces sent with a frame so a missing piece can be rebuilt without asking. |
| **Bitrate (kbps / Mbps)** | How much data per second the stream uses. Adjusted every second to fit the network. |
| **Pairing** | Typing the 6-digit code once so two computers trust each other from then on. |
| **Key** | Each computer's secret identity file. Pairing remembers the other computer's public key. |
| **Service / launch agent** | The always-on copy of the host that starts with the computer (Windows service / Mac launch agent). |
| **Crate** | One Rust package. Anywhere has six (see section 3). |
| **CI** | GitHub's computers that build and test every change on Windows, Mac and Linux. |

## 2. The programs that get installed

| Program | What it is | Where its records (logs) are |
|---|---|---|
| **Anywhere** (`anywhere.exe` / `Anywhere.app`) | The window you open: Connect, Share, Settings, Extras. It starts the other programs. | PC: `%APPDATA%\AnywhereAlternative\`; Mac: `~/Library/Application Support/AnywhereAlternative/` |
| **aa-host** | Shares this computer: captures the screen and sound, sends them, applies the other side's mouse and keys. | PC always-on: `C:\ProgramData\AnywhereAlternative\service.log`; otherwise `host.log` in the app's folder above |
| **aa-viewer** | The stream window: receives the picture and sound, sends your mouse, keys, controller, camera, microphone. | `viewer.log` in the app's folder above |
| **aa_vcam.dll** (Windows 11 only) | Makes "Anywhere Camera" appear in every Windows app. Windows' camera service loads it. | none (reports through the host log) |
| **Browser add-on** | Adds "Anywhere Camera" and "Anywhere Microphone" to Chrome, Edge and Safari. | the browser's extension console |
| **Installers** | `Anywhere-x.y.z-windows-setup.exe`, `Anywhere-x.y.z-mac.dmg`. Built by the Release workflow. | Mac crash reports: `~/Library/Logs/DiagnosticReports/aa-*.ips` |

The app, host and viewer are separate programs on purpose: if one crashes,
the others keep running.

## 3. How the code is organised

All program code is in `crates/`. Each crate has one job:

| Crate | Job | Contains operating-system code? |
|---|---|---|
| `aa-core` | The shared rules both computers follow: packet layout, messages, pairing and encryption, bitrate rules, settings. | No. Runs and is tested anywhere. |
| `aa-platform` | Everything that talks to Windows or macOS: screen capture, encoders, decoders, mouse and keyboard, sound, camera, controllers, discovery, updates. | Yes: `src/windows/` and `src/macos/`, plus shared parts. |
| `aa-host` | The sharing program, built from the two crates above. | No (uses `aa-platform`). |
| `aa-viewer` | The watching program and its window. | No (uses `aa-platform`). |
| `aa-app` | The Anywhere window people open. | A little (installing the always-on service). |
| `aa-vcam` | The Windows camera DLL. | Yes (Windows only). |

Other folders: `browser-addon/` (the add-on's JavaScript), `packaging/`
(installer scripts for Windows and Mac), `.github/workflows/` (CI and the
Release workflow), `assets/` (icons), `docs/` (these notes).

### Every file, in one line each

**aa-core** (shared rules)
- `lib.rs`: list of the modules below.
- `wire.rs`: the 12-byte label on every packet; cutting frames into packets and putting them back together; deciding what to ask to be resent.
- `control.rs`: the readable (JSON) messages: hello, welcome, pairing, settings changes, camera state.
- `capability.rs`: what each side can do, and the agreement they settle on (codec, size, frame rate).
- `control_flow.rs`: adaptive bitrate (faster when the network allows, slower when it struggles) and packet pacing.
- `quality.rs`: how much data "full quality" needs and which frame rate the connection can afford.
- `secure.rs`: pairing codes, keys and encryption of every packet.
- `input.rs`: mouse, keyboard, scroll and controller events, packed small.
- `video.rs`, `audio.rs`: descriptions of video frames and sound frames.
- `av1.rs`: reads just enough of an AV1 stream to hand it to a Mac.
- `clipboard.rs`: copy on one computer, paste on the other (large items in pieces).
- `ds5.rs`: DualSense controller pass-through messages.
- `calls.rs`: shared settings for "Anywhere Camera" and the browser add-on.
- `share.rs`: "whole screen / one app / one window".
- `stats.rs`: running measurements (round trip, loss) for the bitrate and the stats overlay.
- `config.rs`: stream settings and the encoder parameters derived from them.

**aa-host** (sharing side)
- `main.rs`: starts everything; command-line options; the always-on mode.
- `session.rs`: the main loop: handshake, sending frames, receiving input and reports, adapting bitrate and frame rate.
- `pipeline.rs`: the capture-then-encode thread and the input thread.
- `net.rs`: the encrypted socket and pairing on the host side.
- `calls.rs`: receives the viewer's camera and microphone for calls; the browser add-on's local connection.
- `vcam.rs`: feeds "Anywhere Camera" on Windows.
- `udp.rs`: creates network sockets with big enough buffers.

**aa-viewer** (watching side)
- `main.rs`: starts everything; options; tells the host what this screen can show.
- `session.rs`: the main loop: handshake, receiving frames, asking for resends, decoding, sending reports.
- `window.rs`: the stream window: drawing pictures, mouse and keyboard, game mouse, shortcuts.
- `overlay.rs`: the in-window settings (Ctrl/Cmd+Shift+S).
- `net.rs`, `link.rs`, `udp.rs`: the encrypted connection and the hand-off between window and network.
- `audio.rs`: plays the host's sound; sends this microphone.
- `pads.rs`, `ds5.rs`: controllers (DualSense raw pass-through).
- `camera.rs`: sends this computer's camera during calls.
- `keymap.rs`: physical keys to standard key numbers.
- `pacing.rs`: measures how evenly frames arrive (smoothness).
- `nv12.wgsl`: the small graphics-card program that turns video colours into screen colours.

**aa-platform** (operating-system parts)
- `lib.rs`: the five abilities every system must provide (capture, encode, decode, input, controllers) and which version each system gets.
- `windows/`: `capture.rs` (screen), `convert.rs` (colours), `encoder.rs`, `decoder.rs`, `input.rs`, `audio.rs`, `gamepad.rs` (virtual Xbox/PlayStation pads), `camera.rs`, `vcam.rs`, `share.rs` (one app/window), `session.rs` (sign-in screen and the always-on service), `mod.rs` (puts them together).
- `macos/`: `capture.rs`, `encoder.rs`, `decoder.rs`, `input.rs`, `audio.rs`, `speaker.rs` (mute), `gamepad.rs`, `camera.rs`, `wininfo.rs` (which window is where), `mod.rs`.
- Shared: `audio.rs` (Opus sound codec, playback, microphone), `playout.rs` (smoothing sound), `camera.rs`, `clipboard.rs`, `discover.rs` and `lan.rs` (finding computers), `portmap.rs` (opening a door on the home router so paired computers connect from any network), `pairing.rs`, `trust.rs` (who is paired, keys, pairing code), `tailscale.rs`, `update.rs` (download and install updates), `usbip.rs` + `ds5dev.rs` + `padhub.rs` (virtual USB DualSense on a PC), `padmap.rs`, `hid_scancode.rs` + `hid_mac.rs` (key tables), `share.rs`, `sw.rs` (software video codec fallback), `mock.rs` (fake hardware for tests).

**aa-app** (the window people open)
- `main.rs`: all four pages, settings, finding computers, starting host and viewer, updates.
- `procs.rs`: starts and stops `aa-host` and `aa-viewer`.
- `service.rs`: installs the always-on sharing (Windows service, Mac launch agent) and the add-ons.
- `update.rs`: checks GitHub for new versions and installs them.
- `checks.rs`: which optional helpers are installed (Extras page).
- `addon.rs`: the browser add-on folder and Safari extension.

**aa-vcam** (Windows camera DLL)
- `lib.rs`: picture conversion; `source.rs`: the camera objects Windows talks to.

## 4. One connection, step by step

1. **Finding.** The host announces itself on the network every second and
   answers "who's there?" questions (`aa-platform/src/lan.rs`,
   `discover.rs`, `aa-host/src/session.rs`). The app lists what it hears.
2. **Pairing (first time only).** You type the host's code on the viewer.
   Both sides prove they know it without sending it, and remember each
   other's key (`aa-core/src/secure.rs`, `aa-platform/src/pairing.rs`,
   `trust.rs`, `aa-host/src/net.rs`).
3. **Hello and agreement.** The viewer says what it can show (codecs,
   screen size, frame rate); the host answers with the agreed setup
   (`aa-core/src/capability.rs`, `control.rs`). From here every packet is
   encrypted.
4. **Picture, host side.** Capture the screen (`windows/capture.rs` or
   `macos/capture.rs`) → encode (`encoder.rs`) → cut into packets with
   parity (`aa-core/src/wire.rs`) → send at an even pace
   (`aa-host/src/session.rs`, `control_flow.rs`).
5. **Picture, viewer side.** Collect packets into frames, ask again for
   missing pieces (`wire.rs`, `aa-viewer/src/session.rs`) → decode
   (`decoder.rs`) → draw (`aa-viewer/src/window.rs`, `nv12.wgsl`).
6. **Feedback.** Once a second the viewer reports loss, round trip and
   frames received; the host raises or lowers the bitrate and frame rate
   (`control_flow.rs`, `quality.rs`).
7. **Your input.** Mouse, keys and controllers go the other way at once,
   on their own lane (`aa-viewer/src/window.rs`, `pads.rs`, `ds5.rs` →
   `aa-core/src/input.rs` → `aa-host/src/pipeline.rs` →
   `windows/input.rs` or `macos/input.rs`).
8. **Sound, clipboard, camera.** Sound: `windows/audio.rs` or
   `macos/audio.rs` → Opus → `aa-viewer/src/audio.rs`. Clipboard:
   `aa-core/src/clipboard.rs` + `aa-platform/src/clipboard.rs`. Camera and
   microphone for calls: `aa-viewer/src/camera.rs` → `aa-host/src/calls.rs`
   → browser add-on or `aa-vcam`.
9. **Goodbye.** Either side says bye, or the host times the viewer out after
   a few seconds of silence; the viewer reconnects by itself if it can.

## 5. When something breaks, look here

| What you see | First look in | Likely files |
|---|---|---|
| A computer doesn't appear, or "older Anywhere" | host log (is it sharing? which version?) | `discover.rs`, `lan.rs`, `aa-host/src/session.rs` (Discover), `aa-app/src/service.rs` |
| Can't connect from another network | host log `outside:` line (also on the Share page) | `portmap.rs`, `discover.rs`, `tailscale.rs` |
| Pairing code rejected | host log "pairing" lines | `secure.rs`, `aa-host/src/net.rs`, `pairing.rs`, `trust.rs` |
| Black screen | viewer log "decoder", host log "encoder" | `encoder.rs`, `decoder.rs`, `capability.rs` |
| Blurry, blocky, smeared, garbled patches | viewer log `stream` lines (loss, resent, dropped), "repair frame" | `wire.rs` (resends), `control_flow.rs`, `quality.rs`, `decoder.rs`, `aa-viewer/src/session.rs` |
| Wrong frame rate | viewer log "asking the host for", host log "stream set up" | `aa-viewer/src/main.rs`, `quality.rs`, `windows/mod.rs` / `macos/capture.rs` |
| Wrong colours | | `convert.rs`, `nv12.wgsl`, `macos/capture.rs` (sRGB), `macos/mod.rs` |
| Mouse or keys wrong | host log "input" | `window.rs`, `keymap.rs`, `windows/input.rs`, `macos/input.rs`, `hid_*.rs` |
| Game camera spins | | game mouse in `window.rs`, `macos/input.rs` (`MouseMoveRel`) |
| Controller missing or dropping | both logs "DualSense", "controller" | `pads.rs`, `ds5.rs`, `padhub.rs`, `usbip.rs`, `windows/gamepad.rs`, `macos/gamepad.rs` |
| No sound / crackles | viewer log `audio=` part of stream lines | `windows/audio.rs`, `macos/audio.rs`, `playout.rs`, `aa-viewer/src/audio.rs` |
| Microphone not reaching the other side | viewer log "microphone" | `aa-platform/src/audio.rs` (Mic), `aa-viewer/src/audio.rs` |
| Copy-paste not crossing | both logs "clipboard" | `aa-core/src/clipboard.rs`, `aa-platform/src/clipboard.rs` |
| Camera in calls | host log "camera", browser console | `camera.rs` files, `aa-host/src/calls.rs`, `vcam.rs`, `aa-vcam`, `browser-addon/` |
| App window flickers, blinks, freezes | `app-crash.log` | `aa-app/src/main.rs` (never do slow work on the window thread) |
| Updates don't install, app closes itself | `service.log` "update", markers in `C:\ProgramData\AnywhereAlternative` | `aa-app/src/update.rs`, `aa-platform/src/update.rs`, `windows/session.rs` |
| Sign-in screen not shared | `service.log`, `service-supervisor.log` | `windows/session.rs`, `aa-app/src/service.rs` |

When the picture smears or blocks after a hiccup, look for "keyframe
arrived; resuming" in viewer.log: each one is a gap in the frame numbers
the viewer noticed and repaired. Many of them mean a lossy link (see the
`loss` and `dropped` figures); "decode failed" lines without them would be
a new kind of problem.

The viewer's `stream` line, written once a second, reads like this:
`fps` frames shown, `mbps` data received, `rtt_ms` round trip, `loss` pieces
lost before repair, `dropped` frames that never reached the decoder (lost
whole, given up, or dropped on either side), `fec_fixed` pieces rebuilt
from parity, `resent` pieces that came again after asking, `gap_ms`
spacing between frames (typical / worst 1% / worst), `stutters` visible
hitches, `audio=` sound frames / filled-in gaps / buffer / underruns.

## 6. Making a change safely

1. Read `HANDOFF.md` and check `JOURNAL.md` for anything already tried.
2. Find the cause in the logs before changing code (section 5).
3. Change the smallest thing that fixes it. Explain the why in a comment,
   with the version and what was seen.
4. Run `cargo fmt --all && cargo clippy --all-targets -- -D warnings &&
   cargo test`, then the mock smoke test (`aa-host --mock` and
   `aa-viewer --headless --mock --pair <code>`).
5. Push. CI builds Windows and Mac; if it fails, it leaves the errors as a
   comment on the commit.
6. Bump the version in `Cargo.toml` and run the Release workflow to ship.
7. Write what was learned in `ARCHITECTURE.md` section 9 (bugs that cost a
   round) and `HANDOFF.md`.

## 7. How the comments in the code are written

- Every file starts with a `//!` paragraph: what it is for and why it is
  built that way.
- Every function, type and important value has a `///` line saying what it
  does in plain words.
- Inside functions, `//` lines explain each step, especially anything that
  looks odd. Many end with the version and what the owner saw when that
  line was added (for example "0.5.4: the camera spun"), so the history of a
  decision stays next to the code.
- Every file's `//!` header also has an "In plain words" part where the
  technical description alone would lose a non-programmer.
- `SAFETY:` comments sit above every `unsafe` block and say why it is safe.
  Those blocks are where Rust's own checks are switched off to talk to
  Windows or macOS, so they deserve the most care.
