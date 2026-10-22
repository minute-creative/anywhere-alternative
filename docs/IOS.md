# Anywhere for iPhone

The iPhone app is a **viewer only**: it watches a Mac or PC that runs the
normal Anywhere host. It is the same Rust viewer the Mac uses (`aa-viewer`),
built as a static library and wrapped in a tiny Xcode app. There is no
iPhone host, and none is planned: iOS doesn't let apps capture the screen
or type into other apps.

Status (2026-10-20, branch `ios-viewer`, on top of 0.5.13): the owner's
iPhone 14 Plus watches the Mac mini over Tailscale at 60 fps, HEVC decoded
in hardware, Razer Kishi working as the controller.

## How it fits together

```
iPhone                                         Mac mini (or PC)
┌──────────────────────────────────┐           ┌───────────────────┐
│ main.m → aa_viewer_ios_main()    │           │ aa-host (service) │
│  reads Settings > Anywhere       │  UDP 7700 │  SCK capture      │
│  run_with_args([...])            │ ◀───────▶ │  VT HEVC encode   │
│  winit + wgpu (Metal) window     │ Tailscale │  virtual DualSense│
│  VideoToolbox decode (shared     │  or LAN   │                   │
│   with the Mac viewer)           │           │                   │
│  GameController pads (Kishi)     │           │                   │
└──────────────────────────────────┘           └───────────────────┘
```

| Piece | File | What it does |
|---|---|---|
| App entry | `packaging/ios/AnywhereiOS/main.m` | Calls `aa_viewer_ios_main()`; never returns. |
| iOS start-up | `crates/aa-viewer/src/ios.rs` | Sets `AA_DATA_DIR` to `Documents/AnywhereData`, logs to `Documents/viewer.log`, reads settings, builds the same words the command line would get, runs the viewer. |
| Viewer as a library | `crates/aa-viewer/src/lib.rs` | The old `main.rs`. `run_cli()` for the Mac/PC program, `run_with_args()` for the iPhone. `main.rs` is one line. |
| Decode and capabilities | `crates/aa-platform/src/ios.rs` | Reuses `macos/decoder.rs` (VideoToolbox) through `#[path]`. Says HEVC + H.264, up to 1920x1080 at 60, gamepad yes, clipboard no. |
| Controllers | `crates/aa-viewer/src/pads.rs`, `mod apple` | Polls `GCController::controllers()` every 4 ms and maps the Kishi (or any MFi/Xbox/PS pad) onto the same DualSense-shaped state gilrs produces on Mac/PC. |
| Window | `crates/aa-viewer/src/window.rs` | Same as Mac. On iOS: no fixed 1280x720 size, and the GPU is asked for the adapter's own limits. |
| Settings | `packaging/ios/AnywhereiOS/Settings.bundle/Root.plist` | Settings > Anywhere: Address, Pairing code, Resolution (720/1080), Max Mbps. |
| App info | `packaging/ios/AnywhereiOS/Info.plist` | Landscape only, full screen, extended gamepad, local-network reason, Files-app sharing (so the log can be read). |
| Xcode project | `packaging/ios/AnywhereiOS.xcodeproj` | Hand-written. One target "Anywhere"; a "Build Rust viewer" script phase runs `build-rust.sh`, then links `libaa_viewer.a`. |
| Rust build | `packaging/ios/build-rust.sh` | `cargo rustc -p aa-viewer --lib --crate-type staticlib --target aarch64-apple-ios`, with a clean environment. |
| Helpers | `packaging/ios/scripts/*.command` | Double-click on the Mac: build, install, update (build + install), get the phone's log, simulator cross-test. |

Identity: bundle id `com.minutecreative.anywhere.ios`, Apple team
`PGYRXU4773` (owner's free/personal developer account), iOS 17.0 minimum.
Free accounts' apps expire after 7 days; re-run the update helper.

## Using it (owner steps)

1. Mac: Anywhere sharing on, Tailscale on. Share page shows the six-digit code.
2. iPhone: Tailscale on (it can't run together with other VPN apps such as
   LocalDevVPN). Settings > Anywhere: Address = the Mac's Tailscale address
   (100.x.y.z) or its home address; Pairing code = the six digits.
3. Open Anywhere. The code is used once and then cleared; next time no code.
4. Kishi: just plug the phone in. It shows up as a PlayStation pad on the Mac.

iOS blocks the broadcast that Mac/PC viewers use to find computers, so the
address is typed once.

## Building, installing, testing (for Claude)

All on the owner's Mac, from `~/anywhere-alternative`:

| Helper | Does | Log / result |
|---|---|---|
| `packaging/ios/scripts/ios-update.command` | build + install on the cabled iPhone | `ios-update-log.txt`, ends with `UPDATE OK` |
| `ios-build.command` | Mac `cargo check`, Rust for iPhone, Xcode build | `ios-build-log.txt` |
| `ios-install.command` | `xcrun devicectl device install app` | `ios-install-log.txt` |
| `ios-getlog.command` | copies `Documents/viewer.log` from the phone | `ios-viewer.log` |
| `ios-simtest.command` | builds for the Simulator, watches this Mac (127.0.0.1), screenshot after 25 s | `sim-shot.png`, `sim-viewer.log` |

Simulator: put a fresh code from the Mac's Share page in
`sim-pair-code.txt` the first time (the simulator is a separate "phone").
The host serves one viewer at a time: with the real phone connected the
simulator gets "host is busy".

The Linux cloud container can't build any of this. CI doesn't build the
iPhone target yet; the Mac is the only compiler for it.

## Gotchas already paid for

- `___chkstk_darwin` undefined at link: set `IPHONEOS_DEPLOYMENT_TARGET=17.0`
  for cargo (`build-rust.sh` does).
- Xcode's environment breaks cargo's build scripts (it exports SDKROOT for
  iOS into Mac programs): `build-rust.sh` runs cargo under `env -i`.
- Picture in the top-left corner, black elsewhere: `with_inner_size(1280,
  720)` on iOS makes a 1139x720 view in a 2778x1284 screen. Not set on iOS.
- Simulator: `max_inter_stage_shader_variables 16 > 15`: wgpu's default
  limits ask for more than the simulator's Metal gives. Request
  `adapter.limits()`.
- "That code isn't right" on every start: the one-use code wasn't cleared
  on disk before the app was killed. Now `synchronize()` right after
  removing it, and if pairing fails the viewer retries as an already-paired
  phone.
- The phone shows up as "localhost" in the Mac's paired list (iOS's
  `gethostname`). Cosmetic; to fix with `UIDevice.name`.
- `vendorName` not found: import the `GCDevice` trait from
  `objc2-game-controller`.

## Not done yet

Audio, touch as mouse, on-screen keyboard, the phone's own name, a start
screen inside the app (instead of Settings), CI build for `aarch64-apple-ios`.
Being watched: occasional "repair frame" skips and VideoToolbox `-12903`
decode errors in the phone log; redraws arriving at ~30 Hz with winit
warnings (overlay still says 60 fps).
