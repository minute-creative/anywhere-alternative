# Anywhere on the iPhone (with a Razer Kishi V2)

Your Mac mini on the phone, driven entirely by the controller clipped to
it: a console-style home, a launcher for the Mac's apps and games, the
Mac's picture at 1080p or 720p up to 75 fps, its sound, and the controller
translated into pointer, keys or a game controller. Touch works too, but
nothing needs it.

## What you need (all free)

- The Mac mini sharing with Anywhere **0.6 or newer** (the launcher needs
  it; the picture works with older hosts).
- An iPhone (iOS 17+) and a Razer Kishi V2 (any iOS controller works).
- A Mac with **Xcode** (free, App Store) to build and install the app. The
  Mac mini itself is fine.
- **XcodeGen** (`brew install xcodegen`) and **Rust** (`rustup`), both
  free.
- Away from home: the **Tailscale** app on the phone (App Store, free)
  signed in to the same account as the Mac.

Free Apple ID signing has two limits you will notice: the app must be
re-installed from Xcode every **7 days** (plug the phone in, press Run),
and it only installs on **your** phone. The $99 developer program removes
both; the app itself does not change.

## Build and install (on the Mac)

```sh
cd ~/anywhere-alternative
rustup target add aarch64-apple-ios
brew install xcodegen
cd ios && xcodegen generate && open Anywhere.xcodeproj
```

In Xcode, once:
1. Click the **Anywhere** project → target **Anywhere** → *Signing &
   Capabilities* → tick *Automatically manage signing* and pick your
   personal team (your Apple ID; add it under Xcode → Settings → Accounts
   if it is not there).
2. Plug in the iPhone, choose it at the top, press **Run** (⌘R). The first
   build also compiles the Rust engine for the phone (a few minutes).
3. On the phone, the first time: Settings → General → VPN & Device
   Management → trust your developer app. Then open Anywhere.

The Xcode project is generated (it is in `.gitignore`); run `xcodegen
generate` again after pulling changes that add Swift files.

## First time: pair

1. On the Mac: Anywhere → **Share** on, note the six-digit code.
2. On the phone: **Add a computer** → the Mac appears (same Wi-Fi; allow
   "local network" when iOS asks) → type the code on the number pad with
   the d-pad and A. Away from home, type the Mac's Tailscale address
   (100.x.y.z) in the box instead.
3. The Mac is now on your Home screen; **A** connects, with no code, from
   anywhere the Mac can be reached.

## Controls

Everywhere: d-pad or left stick moves, **A** confirms, **B** goes back,
**Home** (the Kishi's Nexus button; or Menu + Options together) opens the
quick menu while watching. The hints bar at the bottom of each screen
always lists what the buttons do there.

While watching the Mac — **Desktop** controls (the default):

| Control | Does |
|---|---|
| Left stick | Pointer (L3 toggles slow / fast) |
| Right stick | Scroll |
| A / R2 | Click (hold R2 to drag) |
| B / L2 | Right-click |
| R3 | Middle click |
| X | On-screen keyboard |
| Y | Launcher |
| L1 (hold) | Command ⌘ |
| R1 (hold) | Shift ⇧ |
| D-pad | Arrow keys |
| Menu ≡ | Return |
| Options ⧉ | Escape |
| Home | Quick menu |

**Game** controls (keyboard + mouse games, e.g. through CrossOver): left
stick = W A S D, right stick = mouse look, R2 / L2 = mouse buttons, A
Space, B Ctrl, X R, Y E, L1 Shift, R1 Q, L3 Shift, R3 F, d-pad 1 2 3 4,
Menu Esc, Options Tab.

**Controller pass-through** sends the pad as a controller. That works when
the host can show a virtual controller: a PC with ViGEmBus. **A Mac
cannot**, without Apple's virtual-HID entitlement (paid program +
approval; see JOURNAL Day 6) — use Desktop or Game controls on the Mac.

Launcher (Y): rows *Open now*, *Recent*, *Games* (Steam, CrossOver) and
*Apps*. A opens or switches to; X quits a running app; Y hides it; L1 / R1
jump rows. Icons arrive a moment after the list.

Keyboard (X): d-pad moves, A types, B deletes, X space, Y shift, Menu
return, L1 symbols, R1 tab, Options closes. *Phone keyboard* on the last
row brings up iOS's keyboard for longer text.

Touch, with no controller: tap = click, long press = right-click, drag =
move the pointer, two fingers = scroll, the ≡ button top-right = quick
menu.

## Settings

Picture 1080p / 720p, frame rate 60 / 75, data cap, controls profile,
pointer speed, mute the Mac's speakers, stats corner, and *Problems* (the
engine's log; the full file is `viewer.log` in the app's Documents
folder — Files app → On My iPhone → Anywhere).

## How it is built

- `crates/aa-mobile`: the viewer library (`crates/aa-viewer` without its
  window) behind a C interface (`include/aa_mobile.h`): find, pair,
  connect, frames, sound, input, the launcher.
- `crates/aa-platform/src/ios`: VideoToolbox decoding, the same code as
  the Mac viewer.
- `ios/`: the Swift app. `Engine.swift` talks to the C interface;
  `Pad.swift` reads the controller and runs menu navigation with
  key-repeat; `InputMapper.swift` is the controls table above;
  `StreamView.swift` draws frames with Metal and handles touch;
  `LauncherView`, `QuickMenu`, `KeyboardView`, `HomeView`, `PairView`,
  `SettingsView` are the screens.
- The launcher protocol: `AppList` → `Apps` pages → icons as a named blob
  on the clipboard transfer (`aa_core::apps`). On the Mac,
  `aa_platform::apps` lists `/Applications`, `~/Applications` (incl.
  CrossOver launchers), `/System/Applications` and Steam's library, and
  opens things with `open` (apps, `steam://rungameid/…`).
- CI builds the engine for the simulator and the Swift app unsigned on
  every push ("iPhone app" job).
