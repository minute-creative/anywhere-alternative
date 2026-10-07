//! The two one-way channels between the window (main thread) and the
//! network session (tokio thread).
//!
//! Frames go window-ward through a *latest-frame slot*, not a queue: if the
//! window is busy and two frames arrive, the older one is simply replaced.
//! Showing a stale frame is never useful. Input goes session-ward through a
//! bounded channel; events are tiny and must not be dropped, so the channel
//! is generous and `try_send` failure is logged as a bug, not tolerated.

use std::sync::{Arc, Mutex};

use aa_core::input::InputEvent;
use aa_platform::DecodedFrame;
use tokio::sync::mpsc;

/// What the window can tell the session.
#[derive(Debug)]
pub enum ViewerCommand {
    Input(InputEvent),
    /// User changed the bitrate cap in the overlay (kbps).
    SetMaxBitrate(u32),
    /// User toggled "mute PC speakers" in the overlay.
    SetHostMute(bool),
    /// User toggled "send my microphone to the PC".
    SetMic(bool),
    /// The window closed; send `Bye` and exit.
    Quit,
}

/// Session → window.
#[derive(Clone)]
pub struct FrameSlot {
    latest: Arc<Mutex<Option<DecodedFrame>>>,
    /// Called after a frame is stored, to wake the window's event loop.
    wake: Arc<dyn Fn() + Send + Sync>,
}

impl std::fmt::Debug for FrameSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FrameSlot").finish_non_exhaustive()
    }
}

impl FrameSlot {
    pub fn new(wake: impl Fn() + Send + Sync + 'static) -> Self {
        Self { latest: Arc::new(Mutex::new(None)), wake: Arc::new(wake) }
    }

    /// Store a frame, replacing any unshown one, and wake the window.
    pub fn publish(&self, frame: DecodedFrame) {
        *self.latest.lock().expect("frame slot poisoned") = Some(frame);
        (self.wake)();
    }

    /// Take the newest frame, if a new one arrived since the last take.
    pub fn take(&self) -> Option<DecodedFrame> {
        self.latest.lock().expect("frame slot poisoned").take()
    }
}

pub fn command_channel() -> (mpsc::Sender<ViewerCommand>, mpsc::Receiver<ViewerCommand>) {
    mpsc::channel(1024)
}
