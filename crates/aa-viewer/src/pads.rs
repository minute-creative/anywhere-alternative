// The slot and kind helpers are only used by the Mac/Windows reader.
#![cfg_attr(not(any(target_os = "macos", target_os = "windows", target_os = "ios")), allow(dead_code))]
#![allow(clippy::doc_markdown)] // controller names read better plain

//! Game controllers on this machine → virtual controllers on the host.
//!
//! A thread watches every connected controller (via `gilrs`, which knows
//! the button layouts of DualSense, DualShock, Xbox, Switch Pro and
//! hundreds of others, over USB or Bluetooth). Each one gets a slot 0-3;
//! the host is told when one appears (and whether it is a PlayStation pad,
//! so it can show PlayStation button icons) or disappears, and receives its
//! full state whenever anything changes, polled at 250 Hz.
//!
//! Why the whole state rather than individual button events: a lost UDP
//! packet would otherwise leave a button stuck down. With snapshots the
//! next one simply corrects it. The state is also repeated every 100 ms
//! while nothing changes, so a lost "button released" can't leave the
//! button held on the host while the controller sits still.
//!
//! In plain words: this file finds controllers plugged into (or paired
//! with) this computer and keeps the other computer's virtual controllers
//! in step with them. DualSense pads may instead be read raw by `ds5.rs`
//! (for rumble, triggers and the touchpad); those are skipped here.

use aa_core::input::GamepadKind;

/// Up to four controllers, like every console.
pub const SLOTS: usize = 4;

/// Hands out the lowest free slot, so the first controller is always
/// player 1 and a reconnecting one gets its old place back if free.
#[derive(Debug)]
pub struct SlotTable<K: PartialEq> {
    slots: [Option<K>; SLOTS],
}

// By hand: a derived Default would demand `K: Default`, which gilrs's
// controller ids don't have (CI caught it; Linux never builds this path).
impl<K: PartialEq> Default for SlotTable<K> {
    fn default() -> Self {
        Self { slots: std::array::from_fn(|_| None) }
    }
}

impl<K: PartialEq + Copy> SlotTable<K> {
    pub fn take(&mut self, key: K) -> Option<u8> {
        if let Some(i) = self.find(key) {
            return Some(i);
        }
        let free = self.slots.iter().position(Option::is_none)?;
        self.slots[free] = Some(key);
        u8::try_from(free).ok()
    }

    pub fn find(&self, key: K) -> Option<u8> {
        self.slots.iter().position(|s| *s == Some(key)).and_then(|i| u8::try_from(i).ok())
    }

    pub fn release(&mut self, key: K) -> Option<u8> {
        let i = self.find(key)?;
        self.slots[usize::from(i)] = None;
        Some(i)
    }
}

/// Sony's USB vendor id.
const SONY: u16 = 0x054C;

/// PlayStation pads get a DualShock 4 on the PC; everything else an Xbox pad.
pub fn kind_of(name: &str, vendor: Option<u16>) -> GamepadKind {
    let n = name.to_ascii_lowercase();
    if vendor == Some(SONY)
        || n.contains("dualsense")
        || n.contains("dualshock")
        || n.contains("ps5")
        || n.contains("ps4")
    {
        GamepadKind::PlayStation
    } else {
        GamepadKind::Xbox
    }
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
mod real {
    use std::collections::HashMap;
    use std::time::{Duration, Instant};

    use aa_core::input::{gamepad_buttons as b, GamepadState, InputEvent};
    use gilrs::{Axis, Button, EventType, GamepadId, Gilrs};
    use tokio::sync::mpsc;

    use super::{kind_of, SlotTable};
    use crate::link::ViewerCommand;

    /// Repeat an unchanged controller state this often (see the file notes).
    const REPEAT_MS: u64 = 100;

    #[allow(clippy::cast_possible_truncation)]
    fn stick(v: f32) -> i16 {
        (v.clamp(-1.0, 1.0) * 32767.0) as i16
    }

    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    fn trigger(pad: &gilrs::Gamepad<'_>, btn: Button) -> u8 {
        let v = pad
            .button_data(btn)
            .map_or_else(|| if pad.is_pressed(btn) { 1.0 } else { 0.0 }, gilrs::ev::state::ButtonData::value);
        (v.clamp(0.0, 1.0) * 255.0) as u8
    }

    fn snapshot(pad: &gilrs::Gamepad<'_>) -> GamepadState {
        let map = [
            (Button::South, b::CROSS),
            (Button::East, b::CIRCLE),
            (Button::West, b::SQUARE),
            (Button::North, b::TRIANGLE),
            (Button::LeftTrigger, b::L1),
            (Button::RightTrigger, b::R1),
            (Button::LeftThumb, b::L3),
            (Button::RightThumb, b::R3),
            (Button::Select, b::SHARE),
            (Button::Start, b::OPTIONS),
            (Button::Mode, b::PS),
            (Button::DPadUp, b::DPAD_UP),
            (Button::DPadDown, b::DPAD_DOWN),
            (Button::DPadLeft, b::DPAD_LEFT),
            (Button::DPadRight, b::DPAD_RIGHT),
        ];
        let buttons = map.iter().filter(|(btn, _)| pad.is_pressed(*btn)).fold(0, |acc, (_, bit)| acc | bit);
        GamepadState {
            buttons,
            left_x: stick(pad.value(Axis::LeftStickX)),
            left_y: stick(pad.value(Axis::LeftStickY)),
            right_x: stick(pad.value(Axis::RightStickX)),
            right_y: stick(pad.value(Axis::RightStickY)),
            left_trigger: trigger(pad, Button::LeftTrigger2),
            right_trigger: trigger(pad, Button::RightTrigger2),
        }
    }

    #[allow(clippy::too_many_lines)] // one reader loop: plug, unplug, hand-over, state
    pub fn spawn(commands: mpsc::Sender<ViewerCommand>) {
        let _ = std::thread::Builder::new().name("aa-pads".into()).spawn(move || {
            let mut gilrs = match Gilrs::new() {
                Ok(g) => g,
                Err(e) => {
                    tracing::warn!("controllers unavailable: {e}");
                    return;
                }
            };
            let mut slots = SlotTable::<GamepadId>::default();
            let mut kinds: HashMap<u8, aa_core::input::GamepadKind> = HashMap::new();
            // Unplug notices still being repeated, with when they started.
            let mut leaving: Vec<(u8, Instant)> = Vec::new();
            let mut last_announce = Instant::now();
            // What each controller last sent, and when (for the repeat).
            let mut last: HashMap<GamepadId, (GamepadState, Instant)> = HashMap::new();
            let send = |ev: InputEvent| commands.blocking_send(ViewerCommand::Input(ev)).is_ok();
            // Controllers already connected at start count as new arrivals.
            let present: Vec<GamepadId> = gilrs.gamepads().map(|(id, _)| id).collect();
            for id in present {
                let pad = gilrs.gamepad(id);
                if let Some(slot) = slots.take(id) {
                    let kind = kind_of(pad.name(), pad.vendor_id());
                    tracing::info!(slot, name = pad.name(), ?kind, "controller connected");
                    kinds.insert(slot, kind);
                    send(InputEvent::GamepadAttach { slot, kind });
                }
            }
            loop {
                while let Some(ev) = gilrs.next_event() {
                    match ev.event {
                        EventType::Connected => {
                            let pad = gilrs.gamepad(ev.id);
                            if let Some(slot) = slots.take(ev.id) {
                                let kind = kind_of(pad.name(), pad.vendor_id());
                                tracing::info!(slot, name = pad.name(), ?kind, "controller connected");
                                kinds.insert(slot, kind);
                                leaving.retain(|(s, _)| *s != slot);
                                if !send(InputEvent::GamepadAttach { slot, kind }) {
                                    return;
                                }
                            }
                        }
                        EventType::Disconnected => {
                            last.remove(&ev.id);
                            if let Some(slot) = slots.release(ev.id) {
                                tracing::info!(slot, "controller disconnected");
                                kinds.remove(&slot);
                                leaving.push((slot, Instant::now()));
                                if !send(InputEvent::GamepadDetach { slot }) {
                                    return;
                                }
                            }
                        }
                        _ => {}
                    }
                }
                // Input packets can be lost; plug/unplug notices matter, so
                // repeat them every 2 s (repeats are harmless on the host).
                if last_announce.elapsed() >= Duration::from_secs(2) {
                    last_announce = Instant::now();
                    for (&slot, &kind) in &kinds {
                        send(InputEvent::GamepadAttach { slot, kind });
                    }
                    leaving.retain(|(_, since)| since.elapsed() < Duration::from_secs(7));
                    for &(slot, _) in &leaving {
                        send(InputEvent::GamepadDetach { slot });
                    }
                }
                let ids: Vec<GamepadId> = gilrs.gamepads().map(|(id, _)| id).collect();
                for id in ids {
                    // DualSenses read directly (crate::ds5) are not ours: hand
                    // them over, or the PC would see every press twice.
                    let pad = gilrs.gamepad(id);
                    if crate::ds5::raw_active() && crate::ds5::is_dualsense(pad.vendor_id(), pad.product_id()) {
                        if let Some(slot) = slots.release(id) {
                            tracing::info!(slot, "DualSense handed to direct pass-through");
                            kinds.remove(&slot);
                            last.remove(&id);
                            leaving.push((slot, Instant::now()));
                            send(InputEvent::GamepadDetach { slot });
                        }
                        continue;
                    }
                    let Some(slot) = slots.find(id).or_else(|| {
                        // Re-adopt a pad handed over earlier whose direct
                        // reader has since let go.
                        let slot = slots.take(id)?;
                        let kind = kind_of(pad.name(), pad.vendor_id());
                        kinds.insert(slot, kind);
                        leaving.retain(|(s, _)| *s != slot);
                        send(InputEvent::GamepadAttach { slot, kind });
                        Some(slot)
                    }) else {
                        continue;
                    };
                    let state = snapshot(&gilrs.gamepad(id));
                    let due = last
                        .get(&id)
                        .map_or(true, |(prev, at)| *prev != state || at.elapsed() >= Duration::from_millis(REPEAT_MS));
                    if due {
                        match commands.try_send(ViewerCommand::Input(InputEvent::Gamepad { slot, state })) {
                            Ok(()) => {
                                last.insert(id, (state, Instant::now()));
                            }
                            // Queue full: leave `last` alone so the next pass
                            // tries again. (Before 0.5.8 `last` was updated
                            // first, so a state that didn't fit in the queue
                            // was never sent: a release could be lost for good.)
                            Err(mpsc::error::TrySendError::Full(_)) => {}
                            Err(mpsc::error::TrySendError::Closed(_)) => return,
                        }
                    }
                }
                std::thread::sleep(Duration::from_millis(4));
            }
        });
    }
}

/// iPhone/iPad: Apple's `GameController` framework instead of gilrs (which
/// has no iOS backend). It sees MFi pads like the Razer Kishi and Bluetooth
/// Xbox/PlayStation pads, with the same face-button layout as gilrs's
/// South/East/West/North. Same slots, same 250 Hz snapshots, same 100 ms
/// repeats and 2 s plug notices as the Mac/PC reader above.
#[cfg(target_os = "ios")]
mod apple {
    use std::collections::{HashMap, HashSet};
    use std::time::{Duration, Instant};

    use aa_core::input::{gamepad_buttons as b, GamepadKind, GamepadState, InputEvent};
    use objc2::rc::{autoreleasepool, Retained};
    use objc2_game_controller::{GCController, GCControllerButtonInput, GCDevice, GCExtendedGamepad};
    use tokio::sync::mpsc;

    use super::{kind_of, SlotTable};
    use crate::link::ViewerCommand;

    const REPEAT_MS: u64 = 100;

    #[allow(clippy::cast_possible_truncation)]
    fn stick(v: f32) -> i16 {
        (v.clamp(-1.0, 1.0) * 32767.0) as i16
    }

    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    fn trigger(v: f32) -> u8 {
        (v.clamp(0.0, 1.0) * 255.0) as u8
    }

    fn pressed(btn: Option<Retained<GCControllerButtonInput>>) -> bool {
        btn.is_some_and(|b| unsafe { b.isPressed() })
    }

    fn snapshot(gp: &GCExtendedGamepad) -> GamepadState {
        unsafe {
            let dpad = gp.dpad();
            let map = [
                (Some(gp.buttonA()), b::CROSS),
                (Some(gp.buttonB()), b::CIRCLE),
                (Some(gp.buttonX()), b::SQUARE),
                (Some(gp.buttonY()), b::TRIANGLE),
                (Some(gp.leftShoulder()), b::L1),
                (Some(gp.rightShoulder()), b::R1),
                (gp.leftThumbstickButton(), b::L3),
                (gp.rightThumbstickButton(), b::R3),
                (gp.buttonOptions(), b::SHARE),
                (Some(gp.buttonMenu()), b::OPTIONS),
                (gp.buttonHome(), b::PS),
                (Some(dpad.up()), b::DPAD_UP),
                (Some(dpad.down()), b::DPAD_DOWN),
                (Some(dpad.left()), b::DPAD_LEFT),
                (Some(dpad.right()), b::DPAD_RIGHT),
            ];
            let buttons = map.into_iter().filter(|(btn, _)| pressed(btn.clone())).fold(0, |acc, (_, bit)| acc | bit);
            let (ls, rs) = (gp.leftThumbstick(), gp.rightThumbstick());
            GamepadState {
                buttons,
                left_x: stick(ls.xAxis().value()),
                left_y: stick(ls.yAxis().value()),
                right_x: stick(rs.xAxis().value()),
                right_y: stick(rs.yAxis().value()),
                left_trigger: trigger(gp.leftTrigger().value()),
                right_trigger: trigger(gp.rightTrigger().value()),
            }
        }
    }

    /// Every connected controller that has the full gamepad layout, keyed by
    /// its object address (stable while it stays connected).
    fn connected() -> Vec<(usize, String, Retained<GCExtendedGamepad>)> {
        let mut out = Vec::new();
        unsafe {
            for c in &GCController::controllers() {
                let Some(gp) = c.extendedGamepad() else { continue };
                let name = c.vendorName().map_or_else(|| "controller".to_owned(), |n| n.to_string());
                out.push((Retained::as_ptr(&c) as usize, name, gp));
            }
        }
        out
    }

    pub fn spawn(commands: mpsc::Sender<ViewerCommand>) {
        let _ = std::thread::Builder::new().name("aa-pads".into()).spawn(move || {
            let mut slots = SlotTable::<usize>::default();
            let mut kinds: HashMap<u8, GamepadKind> = HashMap::new();
            let mut leaving: Vec<(u8, Instant)> = Vec::new();
            let mut last: HashMap<usize, (GamepadState, Instant)> = HashMap::new();
            let mut known: HashSet<usize> = HashSet::new();
            let mut last_announce = Instant::now();
            let send = |ev: InputEvent| commands.blocking_send(ViewerCommand::Input(ev)).is_ok();
            loop {
                let alive = autoreleasepool(|_| {
                    let pads = connected();
                    // Unplugged since last pass.
                    let gone: Vec<usize> =
                        known.iter().copied().filter(|k| !pads.iter().any(|(id, _, _)| id == k)).collect();
                    for id in gone {
                        known.remove(&id);
                        last.remove(&id);
                        if let Some(slot) = slots.release(id) {
                            tracing::info!(slot, "controller disconnected");
                            kinds.remove(&slot);
                            leaving.push((slot, Instant::now()));
                            if !send(InputEvent::GamepadDetach { slot }) {
                                return false;
                            }
                        }
                    }
                    for (id, name, gp) in &pads {
                        let slot = match slots.find(*id) {
                            Some(s) => s,
                            None => {
                                let Some(slot) = slots.take(*id) else { continue };
                                let kind = kind_of(name, None);
                                tracing::info!(slot, name, ?kind, "controller connected");
                                kinds.insert(slot, kind);
                                known.insert(*id);
                                leaving.retain(|(s, _)| *s != slot);
                                if !send(InputEvent::GamepadAttach { slot, kind }) {
                                    return false;
                                }
                                slot
                            }
                        };
                        let state = snapshot(gp);
                        let due = last.get(id).map_or(true, |(prev, at)| {
                            *prev != state || at.elapsed() >= Duration::from_millis(REPEAT_MS)
                        });
                        if due {
                            match commands.try_send(ViewerCommand::Input(InputEvent::Gamepad { slot, state })) {
                                Ok(()) => {
                                    last.insert(*id, (state, Instant::now()));
                                }
                                Err(mpsc::error::TrySendError::Full(_)) => {}
                                Err(mpsc::error::TrySendError::Closed(_)) => return false,
                            }
                        }
                    }
                    true
                });
                if !alive {
                    return;
                }
                if last_announce.elapsed() >= Duration::from_secs(2) {
                    last_announce = Instant::now();
                    for (&slot, &kind) in &kinds {
                        send(InputEvent::GamepadAttach { slot, kind });
                    }
                    leaving.retain(|(_, since)| since.elapsed() < Duration::from_secs(7));
                    for &(slot, _) in &leaving {
                        send(InputEvent::GamepadDetach { slot });
                    }
                }
                std::thread::sleep(Duration::from_millis(4));
            }
        });
    }
}

/// Start watching controllers; events go out through the command channel.
pub fn spawn(commands: tokio::sync::mpsc::Sender<crate::link::ViewerCommand>) {
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    real::spawn(commands);
    #[cfg(target_os = "ios")]
    apple::spawn(commands);
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "ios")))]
    drop(commands);
}

/// `--test-gamepad`: a pretend DualSense in slot 0 that circles its left
/// stick and taps Cross once a second, for mock runs.
pub fn spawn_test(commands: tokio::sync::mpsc::Sender<crate::link::ViewerCommand>) {
    use aa_core::input::{gamepad_buttons as b, GamepadState, InputEvent};
    let _ = std::thread::Builder::new().name("aa-test-pad".into()).spawn(move || {
        let send = |ev| commands.blocking_send(crate::link::ViewerCommand::Input(ev)).is_ok();
        if !send(InputEvent::GamepadAttach { slot: 0, kind: GamepadKind::PlayStation }) {
            return;
        }
        let mut t = 0f32;
        loop {
            #[allow(clippy::cast_possible_truncation)]
            let state = GamepadState {
                buttons: if t.fract() < 0.1 { b::CROSS } else { 0 },
                left_x: (t.sin() * 30000.0) as i16,
                left_y: (t.cos() * 30000.0) as i16,
                ..GamepadState::default()
            };
            if !send(InputEvent::Gamepad { slot: 0, state }) {
                return;
            }
            t += 0.004;
            std::thread::sleep(std::time::Duration::from_millis(4));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slots_fill_lowest_first_and_reuse() {
        let mut t = SlotTable::<u32>::default();
        assert_eq!(t.take(10), Some(0));
        assert_eq!(t.take(11), Some(1));
        assert_eq!(t.take(10), Some(0), "same pad keeps its slot");
        assert_eq!(t.release(10), Some(0));
        assert_eq!(t.take(12), Some(0), "freed slot is reused");
        t.take(13);
        t.take(14);
        assert_eq!(t.take(15), None, "only four");
    }

    #[test]
    fn playstation_pads_are_recognised() {
        assert_eq!(kind_of("DualSense Wireless Controller", Some(0x054C)), GamepadKind::PlayStation);
        assert_eq!(kind_of("Wireless Controller", Some(0x054C)), GamepadKind::PlayStation);
        assert_eq!(kind_of("PS4 Controller", None), GamepadKind::PlayStation);
        assert_eq!(kind_of("Xbox Wireless Controller", Some(0x045E)), GamepadKind::Xbox);
        assert_eq!(kind_of("Pro Controller", Some(0x057E)), GamepadKind::Xbox);
    }
}
