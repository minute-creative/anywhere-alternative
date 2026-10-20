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
- The repo is **public** (owner's decision 2026-10-18; see "Product
  direction"). Never commit personal details. Push directly to `main`.
- When something fails on their machine, ask for the exact log lines rather
  than guessing.

## Their setup

| Machine | Role today | Notes |
|---------|-----------|-------|
| Windows 11 25H2, Intel Core Ultra with Arc 130T, 2880×1800 @ 60 Hz (panel supports 120 Hz+, owner says) | host | Quick Sync H.264 **and** HEVC encoders via Media Foundation; WASAPI process-loopback audio works; "Anywhere Alternative" UDP 7700 firewall rule added |
| Mac mini M4 Pro | viewer | VideoToolbox hardware decode; cpal output |
| Network | Wi-Fi, same router, **both machines wireless; no Ethernet cable available; AirDrop must stay on** (owner, 2026-10-16) | Ping has measured 36–187 ms on bad days and a flat 5 ms with 18% loss on others. Software must cope: see "Lossy Wi-Fi" in 0.4.8. Don't lead with "plug in a cable" again. |

Their router reassigns the PC's address often (seen 192.168.1.3 → .5); the
viewer now discovers the host by broadcast so no address is needed.

## What works (verified by the owner on real hardware)

- Windows host → Mac viewer over Wi-Fi: video (hardware H.264, zero-copy
  DXGI → Quick Sync), keyboard + mouse, settings overlay (Cmd+Shift+S),
  adaptive bitrate, keyframe recovery after loss.
- System audio PC → Mac ("audio is working").
- Firewall, DPI scaling, stuck-modifier and colour-range issues all fixed.

## Pairing, encryption, Tailscale, always-on (0.4.0, 2026-10-08)

Owner asked: every combination (Mac/PC either way, Mac-Mac, PC-PC) easy to
connect, working from far away and staying connected, and a Mac that lost
power should come back reachable, with no code for computers paired before.
Decisions they made: **Tailscale now, own server later**; after a power
cut the Mac **waits at its login screen and you type the Mac password
through Anywhere**; **code once, then remembered**.

- `aa_core::secure`: SPAKE2 pairing with a six-digit code, then per
  connection a 3-DH (X25519) handshake and ChaCha20-Poly1305 on every
  datagram with a replay window. Only `Discover` may arrive in clear.
  `aa-host/src/net.rs` and `aa-viewer/src/net.rs` wrap the sockets; the
  session code is unchanged above them. Protocol version 2 (0.3 and 0.4
  don't talk).
- `aa_platform::trust`: keys and pairings as files (`viewer.key`,
  `host.key`, `paired-hosts.json`, `paired-viewers.json`, `pair-code`);
  `AA_DATA_DIR` moves them (tests). On a Mac with always-on sharing, the
  host side lives in `/Library/Application Support/AnywhereAlternative`
  (root:admin 0770) so the login-screen copy is the same computer.
- Far away: `Here` carries the host key and every address (incl. its
  Tailscale 100.x). Discovery also asks every remembered address of paired
  hosts and every online Tailscale peer (`tailscale status --json`). The
  viewer reconnects until its window closes (headless: 2 min), finding the
  same host by key; a restarted host answers stale sealed packets with
  `RESET` so the viewer reconnects in ~1 s.
- Mac always-on (`aa-app/src/service.rs`): `/Library/LaunchAgents/
  com.minutecreative.anywhere.host.plist` with `LimitLoadToSessionType`
  Aqua + LoginWindow runs `aa-host --service`; `pmset autorestart 1`,
  `sleep 0`. Logs `service.log` / `service-login.log` in the system folder.
  **Unknown until tested on the Mac:** whether ScreenCaptureKit and
  CGEvent work for the root copy at the login window, and the TCC prompts
  for `aa-host` itself (launchd starts it, not the app). FileVault on =
  impossible (pre-boot screen); the app warns. Tailscale's App Store and
  Standalone apps start only after login; for far-away access to the
  login screen the Mac needs the open-source `tailscaled` (Homebrew).
- Windows always-on (0.4.1, owner: "same applies to windows"):
  service `AnywhereHost` = `aa-host --windows-service` (SYSTEM, from boot,
  `windows/session.rs`) keeps one `aa-host --service` running as SYSTEM in
  the active console session (`CreateProcessAsUserW`, token session id);
  capture and input call `follow_input_desktop()` (`OpenInputDesktop` +
  `SetThreadDesktop`) so the sign-in/lock/UAC desktops are captured and
  typed into. Shared folder `%ProgramData%\AnywhereAlternative` (ACL:
  SYSTEM, Administrators, the user's SID). Setup via one UAC prompt
  (`run_elevated` + a .cmd: icacls, `sc create/config`, failure restart,
  `powercfg` no sleep/hibernate/lid action on AC). Installer stops the
  service before replacing files and restarts it. Logs:
  `service-supervisor.log`, `service.log`. Untested on hardware: whether
  DXGI duplication and SendInput really work on Winlogon from this
  SYSTEM child (the standard approach of remote-desktop tools).
  Laptop power-on after the battery dies is a BIOS setting (not ours).
  Tailscale on Windows needs "Run unattended" to be up before sign-in.
  Also "Open Anywhere when I sign in" (HKCU Run, `--background`).
- Verified here (Linux, mock): pairing with right/wrong code, connect
  without code afterwards, unpaired refused, host restart → reconnect
  (1.3 s fast path, 8 s slow path), all unit tests incl. tamper/replay.
  Not verified on real hardware yet.

## Shipped but NOT yet confirmed by the owner (ask for results first)

**After 0.6.0 (2026-10-20 evening), owner's first phone test, CI only, not yet released:**
Owner tested the phone (0.6.0 phone build, Mac on 0.5.13 or the mislabelled
release, see below) and sent screenshots. Fixed and on `main`:
- Settings list didn't scroll (10 rows taller than a landscape phone):
  scrolls, follows the d-pad, log opens on the newest lines.
- Launcher strip sank under the screen edge (the Full access note wrapped
  in the hint row): hero sized from the real height, note moved into it.
- Desktop controls (owner's request): B = Back (⌘[), Y = right-click,
  R4 = launcher (needs a job in the Razer Nexus app to reach apps; also in
  the quick menu), hold L2 = magnify.
- Text fields: the Mac host reports the focused text field over
  Accessibility (`macos/textfocus.rs`, `TextFocus` message, only to viewers
  with `text_focus` in their hello); the phone opens the iPhone keyboard by
  itself (Settings → Typing: iPhone / Controller / Off), with autocorrect
  and dictation, sending edits as a diff. While typing the picture is a
  wide band zoomed to the field instead of the whole screen shrunk small.
- Release workflow: refuses a typed version that differs from Cargo.toml.
  The first 0.6.0 run had "0.5.6" typed, built 0.6.0 under that name and
  replaced the 0.5.6 downloads; no v0.6.0 release existed. Owner asked to
  remove those three files from the 0.5.6 release (the session couldn't
  delete release assets). CI actions moved off Node 20.
Owner decisions still to build: **Phone mode** (a phone-shaped virtual
display on the Mac with HiDPI, streamed while the phone watches; owner
chose this over changing the Mac's own resolution), window actions in the
quick menu, and an **Elden Ring layout** (below).
Elden Ring with the Kishi (owner: menus and gameplay don't respond): no
virtual controller on a Mac host (root refused, Day 6), and the phone was
on Desktop controls. Plan offered: a layout mapping the pad to Elden
Ring's PC keys (play: F jump, Space dodge, R item, E interact, clicks,
Shift+click, X crouch, Q lock-on, arrows, G map, Esc; menus: E OK, Q back).
A, B and L1/R1 differ between play and menus, so it needs to know which:
`cursor_captured` is Windows-only today (always false on a Mac host).
Owner chose **automatic**; built in 0.6.1: `macos/pointer.rs` (pulled-back
pointer + CGCursorIsVisible in a full-screen app; logs "game pointer
capture changed" with both signs) and the phone's "Elden Ring" controls
(play / menu layouts, `shiftClick` action, "Play keys"/"Menu keys" note).
To confirm in the owner's test: the note flips; A/B work in menus; L1/R1
(Z/X) switch menu tabs. In game, "Device for On-Screen Prompts" =
Keyboard and Mouse.
Smoothness (owner: "doesn't feel like a consistent 60" on an iPhone 14
Plus, a 60 Hz screen): the phone took only the newest frame each refresh
(repeat-then-skip on uneven arrival); now `FrameSlot::queued(2)` shows
frames in order. 75 fps is only offered on >60 Hz screens; a Mac with a
60 Hz monitor captures 60 anyway (Phone mode's virtual display could run
faster).
Handheld audit (owner: "everything operable with the Kishi V2, best
experience"): pairing pad fitted (~330 pt, was ~420), home cards scale,
button-hint pills (multi-character glyphs showed "…"), Problems log pages
with L1/R1, the app claims the pad's Home button
(`preferredSystemGestureState = .disabled`; before, only ≡+⧉ worked),
sticky ⌘ on the controller keyboard (L2), X from the iPhone keyboard to
pad keys, quick menu L1/R1 jumps, exit hints while connecting.
**Phone mode** built (owner: phone screen becomes main, Large text):
`macos/vdisplay.rs` (CGVirtualDisplay, HiDPI, made main for the session,
front window moved over), capture rebuilds when the main screen changes,
`Capabilities::phone_display` from the phone (Settings → Phone mode, on).
Also fixed: a pipeline rebuild dropped the viewer's Resolution setting.
To test on hardware: Phone mode on/off, menu bar + front app on the phone
screen, the Mac monitor keeps working, everything back on disconnect,
and a host crash leaves the Mac normal.
**Version is 0.6.1**; the owner releases it (0.6.0 was published before
the text-field and game-pointer host code).
Flaky test: `usbip_server_survives_garbage_and_still_serves` (50 ms sleep
before checking the connection was forgotten) failed once on macOS CI.

**0.6.0 (2026-10-20), the newest — the iPhone app and the launcher, CI only:**
Owner asked (2026-10-20): the Mac on the iPhone, "custom macOS designed for
iPhone", console-style, everything operable with a Razer Kishi V2 (touch
also works); Mac mini is the host; landscape only; 1080p or 720p; 75 fps
cap on the phone; Tailscale/cellular from day one; free tooling only (no
Apple Developer Program); Android not now; the desktop app may adopt the
look later. See `docs/IPHONE.md` for the result and the controls table.
- `crates/aa-viewer` is now a library + the desktop program (`window`
  feature). `reconnect.rs` holds connect/reconnect (was in main.rs);
  `link.rs` holds `Wake`, `LiveStats`, the launcher state, the icon store
  and an audio ring for systems where the UI plays sound (iOS).
- `crates/aa-mobile`: C interface (`include/aa_mobile.h`) over the viewer
  library; static library linked into the Swift app. Tested here: unit
  tests only (no compiler for iOS on Linux).
- `crates/aa-platform/src/ios`: VideoToolbox decoder shared with macOS
  (`#[path]` to `macos/decoder.rs`); Apple framework crates now listed for
  both systems in Cargo.toml.
- Launcher: `aa_core::apps` (AppEntry, pages, IconPack), control messages
  `AppList` / `Apps` / `AppAction`; `ClipItem::Blob` (format byte 3) rides
  the clipboard transfer for the icon pack. Host: `aa_platform::apps`
  (Mac: app folders + Steam library via `plutil`/`sips`/`ps`/`open`/
  `osascript`; Windows: empty list for now; Linux/mock: a pretend list).
  Quitting an app uses AppleScript: the first time macOS may ask the host
  for Automation permission; it falls back to a TERM signal.
- `ios/`: SwiftUI app, XcodeGen spec (`project.yml`), `build-rust.sh`
  (run by Xcode before compiling). Free signing = 7-day reinstall.
- CI job "iPhone app": Rust for `aarch64-apple-ios-sim`, then xcodegen +
  xcodebuild (simulator, unsigned). Expect API-drift rounds: nothing in
  `ios/` or the iOS Rust target has been compiled before the first push.
- Second pass the same day (owner: "add the missing pieces"): editable
  Custom control layout (`ios/Anywhere/Bindings.swift`, `ControlsView`);
  clipboard both ways (`ViewerCommand::CopyItem`, `link::take_clip`,
  C `aa_mobile_copy_text` / `aa_mobile_take_clip_text`); held keys
  released on background/lock/pad-disconnect; audio restarts after a
  call; a half-received launcher list is re-asked (`apps_pending`);
  Windows host lists Start-menu shortcuts + Steam (open only, no icons).
- Third pass (same day): picture zoom 2x/3x around the pointer (Metal
  samples a window; touches map through it), launcher opens on the first
  connect (setting), quick-menu Full-screen app / Hide other apps / Show
  controls (cheat-sheet), haptic ticks on focus moves, app icon from
  assets/icon.png, listing moved off the host's network loop (it would
  have frozen the picture for a second), bundle scan cached (icon lookup
  was quadratic in plutil runs).
- CI rounds so far: 1 lints in apps.rs; 2 lints in aa-mobile; 3 the iOS
  Rust build and XcodeGen passed, Swift had one error (`window` clashed
  with UIView.window); 4 (afternoon) `struct_excessive_bools` in
  aa-mobile. **CI is green on `main` as of `fe00bce` (2026-10-20 14:xx
  UTC)**, all five jobs. Read the latest commit comment for the current
  state before changing anything.
- **Status at the end of 2026-10-20's afternoon session:**
  - The phone build on the owner's Mac failed with `Undefined symbol:
    _aa_mobile_show_desktop`: Xcode had skipped the Rust step since the
    first build (output listed, no inputs → "up to date" forever) and
    linked the day-old library. Fixed in `ios/project.yml`
    (`basedOnDependencyAnalysis: false`); owner then reported **"build
    succeeded"**. If an older generated project is still open, `git pull`
    + `xcodegen generate` picks the fix up.
  - **The Mac is still on 0.5.13**, the newest published release. The
    launcher needs a 0.6 host ("Asking the Mac" otherwise). The 0.6.0
    Release run started on `5ae8f68` at 13:18 UTC was cancelled, so no
    0.6.0 dmg exists yet; the owner was asked to start Actions → Release →
    Run workflow → `0.6.0`, then "Update now" on the Mac. Confirm the Share
    page shows 0.6.0 before reading any iPhone test result.
  - The phone has not been run against a 0.6 host yet. Nothing in the
    0.6.0 list above is confirmed on hardware beyond "built, paired and
    streamed" from the first evening (pre-redesign).
- First run on the owner's iPhone (same evening): built, paired and
  streamed. Fixed: the Mac wasn't found (iOS forbids broadcasts without
  the multicast entitlement → `lan::subnet_hosts` sweep on iOS), the
  address box opened iOS's keyboard (now typed on the pad), the launcher
  said "Asking the Mac" against 0.5.13 (host version now reaches the
  phone). Owner: "looks boring, not designed to use Mac apps on a
  phone" → redesign: **app-first** (`AppAction { show }`: after an app
  runs, the host shares only that app via the existing one-app share;
  needs Full access for the phone), PS5-style launcher (tabs, hero, one
  strip, blurred icon background, Desktop tile), quick menu as a bottom
  control bar. All CI-only at the time of writing.
- Known limits, told to the owner: no virtual controller on a Mac host
  (entitlement), so games on the Mac get keys + mouse from the pad; the
  phone sends no mic/camera; icons only on a Mac host; CPU frame upload
  (fine at 1080p75, not zero-copy).
Ask for: whether the app installs and pairs; `viewer.log` from the phone
(Settings → Problems, or the Files app); fps/Mbps in the corner at 1080p75
on Wi-Fi and over Tailscale; whether the launcher lists Steam games and
icons appear; the feel of the pointer speed.

**0.5.1 (2026-10-17), the newest — camera and microphone for calls, CI only:**
Owner asked: change camera and mic in Google Meet and other apps on the
computer being watched, "flexible both ways"; no $99 Apple account, so
for the Mac the free route is a browser add-on (owner chose it over OBS
and NDI); Chrome/Edge via store listing (owner pays the $5 Chrome fee and
uploads), Safari with the "Allow Unsigned Extensions" catch; camera only
while an app uses it; laptop's built-in camera.
- Viewer (sitting side) captures camera only when the host asks
  (`CallDevices`), 1280x720@30 NV12 → OpenH264 baseline 2.5 Mbps →
  `Kind::Camera` slices. Mac: AVFoundation (Info.plist
  NSCameraUsageDescription). PC: MF source reader with advanced video
  processing. `--camera`, `--call-mic` (name/auto/off), `--test-camera`.
  Mic: existing Opus path, now also switched on by the host's call request.
- Host: `aa-host/src/calls.rs` CallHub: WebSocket 127.0.0.1:7721 (Origin
  must be our add-on: dev ID `eboaohfmengdlgbmnjailhbjjkdcfelp`, store IDs
  in `aa_core::calls` once published, any `safari-web-extension://`).
  Sends H.264 and 10 ms PCM; keyframe on new watcher or loss.
- Add-on `browser-addon/`: MAIN-world `page.js` adds devices to
  enumerateDevices/getUserMedia (also the default when the computer has no
  camera/mic, e.g. Mac mini), WebCodecs decode → MediaStreamTrackGenerator
  (Chrome/Edge) or canvas.captureStream (Safari); asks per site in a popup
  window. `packaging/browser/build_addon.py` makes the unpacked folder (in
  the app), the store zip (release asset) and the Safari variant;
  `packaging/macos/build-safari-ext.sh` turns it into an appex in
  Anywhere.app/Contents/PlugIns (non-fatal if Xcode step fails).
- Tested here: Playwright Chromium with the add-on against mock host +
  viewer: devices listed, permission window, tracks live, sound arrives,
  camera starts/stops with the tab. Video decode NOT testable here
  (open-source Chromium has no H.264) — first real check is the owner's.
- Windows 11 virtual camera: `crates/aa-vcam` (cdylib, media source per
  Microsoft's Frame Server custom media source rules, NV12+YUY2), CLSID
  `{4ABA54B9-4410-46E2-99D7-5445CD96A471}` registered by the installer;
  host creates the camera with MFCreateVirtualCamera (looked up at run
  time; Windows 10 just logs and relies on the add-on); shared memory
  `Global\AnywhereCamera` created by the DLL while an app uses it, filled
  by the host after decoding. Entirely CI-compiled, never run.
Ask for: viewer.log + host.log from a Meet call each way, a screenshot of
the call's camera menu, and on the PC whether "Anywhere Camera" shows in
the Camera app.

**0.5.0 (2026-10-17) — built on top of 0.4.8, CI only:**
Owner's rules (asked and answered 2026-10-17): quality steady rather than
chasing the top; frame rate gives way first but never below 60, then
sharpness (not resolution); 240 fps is the most; Resolution and Frame
rate as settings; share one app *or* one window; viewer limited to that
app (clicks, keys, sound); only the person at the sharing computer picks,
unless a paired computer has **Full access** (= may pick what is shared
*and* keeps full control); a computer left alone shares the whole screen.
- **Frame-rate ladder** (`aa_core::quality`): starts at 60, climbs to the
  fastest rung the bitrate affords at full sharpness after 3 clean
  seconds, drops at once below 80% of what the rung needs. Mac capture
  now runs at the display's rate (≤240, `AA_MAC_MAX_FPS`); the pipeline
  holds early frames back (newest wins). Host log: `frame rate adapted`.
- **Settings** (watching side): Resolution full/1440/1080/720 (Mac scales
  in ScreenCaptureKit, PC in the GPU video processor); Frame rate
  auto/60–240 (`--resolution`, `--fps`; the viewer also sends its screen's
  refresh, so a 60 Hz PC screen is never sent 200).
- **Share one app/window**: app writes `share.json` (fresh ≤120 s, else
  whole screen) next to the pairing files; host follows it. Mac: SCK
  filters (app = display incl. that app; window = desktop-independent
  window) and SCK audio filter; input limited via the Quartz window list.
  PC: whole screen captured, only the app's visible rects copied into a
  black texture on the GPU (`windows/share.rs`), crop for one window;
  per-process loopback audio (include tree); input limited via
  WindowFromPoint/GetForegroundWindow; Win key, Alt+Tab etc. blocked.
  Failure to share just that = nothing shown (never the whole screen).
- **Full access** toggle per paired computer (Share page); viewer overlay
  shows "What to show" with the host's list (`ShareList`/`SetShare`).
- **AV1 PC→Mac**: Windows offers AV1 if an MF hardware AV1 encoder opens
  (Arc 130T should); Mac viewer offers AV1 if `VTIsHardwareDecodeSupported`
  (M4 Pro: yes). av1C built from the sequence header (`aa_core::av1`).
  `AA_NO_AV1=1` on the PC turns it off. Mac→PC stays HEVC (no Mac AV1 encoder).
- **Loss repair without keyframes (Mac host)**: VideoToolbox long-term
  references; viewer reports `last_good_frame`; NACK → `ForceLTRRefresh`
  frame flagged `RECOVERY`, viewer resumes on it. `AA_NO_LTR=1` off.
  Encoder log `loss_repair=true` if the Mac accepted it.
- **Steady**: bitrate holds at 90% of the last choke point for 60 s.
- **HEVC Video Extensions** as a PC add-on (winget msstore 9N4WGH0Z6VHQ);
  Extras check asks Windows' decoders directly. Viewer corner shows the
  codec, target fps, what is shared, and notes (H.264 fallback, Tailscale
  relay via `tailscale status --json` CurAddr/Relay).
Ask for: host + viewer logs from one session each way; `encoder:` lines
(codec, loss_repair), `frame rate adapted`, corner screenshot.

**0.4.8 (2026-10-17), check these too:**
- **Lossy Wi-Fi fix** for the owner's "blurry + red smears" at
  `5 ms / 18% loss / 0.1 Mbps`: radio loss no longer trims the bitrate,
  parity scales with loss, and the viewer asks for missing packets
  (`Kind::Resend`). Ask for an overlay screenshot: expect loss still in the
  teens (that is the radio), Mbps in the tens, ~60 fps, `resent=` growing
  in `viewer.log`. If fps is fine but it still blurs, look at `mbps` and
  the host log's `bitrate adapted` lines.
- **Stale always-on host after update**: `service-version` file + app
  check every 30 s; Mac kickstarts itself, Windows shows what to do. Only
  hosts ≥0.4.8 write the file. The "older Anywhere" note now names both
  versions.
- **Tailscale adapters skipped by the beacon** (the yellow "os error 49"
  note on the Share page should be gone).
- **Caps Lock on a Mac host** now toggles the real lock state through
  IOKit (`IOHIDSetModifierLockState`); a Mac viewer sends a full tap per
  state change. Ask: does the Mac's keyboard light follow the PC's key?
- Release 0.4.8 must be started by the owner (Actions → Release → Run
  workflow → `0.4.8`); Claude sessions are not allowed to trigger it.

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

## 0.5.5 (2026-10-17): first round of the full test checklist

Owner tests from a checklist page (claude.ai artifact, results in its
database collection "results"; read them with ArtifactData). Found and fixed:
- Mac's always-on host was pre-0.4 code (no key, name "192.168.1.9"): PC
  said "older Anywhere". Restarted by hand; app now restarts a host with no
  version note.
- PC offered 60 fps after starting at 60 Hz: now offers the screen's max.
- PC mic reopened every second on a cpal Xrun: glitches no longer reopen.
- PC viewer still H.264 with the Store HEVC extension installed: 0.5.5
  logs the reason and every HEVC decoder Windows lists (viewer.log) — read
  it and fix next.
Open: Elden Ring camera spins when played from the Mac (viewer only sends
absolute mouse; games re-centre the cursor → needs relative "game mouse"
mode); the Mac's DualSense drops and re-attaches every 10–40 s (ask for
the Mac's viewer.log lines); ~1 in 20 frames dropped PC watching Mac;
the app opens a second copy if launched again (no single-instance).

## 0.5.6–0.5.7 (2026-10-17, later)
- Found via logs: HEVC on the PC failed because HEVCVideoExtension needs
  MF_MT_FRAME_SIZE on the input type; Windows mis-decodes LTR repair
  frames (garbled static areas) → Windows viewer uses keyframes; game
  mouse (Ctrl/Cmd+Shift+G); "Update now" button; re-pairing drops ghost
  entries; Windows app window uses OpenGL; sRGB colours on the Mac.
- Mac "out of application memory" at 19:57 was not Anywhere (aa-host 0.1
  GB); aa-host was SIGKILLed (code signature) when the owner dragged in a
  new app while it ran.
- Owner idea, LOW priority (after fixes): a second macOS user used remotely
  at the same time as someone at the Mac's own screen. Plan: run aa-host in
  that user's background session (Apple Screen Sharing "log in as
  yourself"); unknown is input routing (CGEventPost targets the console).

## 0.5.8 (2026-10-17): code review, comments, real bugs
Owner asked for a full pass over syntax, logic, architecture and comments
so anyone can debug and change the code. Every file now opens with a
plain-language note; `docs/CODE-GUIDE.md` is the map. Bugs found and fixed:
- **Smear after a lost frame (the big one).** A frame whose packets were
  all lost, or one the host dropped while its network side was busy, left
  no trace on the viewer: the next frame decoded on a missing reference
  and the picture smeared until a keyframe happened to come. Also a race
  let the decoder clear the "need keyframe" flag right after the network
  loop set it. Now the decode thread checks the frame numbers itself (the
  host numbers every encoded frame) and waits for a keyframe or repair
  frame after any gap. Mock test at 15% loss: decoder errors went from 5–7
  per run to 0; the `dropped` figure now counts these frames. Likely part
  of the owner's "pixelated / not accurate" report: ask after 0.5.8.
- Keys stayed held on the host after Alt+Tab away from game mouse (the
  game-mouse focus handler skipped the key release).
- A controller "button released" could be lost for good (queue full, or a
  lost packet while the pad sat still): states now repeat every 100 ms.
- DualSense sound glitches closed its speaker/haptics for 5 s; a viewer's
  DualSense dropping now logs why ("DualSense stopped answering" in
  viewer.log) — ask the owner for that line for the Mac drop issue.
- Reconnect waited 5 s after every drop after the first (backoff never
  reset).
- A key file that existed but couldn't be read was replaced (computers
  would need to pair again); now a temporary key is used for that run.
- Two programs saving the same file could rename each other's half-written
  copy into place (shared temporary name).
- The app restarted a sharing process every 30 s when the service was
  *newer* than the app; Extras dot lit forever for HEVC alone; settings
  could be lost to a half-written file.
- Windows service: a failed installer start left "update running" behind.
- Host: a viewer reconnecting from a new port now releases the old
  connection's keys and restores the speakers.
Owner report, same day: PC watching the Mac lagged and froze (Elden Ring
showcase over 100 Mbps; YouTube froze too). The PC's viewer.log of the
last session showed 30 s at 0 fps after joining an idle Mac. Fixed in 0.5.8:
- A viewer joining a still screen got no picture until something moved
  (screens only hand out changed pictures; the host had none to send). The
  capture now has `refresh()`; the host asks for a whole picture when a
  viewer arrives, and re-encodes the last picture at once when a fresh one
  is wanted (lost frame) on a still screen instead of waiting for motion.
- The bitrate climbed to the 80 Mbps cap whatever the picture needed (1080p
  60 fps HEVC is sharp at ~14 Mbps): bursts of ~140 packets a frame swamped
  Wi-Fi. Ceiling is now twice the "sharp" rate for the negotiated size,
  codec and frame rate (~27 Mbps for that case), under the user's cap.
- Mac capture kept the *oldest* pictures when the encoder fell behind (a
  full queue dropped new ones) and held 3 of SCK's 4 buffers while idle;
  now one newest-picture slot.
- The app keeps the previous run's log as `viewer.prev.log` /
  `host.prev.log` (starting again used to wipe the log of a freeze).
Ask after 0.5.8: is it smooth at 60 fps on Wi-Fi; Mbps in the corner (should
stay well under 40 at 1080p).
Owner report: PC shared to the Mac, hover and keys worked, clicks never did
(whole desktop, not just Valorant). PC service.log: every click "SendInput
accepted 0 of 1"; also 20 s of SetCursorPos failures after a desktop
switch (that path never followed the desktop on the screen). 0.5.8: pointer
moves follow the desktop too; refused input retries after following at
once; a refused click retries without a position; after 4 refusals the log
explains the likely cause once (Riot Vanguard, installed with Valorant,
blocks injected clicks system-wide per Parsec's support page) and how to
check (`sc query vgk`). If Vanguard is confirmed, the real fix is a
virtual USB mouse through the existing USB/IP path (needs usbip-win2):
offer it to the owner as an option, it is a larger piece of work.
Open from before: DualSense drop on the Mac (need the new log line), Mac
window not reopening from the Dock, softness from 1.5x scaling, "share one
app" real test, confirm HEVC on the PC.

## 0.5.9 fixes from the owner's Mac-watching-PC test (2026-10-18)
- Clicks: confirmed Riot Vanguard (clicks worked once Valorant closed).
  SendInput reports success while Vanguard drops the click, so the
  "refused" detection never fires. Now the host checks every 5 s whether the
  `vgc` service runs (`aa_platform::input_blocker`) and sends a plain
  warning in `StreamInfo.warning` (serde default, old peers fine); the
  viewer shows it in the corner and removes it when Vanguard stops.
- Freeze/pixelation: one Wi-Fi hiccup cascaded 80 -> 4.25 Mbps in 9 s and
  the wall was set at the bottom, so it stayed blurry ~50 s. Cuts within 10
  reports are now one episode: the wall stays where it began, cuts floor at
  25% of the start (unless loss >= 25%), a halving holds 3 reports, and
  growth is 25%/s while under half the wall. Test
  `one_hiccup_does_not_sink_the_bitrate`.
- 0.5.10 (owner's Valorant test, Mac viewing PC): the Mac got a steady
  24 fps (of 120 negotiated, 2880x1800 HEVC) at ~7 Mbps, no loss, so the
  PC produced only 24. Likely the game starving capture/encode on the Arc
  130T iGPU: now the host raises its GPU priority (REALTIME, fallback HIGH,
  plus SetGPUThreadPriority(7), as Sunshine does) and logs "video speed"
  (screen_fps / sent_fps / encode_ms) every 10 s. CHECK that line in the
  next game test. Aiming did nothing because game mouse was never on (and
  I'd told the owner Ctrl+Shift+G; on a Mac it's Cmd+Shift+G): the host now
  reports when a game hides and clips the pointer (StreamInfo
  cursor_captured) and the viewer switches game mouse on/off to match.
- 0.5.10 auto-update stalled the same way on the PC. 0.5.11: the service
  now schedules a one-time SYSTEM task (AnywhereUpdateGuard, +5 min,
  `sc start AnywhereHost`) before running the installer, and the installer
  writes C:\ProgramData\AnywhereAlternative\installer.log: READ IT after
  the next update to find the real cause.
- 0.5.13 picture size follows the bitrate too: owner reported blurry +
  "fps never above 75" on 0.5.11-era logs. host.log: bitrate controller
  sat at 11-16 Mbps (loss 2-4%, RTT 11 -> 40, spikes 273/608 ms: Wi-Fi
  queueing, PC 192.168.1.2 / Mac 192.168.1.8 same LAN) while 2880x1800
  HEVC needs ~34 Mbps; overlay showed 3.6 Mbps actual. PC screen flips
  60/120 Hz (dynamic refresh); at 120 the Intel MFT's 13-16 ms per full
  frame caps ~70 fps. Mac had Frame rate fixed at 120. Fix:
  `SizeGovernor::on_bitrate` picks the biggest step with >= 85% of
  `sharp_kbps` once below 70% on a busy screen (2 windows), up when 100%
  of the bigger size for 4 windows; limit = smaller of speed and bits.
  Log: `sending a smaller picture why=connection|compressing`. Headless
  viewer ignores --max-mbps, so only unit tests cover the bits path.
  Advised: Frame rate Automatic, Ethernet on one side.
- 0.5.12 automatic picture size (owner chose this over pipelining, for
  input feel): PC log in Valorant showed encode 15-25 ms per 2880x1800
  frame on the Intel HEVC MFT (hardware, GPU NV12, zero-copy, realtime GPU
  priority all confirmed), so ~40 fps max, plus Wi-Fi-like loss of 8-34%
  and RTT spikes to 2.5 s on the Mac side (asked owner: cable or Wi-Fi?
  no answer yet). `quality::SizeGovernor` (tests) watches 2 s windows;
  "busy" = screen_fps >= 10 and >= 0.7 x sent_fps (capture shares the loop
  with encoding, so screen_fps can't exceed sent); steps 1440/1080/900/720
  down when encode > 0.85 x budget twice, up slowly. Mock check at
  2880x1800: 13 -> 39 -> 58 fps. PC monitor reports 60 Hz, so 60 is the cap.
- Third stall (0.5.9 app stuck on "Installing the update...", 0.5.11
  out). Likely cause: Inno's Restart Manager (CloseApplications defaults
  on) trying to close the shared Windows camera service that loads
  aa_vcam.dll, hanging the /VERYSILENT install. 0.5.12 installer:
  CloseApplications=no, RestartApplications=no, and PrepareToInstall
  taskkills anywhere.exe / aa-viewer.exe / aa-host.exe after stopping the
  service. The fix lives in the downloaded installer, so it applies to the
  next update whatever version is installed. Confirm via installer.log.
- OPEN: 0.5.8 -> 0.5.9 auto-update on the PC (10:20 UTC) stopped the
  service, then the installer never finished and nothing restarted the
  service (app wouldn't open). Owner started "Anywhere sharing" by hand;
  the service retried the update and it finished in 15 s. No installer log
  to see why the first run stalled. Idea: have the installer always
  restart the service on exit, or a Windows "restart on failure" recovery
  action plus a scheduled-task watchdog.
- Valorant clicks (owner accepted the risk: Riot doesn't support it, ban
  can't be ruled out): while the `vgc` service runs, `windows/vmouse.rs`
  plugs a virtual USB mouse (`hidmouse.rs`, pid.codes 1209:0001, 5 buttons,
  16-bit relative X/Y, wheel, pan) into the PC through our USB/IP server +
  usbip-win2, and routes clicks, relative moves (game mouse) and the wheel
  through it; pointing stays on SetCursorPos (allowed). Unplugged when
  Vanguard stops. Without usbip-win2 the viewer's corner note says to
  install it. Reports are queued (a click is two reports), never repeated.
  NOT yet confirmed in Valorant. If Vanguard also ignores it, the next
  options are thin: say so honestly.
- Holding a key repeated only once (right arrow moved one step): neither
  Windows nor macOS repeats injected keys. The viewer now forwards each
  held-key repeat as another press; a Mac host flags it as autorepeat.
- Screen mask (owner request): "Black out the PC's screen" in the stream
  window's settings (only when the host is a PC). `windows/privacy.rs`: a
  black, topmost, click-through window over all monitors with
  `WDA_EXCLUDEFROMCAPTURE`, so DXGI capture sees through it; dim line "This
  PC is in use remotely"; never shown if Windows refuses the capture
  exclusion. Off on every way a viewer leaves (`restore_speakers`). Limits:
  the PC's own pointer still shows; Start pop-ups may peek over it. Not on a
  Mac host yet. NOT yet confirmed on the owner's PC.
- One-time update note: Welcome carries the host's version (serde
  default). `aa-viewer/src/update_note.rs` holds a short plain-words
  changelog (add lines each release!) and shows a bottom-right note with an
  X once per newer version (`update-note-seen` file).
- Compatibility test: CI job "works with older versions" builds the last
  three release tags once (cached), then runs old host + new viewer (with
  test input via `AA_TEST_INPUT`) and new host + old viewer in mock mode
  (`scripts/compat`). Old viewers send no test input, so that direction
  checks connect and pictures only.
- Caps Lock Mac -> PC: injected by key name (VK_CAPITAL) plus scancode;
  both ends log it ("Caps Lock changed" on the Mac viewer, "Caps Lock from
  the viewer" on the PC service). If it still fails, those lines say which
  side stops.

## Product direction (2026-10-18, owner decisions)
- Run Anywhere as a product for one month, then decide go/no-go. Priority
  is the best product; selling comes second.
- Positioning: the Mac as host is the gap (Parsec's Mac host has no
  controllers, mic, camera or 4:4:4). Target: people who need their Mac (and
  PC) fully from elsewhere; creatives, agency teams, Mac owners who game.
- Connecting from anywhere, owner's choice: **no central server**. Every
  Anywhere computer is its own server: open a port on the router itself
  (UPnP / NAT-PMP / PCP), IPv6 direct when available, hosts tell paired
  computers their current outside address, and try direct paths from
  both ends at once. Fallback when both ends are blocked: Tailscale
  (already supported), or the Mac mini relaying for the owner's team.
- Owner's home network (PC test 2026-10-18): a public IPv4 address,
  no IPv6 seen, hop 2 is 10.113.0.1 → possibly the ISP shares one address
  (CGNAT); router WAN address not yet checked. If CGNAT and no IPv6, the
  own-direct path can't reach this home from outside; Tailscale covers it.
- Built 2026-10-18 (owner asleep, in 0.5.8, not yet tried on a real
  router): `aa_platform::portmap` opens UDP 7700 on the router by UPnP
  (hand-written SSDP/SOAP, tested against a pretend router) or NAT-PMP,
  renews hourly, closes on stop; the host logs one `outside:` line (open /
  provider shares the address / router won't open) and the Share page
  shows it. Local "who's there?" answers now include the outside address
  and the computer's global IPv6 addresses, so paired viewers remember them
  and find it from other networks. The host also listens on IPv6 (separate
  socket, IPv4 untouched). Pairing is refused from internet addresses
  (only home networks / Tailscale), and internet "who's there?" answers
  carry only the key. Owner's router: ZTE F670L (fibre box), login needed
  to check its WAN address and whether UPnP is on. Not built: trying from
  both ends at once (needs a meeting point we chose not to run).
- Built 2026-10-18 (in 0.5.8, needs a real test): a Windows viewer in
  full screen or game mouse sends the Windows key, Alt+Tab, Alt+Esc,
  Ctrl+Esc and Alt+F4 to the other computer (low-level keyboard hook,
  `windows/syskeys.rs`); Ctrl+Shift+G / Ctrl+Shift+S still work. Check:
  pressing Alt alone afterwards must not leave the stream window stuck in
  Windows' menu mode. Mac viewer (Cmd+Tab) not done: needs an event tap
  with Accessibility permission.
- Built 2026-10-18 (in 0.5.8): light sharpening in `nv12.wgsl`, only when
  the picture is enlarged (amount from screen pixels per video pixel; 0 at
  1:1). Ask the owner whether the Mac's picture on the PC looks crisper,
  or over-sharpened (halos on text).
- Built 2026-10-18 (owner's choices): anonymous usage numbers to PostHog
  (US project, key in `aa_platform::telemetry`). First open asks once, box
  ticked by default; Settings has a switch; file `usage-stats` holds the
  choice; nothing is sent before the question is answered. Notes:
  `app_opened` (once a day: version, os, always-on sharing, number of
  paired computers) and `stream_session` (minutes, avg fps, frozen
  seconds, reconnects, max Mbps, codec, host os, connection kind, game
  mouse/mic/camera used). No person profiles, no GeoIP. Download counts:
  GitHub release asset counts (include auto-updates).
- The repo is PUBLIC (owner confirmed 2026-10-18: keep it public). Never
  commit personal details. A history rewrite removing the owner's home
  address is prepared (branch `cleaned-history`) and needs the owner's OK
  for a force push.
- Beta needs (agreed list): built-in connecting, signed installers (Apple
  developer account and Windows signing still to decide), first-run setup
  without Terminal/sudo, invite a teammate, problem-report button, clear
  "someone is connected" notice with disconnect and view-only, opt-in
  usage numbers.

## The app and releases (2026-10-08)

People now install **Anywhere** instead of running cargo: `crates/aa-app`
(binary `anywhere`, egui/eframe) drives `aa-host`/`aa-viewer` as child
processes (logs in the app-data folder `AnywhereAlternative/`, host
stopped via `--stop-file`). Releases: `.github/workflows/release.yml`
builds `Anywhere-x.y.z-mac.dmg` (ad-hoc signed .app) and
`Anywhere-x.y.z-windows-setup.exe` (Inno Setup, adds firewall rules) and
publishes them on GitHub Releases. Trigger by a `v*` tag or the REST
dispatch (`gh api -X POST .../actions/workflows/release.yml/dispatches -f
ref=main -f 'inputs[version]=x.y.z'`); tag pushes are blocked from Claude
sessions. v0.2.0 is the first release. Unsigned: Mac needs "Open Anyway"
once, Windows SmartScreen "Run anyway". App UI can be screenshotted here
with Xvfb + `WGPU_BACKEND=gl` (`AA_SHOW_SELF=1`, `AA_HOST_MOCK=1`).

## 0.4.2 (2026-10-08): owner feedback round

Owner on 0.4.1 (PC watching Mac, both Wi-Fi): two cursors (fixed: SCK
`showsCursor` false, viewer's own pointer only), blurry/glitchy (logs
requested: PC `viewer.log`, Mac `host.log`, Mac display resolution; not
fixed yet), cropped help text (fixed), wants add-ons installed by default
(Windows installer tasks + `packaging/windows/addons.ps1` via winget /
vb-audio.com; app "Install add-ons" on both, Mac: BlackHole from its
GitHub release + Tailscale pkg) and **fully automatic updates**
(`aa_platform::update` + `aa-app/src/update.rs`; Windows service installs
silently via `update-request`/`update-running`/`relaunch-app` markers in
the shared folder, else one UAC prompt with `/RELAUNCH=1`; Mac swaps the
bundle and kickstarts the agent). Updates wait while `viewer-connected`
(host) or `app-viewing` (app) notes are fresh (<2 min).
Mac releases are signed with the project's own certificate from 0.4.5 on
(owner added `MAC_SIGN_P12`/`MAC_SIGN_PASSWORD`; checked: aa-host in the
0.4.5 dmg carries "Anywhere Alternative Code Signing" and identifier
com.minutecreative.anywhere.aa-host). Permissions should now survive
updates; 0.4.5 itself needs them granted once more (remove + re-add).

## Agreed next steps, in order

1. Owner test of 0.4.0: pair PC↔Mac with the code, reconnect without it,
   Tailscale from another network (phone hotspot), always-on on both:
   switch on, restart, reach the login/sign-in screen from the other one.
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

- You are usually on a **Linux container that cannot compile the Windows,
  macOS or iOS code**. `cargo clippy --all-targets && cargo test` locally
  checks the portable crates (setup below); the platform code is checked
  by **GitHub Actions** on push (ubuntu / macos / windows / iPhone app /
  works with older versions). On failure, CI posts the compiler errors as
  a **commit comment**; read it with
  `gh api repos/minute-creative/anywhere-alternative/commits/<sha>/comments --jq '.[].body'`.
  A full run takes ~15 min (Windows is slowest). Expect one or two rounds
  of API-drift fixes per new platform file. The `gh` CLI here can use the
  REST API only (`gh api ...`); `gh release list`, `gh pr` and other
  GraphQL-backed commands return 403.
- Container facts (2026-10-20): 2 CPUs, 7 GB. A cold `cargo clippy
  --all-targets` takes ~15–20 min and `cargo test` another ~10 (wgpu is
  the heavy crate); incremental checks after that take seconds. Builds
  started in the background only progress while the session is active,
  so don't wrap them in a short `timeout`.
- **crates.io from the Linux container** (2026-10-20): it works only when
  the owner's claude.ai "Allow network egress" setting is on *and* cargo is
  pointed at the session's agent proxy — the container has no DNS of its
  own and `index.crates.io` sits on the environment's `no_proxy` list, so
  plain `cargo fetch` fails with "Could not resolve host". Fix once per
  session, then build normally:
  ```
  printf '[http]\nproxy = "http://127.0.0.1:%s"\ncainfo = "/root/.ccr/ca-bundle.crt"\n[net]\ngit-fetch-with-cli = true\n' \
    "${HTTPS_PROXY##*:}" >> ~/.cargo/config.toml
  env -u no_proxy -u NO_PROXY cargo fetch
  ```
  (The port is whatever `$HTTPS_PROXY` says; `env -u` keeps cargo from
  honouring the bypass list.) With this, `cargo clippy`/`cargo test` run
  here and CI is only needed for the Windows, macOS and iOS code.
- Smoke test that always works locally:
  `aa-host --mock` in one process; read its "pairing code for new
  computers" line; `aa-viewer --headless --mock --pair <code>` (no
  address → discovery) in another; expect `fps=60 loss=0.00%`. The
  pairing is remembered in `AA_DATA_DIR`/the data folder, so later runs
  need no `--pair`. In the claude.ai container the machine's own address
  is `192.0.2.2` (a documentation range the host does not count as a home
  network), so pairing by discovery is refused as "from the internet":
  give the viewer `127.0.0.1:7700` explicitly for the first run. The code
  is printed as `415 051` (with a space); type it without the space.
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
