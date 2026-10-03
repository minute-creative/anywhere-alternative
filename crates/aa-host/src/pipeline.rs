//! The capture → encode thread and the input-injection thread.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use aa_core::input::InputEvent;
use aa_platform::{EncodedFrame, InputInjector, ScreenCapture, VideoEncoder, VirtualGamepad};
use tokio::sync::mpsc;

/// Flags the network task flips to steer the capture thread without locks.
#[derive(Debug, Default)]
pub struct PipelineControl {
    /// A viewer is connected; capture and encode. When false the thread
    /// idles so an unattended host costs nothing.
    pub streaming: AtomicBool,
    /// Viewer lost a frame: begin intra-refresh on the next encode.
    pub intra_refresh: AtomicBool,
    /// Next frame must be a keyframe (new viewer just connected).
    pub force_keyframe: AtomicBool,
    pub shutdown: AtomicBool,
}

/// Runs forever on its own OS thread. Frames go to `tx`; if the network task
/// falls behind, `try_send` drops the frame rather than growing a queue,
/// because a late frame is worth nothing.
pub fn capture_thread(
    mut capture: Box<dyn ScreenCapture>,
    mut encoder: Box<dyn VideoEncoder>,
    ctl: &PipelineControl,
    tx: &mpsc::Sender<EncodedFrame>,
) {
    let idle_poll = Duration::from_millis(50);
    let frame_timeout = Duration::from_millis(100);

    while !ctl.shutdown.load(Ordering::Relaxed) {
        if !ctl.streaming.load(Ordering::Relaxed) {
            std::thread::sleep(idle_poll);
            continue;
        }

        let frame = match capture.next_frame(frame_timeout) {
            Ok(Some(f)) => f,
            Ok(None) => continue, // nothing changed on screen
            Err(e) => {
                tracing::error!("capture failed: {e}");
                std::thread::sleep(Duration::from_millis(500));
                continue;
            }
        };

        if ctl.intra_refresh.swap(false, Ordering::Relaxed) {
            if let Err(e) = encoder.request_intra_refresh() {
                tracing::warn!("intra refresh request failed: {e}");
            }
        }
        let force_key = ctl.force_keyframe.swap(false, Ordering::Relaxed);

        match encoder.encode(&frame, force_key) {
            Ok(packet) => {
                if tx.try_send(packet).is_err() {
                    tracing::debug!("network busy, dropped a frame");
                }
            }
            Err(e) => tracing::error!("encode failed: {e}"),
        }
    }
    tracing::info!("capture thread exiting");
}

/// Applies input events in order on its own thread. Gamepad events are
/// routed to the virtual pad when one exists, everything else to the injector.
pub fn input_thread(
    mut injector: Box<dyn InputInjector>,
    mut gamepad: Option<Box<dyn VirtualGamepad>>,
    mut rx: mpsc::Receiver<InputEvent>,
) {
    while let Some(ev) = rx.blocking_recv() {
        let r = match (ev, gamepad.as_mut()) {
            (InputEvent::Gamepad { slot, state }, Some(pad)) => pad.update(slot, state),
            (ev, _) => injector.inject(ev),
        };
        if let Err(e) = r {
            tracing::warn!("input injection failed: {e}");
        }
    }
    tracing::info!("input thread exiting");
}
