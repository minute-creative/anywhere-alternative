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
| Windows 11 25H2, Intel Core Ultra with Arc 130T, 2880×1800 @ 60 Hz | host | Quick Sync H.264 **and** HEVC encoders via Media Foundation; WASAPI process-loopback audio works; "Anywhere Alternative" UDP 7700 firewall rule added |
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

## Shipped but NOT yet confirmed by the owner (ask for results first)

In order of commit:

1. **Host speaker mute** (overlay → Audio → "Mute PC speakers"). First
   version muted the stream too on their device; fixed by switching capture
   to per-process loopback (`tap=Process` in host log). Needs a listening
   test: PC silent, Mac still playing.
2. **Adaptive jitter buffer** for the crackling. Stats line shows
   `audio=…f/…c buf=NNms under=N`; `under` should stop climbing after the
   first few seconds.
3. **LAN auto-discovery**: `aa-viewer -- --fullscreen` with no address.
   Verified in the mock pipeline, not yet on their machines.
4. **VideoToolbox hardware decode** on the Mac. Compiles on CI; look for
   `viewer decoder: VideoToolbox` and `VideoToolbox decoder configured`.
5. **HEVC end to end**: negotiation picks HEVC when both sides have
   hardware. Look for `codec: Hevc` on the Mac's `connected` line, and on
   the PC `codec=Hevc` in the `windows host:` startup line plus
   `encoder already matches the negotiated codec codec=Hevc` on connect.
   First attempt was a black screen (host sent H.264 labelled HEVC; see
   ARCHITECTURE §9); fixed, awaiting re-test. Intel's MF HEVC
   encoder is the untested piece; if the picture is wrong, the quick
   escape is to drop `Codec::Hevc` from the host's `codecs` list in
   `crates/aa-platform/src/windows/mod.rs`.

## Agreed next steps, in order

1. Microphone: Mac mic → PC. The PC side needs a virtual audio *input*
   device, which Windows does not provide without a driver. Scope options
   honestly before coding (a bundled virtual-cable driver vs. DriverKit-
   style kernel work vs. "WASAPI render to a loopback-capable device").
2. Zero-copy Mac decode: `IOSurface`-backed `CVPixelBuffer` straight into a
   Metal/wgpu texture instead of the BGRA CPU copy in `macos/decoder.rs`.
3. Windows hardware decode (D3D11VA / MF decoder MFT) for Win viewers.
4. Mac as host: ScreenCaptureKit + VideoToolbox encode + CGEvent input;
   virtual DualSense spike (stage 1b in ARCHITECTURE).
5. DualSense end to end; clipboard; multi-monitor; auto-start service.
6. Internet stage: pairing codes, hole-punch, small relay, encryption.

Also on the list, from the owner: audio/video sync (audio packets already
carry `ts_ms`; nothing consumes it yet), 4K60 locked and 1080p240 targets,
host resolution matched to the viewer's aspect ratio.

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
