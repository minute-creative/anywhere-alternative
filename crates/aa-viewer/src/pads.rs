// The slot and kind helpers are only used by the Mac/Windows reader.
#![cfg_attr(not(any(target_os = "macos", target_os = "windows")), allow(dead_code))]
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
//! next one simply corrects it.

use aa_core::input::GamepadKind;

/// Up to four controllers, like every console.
pub const SLOTS: usize = 4;

/// Hands out the lowest free slot, so the first controller is always
/// player 1 and a reconnecting one gets its old place back if free.
#[derive(Debug, Default)]
pub struct SlotTable<K: PartialEq> {
    slots: [Option<K>; SLOTS],
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
            let mut last: HashMap<GamepadId, GamepadState> = HashMap::new();
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
                    let Some(slot) = slots.find(id) else { continue };
                    let state = snapshot(&gilrs.gamepad(id));
                    if last.get(&id) != Some(&state) {
                        last.insert(id, state);
                        // Newest state wins; if the queue is full the next one corrects it.
                        if commands.try_send(ViewerCommand::Input(InputEvent::Gamepad { slot, state })).is_err()
                            && commands.is_closed()
                        {
                            return;
                        }
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
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
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
