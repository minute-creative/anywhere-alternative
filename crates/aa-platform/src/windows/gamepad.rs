//! Virtual controllers on the PC through the ViGEmBus driver.
//!
//! Windows games read controllers through XInput (Xbox) or raw HID
//! (PlayStation). A program can't pretend to be a USB controller without a
//! kernel driver, and ViGEmBus is the standard one: Steam Link, Parsec and
//! Sunshine use it. It is installed once on the PC
//! (github.com/nefarius/ViGEmBus/releases); after that we can plug in up to
//! four virtual pads, each an Xbox 360 pad or a DualShock 4.
//!
//! A DualSense on the Mac becomes a DualShock 4 here, so games show
//! PlayStation button icons; everything else becomes an Xbox 360 pad, which
//! every PC game supports. ViGEm cannot emulate a DualSense itself, so its
//! adaptive triggers and haptics don't carry over.

#![allow(clippy::pedantic)]

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use aa_core::input::{GamepadKind, GamepadState, Rumble};
use vigem_client::{Client, DS4Report, DualShock4Wired, TargetId, XButtons, XGamepad, Xbox360Wired};

use crate::padmap::{to_ds4, to_xinput};
use crate::{PlatformError, Result, VirtualGamepad};

const SLOTS: usize = 4;

fn err(what: &str, e: vigem_client::Error) -> PlatformError {
    PlatformError::Backend(anyhow::anyhow!("{what}: {e:?}"))
}

enum Pad {
    Xbox(Xbox360Wired<Client>),
    Ds4(DualShock4Wired<Client>),
}

pub struct ViGEmPads {
    client: Client,
    pads: [Option<Pad>; SLOTS],
    rumble: Arc<Mutex<VecDeque<Rumble>>>,
}

impl std::fmt::Debug for ViGEmPads {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ViGEmPads").finish_non_exhaustive()
    }
}

impl ViGEmPads {
    /// Fails if the ViGEmBus driver isn't installed.
    pub fn new() -> Result<Self> {
        let client = Client::connect().map_err(|e| {
            PlatformError::Unavailable(format!(
                "ViGEmBus driver not found ({e:?}); install it once from github.com/nefarius/ViGEmBus/releases \
                 to use controllers"
            ))
        })?;
        Ok(Self { client, pads: Default::default(), rumble: Arc::default() })
    }

    fn plug(&mut self, slot: u8, kind: GamepadKind) -> Result<()> {
        let ix = usize::from(slot);
        if ix >= SLOTS {
            return Ok(());
        }
        // Attach is repeated every couple of seconds (UDP can lose it), so
        // the same kind again must change nothing.
        match (&self.pads[ix], kind) {
            (Some(Pad::Xbox(_)), GamepadKind::Xbox) | (Some(Pad::Ds4(_)), GamepadKind::PlayStation) => return Ok(()),
            _ => {}
        }
        self.unplug(slot);
        let client = self.client.try_clone().map_err(|e| err("ViGEm client", e))?;
        let pad = match kind {
            GamepadKind::Xbox => {
                let mut t = Xbox360Wired::new(client, TargetId::XBOX360_WIRED);
                t.plugin().map_err(|e| err("plug in Xbox pad", e))?;
                t.wait_ready().map_err(|e| err("Xbox pad ready", e))?;
                // Rumble the game asks for, to send back to the viewer.
                if let Ok(req) = t.request_notification() {
                    let queue = Arc::clone(&self.rumble);
                    req.spawn_thread(move |_, n| {
                        let mut q = queue.lock().expect("rumble");
                        q.push_back(Rumble { slot, low_freq: n.large_motor, high_freq: n.small_motor });
                        if q.len() > 16 {
                            q.pop_front();
                        }
                    });
                }
                Pad::Xbox(t)
            }
            GamepadKind::PlayStation => {
                let mut t = DualShock4Wired::new(client, TargetId::DUALSHOCK4_WIRED);
                t.plugin().map_err(|e| err("plug in DualShock 4", e))?;
                t.wait_ready().map_err(|e| err("DualShock 4 ready", e))?;
                Pad::Ds4(t)
            }
        };
        tracing::info!(slot, ?kind, "virtual controller plugged in");
        self.pads[ix] = Some(pad);
        Ok(())
    }

    fn unplug(&mut self, slot: u8) {
        if let Some(pad) = self.pads.get_mut(usize::from(slot)).and_then(Option::take) {
            let _ = match pad {
                Pad::Xbox(mut t) => t.unplug(),
                Pad::Ds4(mut t) => t.unplug(),
            };
            tracing::info!(slot, "virtual controller unplugged");
        }
    }
}

impl VirtualGamepad for ViGEmPads {
    fn attach(&mut self, slot: u8, kind: GamepadKind) -> Result<()> {
        self.plug(slot, kind)
    }

    fn detach(&mut self, slot: u8) -> Result<()> {
        self.unplug(slot);
        Ok(())
    }

    fn update(&mut self, slot: u8, state: GamepadState) -> Result<()> {
        let ix = usize::from(slot);
        if ix >= SLOTS {
            return Ok(());
        }
        // State before any attach (it got lost): assume an Xbox pad.
        if self.pads[ix].is_none() {
            self.plug(slot, GamepadKind::Xbox)?;
        }
        match self.pads[ix].as_mut() {
            Some(Pad::Xbox(t)) => {
                let x = to_xinput(&state);
                t.update(&XGamepad {
                    buttons: XButtons(x.buttons),
                    left_trigger: x.left_trigger,
                    right_trigger: x.right_trigger,
                    thumb_lx: x.thumb_lx,
                    thumb_ly: x.thumb_ly,
                    thumb_rx: x.thumb_rx,
                    thumb_ry: x.thumb_ry,
                })
                .map_err(|e| err("Xbox pad update", e))
            }
            Some(Pad::Ds4(t)) => {
                let d = to_ds4(&state);
                t.update(&DS4Report {
                    thumb_lx: d.thumb_lx,
                    thumb_ly: d.thumb_ly,
                    thumb_rx: d.thumb_rx,
                    thumb_ry: d.thumb_ry,
                    buttons: d.buttons,
                    special: d.special,
                    trigger_l: d.trigger_l,
                    trigger_r: d.trigger_r,
                })
                .map_err(|e| err("DualShock 4 update", e))
            }
            None => Ok(()),
        }
    }

    fn poll_rumble(&mut self) -> Result<Option<Rumble>> {
        Ok(self.rumble.lock().expect("rumble").pop_front())
    }
}

impl Drop for ViGEmPads {
    fn drop(&mut self) {
        for slot in 0..SLOTS as u8 {
            self.unplug(slot);
        }
    }
}
