---
name: anywhere-ios-viewer
description: "Use for anything about Anywhere's iPhone app (the iOS viewer): changing its code, building and installing it on the owner's iPhone, cross-testing in the Simulator, reading the phone's log, controllers (Kishi), pairing from the phone, or its known issues."
---

# Anywhere for iPhone

The iPhone app is the normal Rust viewer (`crates/aa-viewer`) compiled as a
static library and wrapped in a tiny Xcode app (`packaging/ios`). It only
watches; the Mac or PC runs the host. Full map: `docs/IOS.md`. Read it first.
For running things on the owner's Mac, see the `anywhere-owner-mac` skill.

## Facts to keep in mind

- Owner's phone: iPhone 14 Plus (A15, 6 GB), screen 2778x1284, Razer Kishi.
  Owner's Mac: Mac mini M4 Pro, Xcode 26.x, Rust via rustup.
- Bundle id `com.minutecreative.anywhere.ios`, team `PGYRXU4773`, iOS 17.0
  minimum. Free developer account: the install expires after 7 days; the
  phone must trust the developer once (Settings > General > VPN & Device
  Management).
- Settings live in the iOS Settings app (Settings > Anywhere): `host`,
  `pair_code` (one use, then cleared), `resolution` (720/1080), `max_mbps`.
  `aa_viewer_ios_main` turns them into `aa-viewer <host> --fullscreen
  --resolution N [--max-mbps M] [--pair CODE]`.
- iOS blocks LAN broadcast, so the phone connects by address: the Mac's
  Tailscale 100.x address from anywhere, or its home address on Wi-Fi.
  Tailscale on the phone can't run alongside another VPN app.
- Pairing keys: app `Documents/AnywhereData`; log: `Documents/viewer.log`.
- iOS code paths are `#[cfg(target_os = "ios")]`. Video decode reuses
  `aa-platform/src/macos/decoder.rs`. Controllers: `pads.rs` `mod apple`
  (GameController framework). Anything shared with the Mac viewer must
  still build for macOS, Windows and Linux.
- The Linux container and CI can't build the iPhone target. Only the
  owner's Mac can.

## Workflow for a change

1. Edit in the repo. Keep iOS-only code behind `cfg(target_os = "ios")`.
2. Portable checks here: `cargo fmt --all && cargo clippy --all-targets &&
   cargo test` (these don't cover iOS code).
3. Get the change onto the Mac's `~/anywhere-alternative` (push and
   `git pull`, or copy the files through the linked computer).
4. Cross-test in the Simulator first: `packaging/ios/scripts/ios-simtest.command`.
   Read `sim-viewer.log` and look at `sim-shot.png` (fills the screen?
   "secure connection"? fps lines?). First run needs a fresh code from the
   Mac's Share page in `sim-pair-code.txt`. Disconnect the real phone first:
   the host takes one viewer at a time ("host is busy").
5. Phone on cable, unlocked: `packaging/ios/scripts/ios-update.command`.
   Wait for `UPDATE OK` in `ios-update-log.txt`.
6. Ask the owner to close the app fully, reopen it, and send a screenshot
   with the overlay (fps, Mbps, ms, loss). Pull the phone's log with
   `ios-getlog.command` → `ios-viewer.log`.
7. Only call it fixed when the phone itself shows it. The Simulator is a
   check, not proof (different GPU limits, no Kishi).

## How to judge the log

- Good: `secure connection`, `negotiated ... Hevc`, steady `fps=60`,
  `loss=0.00%`, `controller connected` on Kishi attach.
- `That code isn't right`: stale or mistyped code. The app retries as an
  already-paired phone; if that fails too, get a fresh code.
- `host is busy`: another viewer (Mac, PC or Simulator) is connected.
- `window size changed` with a small size: the view isn't full screen.
- `repair frame` / VideoToolbox `-12903`: known, being watched (loss
  recovery); only act if the owner sees freezes.

## Lessons already paid for (don't repeat)

- Link error `___chkstk_darwin`: `IPHONEOS_DEPLOYMENT_TARGET=17.0` for cargo.
- Cargo inside Xcode breaks build scripts: run it under `env -i`
  (`build-rust.sh`).
- Never set a window size on iOS (`with_inner_size`): it shrinks the view
  into the top-left corner.
- Ask wgpu for `adapter.limits()`, not the defaults (Simulator rejects them).
- After `removeObjectForKey` on a one-use setting, call `synchronize()`;
  iOS may kill the app before it saves.
- Import `GCDevice` to use `vendorName` on a controller.
- `device_commit_files` to the Mac can resend an old copy when a staged
  path is reused: stage under a fresh name and compare md5 on both sides.

## Open items

Audio, touch as mouse, on-screen keyboard, showing the iPhone's name
instead of "localhost" on the host, a start screen inside the app, a CI
build for `aarch64-apple-ios`, 30 Hz redraw cadence, `-12903` decode errors.
