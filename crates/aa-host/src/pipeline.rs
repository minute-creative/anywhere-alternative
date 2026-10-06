//! The capture → encode thread and the input-injection thread.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
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
    /// Target bitrate from the adaptive controller; 0 = unchanged.
    pub target_kbps: AtomicU32,
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
    let mut encode_failures = 0u32;

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
        let kbps = ctl.target_kbps.swap(0, Ordering::Relaxed);
        if kbps != 0 {
            match encoder.set_bitrate_kbps(kbps) {
                Ok(()) => tracing::info!(kbps, "encoder bitrate updated"),
                Err(e) => tracing::warn!("set bitrate failed: {e}"),
            }
        }

        match encoder.encode(&frame, force_key) {
            Ok(packet) => {
                if tx.try_send(packet).is_err() {
                    tracing::debug!("network busy, dropped a frame");
                }
            }
            Err(e) => {
                encode_failures += 1;
                if encode_failures == 1 || encode_failures % 60 == 0 {
                    tracing::error!(encode_failures, "encode failed: {e}");
                }
                // Give the driver a moment; a wedged encoder usually clears on
                // its next event cycle. Force a keyframe so the viewer resyncs.
                ctl.force_keyframe.store(true, Ordering::Relaxed);
                std::thread::sleep(Duration::from_millis(20));
                continue;
            }
        }
        encode_failures = 0;
    }
    tracing::info!("capture thread exiting");
}

/// Encoded audio packet ready for the wire (header + opus payload).
#[derive(Debug)]
pub struct AudioPacket {
    pub data: bytes::Bytes,
}

/// Captures system audio, encodes 10 ms Opus frames, hands them to the
/// sender. Silent frames are not sent at all. Runs on its own OS thread
/// because WASAPI waits are blocking.
pub fn audio_thread(
    mut capture: Box<dyn aa_platform::audio::AudioCapture>,
    ctl: &PipelineControl,
    tx: &mpsc::Sender<AudioPacket>,
) {
    use aa_core::audio::{AudioHeader, DEFAULT_BITRATE, HEADER_LEN};
    use aa_platform::audio::{OpusEncoder, FRAME_LEN_I16};
    use bytes::BufMut;

    let mut encoder = match OpusEncoder::new(DEFAULT_BITRATE) {
        Ok(e) => e,
        Err(e) => {
            tracing::error!("audio encoder unavailable: {e}");
            return;
        }
    };
    let mut pcm = vec![0i16; FRAME_LEN_I16];
    let mut frame_no: u16 = 0;
    let started = std::time::Instant::now();
    let mut failures = 0u32;

    while !ctl.shutdown.load(Ordering::Relaxed) {
        if !ctl.streaming.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(50));
            continue;
        }
        let has_sound = match capture.next_frame(&mut pcm) {
            Ok(s) => s,
            Err(e) => {
                failures += 1;
                if failures == 1 || failures % 100 == 0 {
                    tracing::warn!(failures, "audio capture failed: {e}");
                }
                std::thread::sleep(Duration::from_millis(20));
                continue;
            }
        };
        failures = 0;
        if !has_sound {
            // Keep the sequence contiguous so the player doesn't conceal
            // "losses" that were silence; it simply gets nothing to play.
            continue;
        }
        let payload = match encoder.encode(&pcm) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!("audio encode failed: {e}");
                continue;
            }
        };
        let mut buf = bytes::BytesMut::with_capacity(HEADER_LEN + payload.len());
        AudioHeader { ts_ms: started.elapsed().as_millis() as u32, frame_no }.write(&mut buf);
        buf.put_slice(payload);
        frame_no = frame_no.wrapping_add(1);
        if tx.try_send(AudioPacket { data: buf.freeze() }).is_err() {
            tracing::trace!("audio queue full; dropping frame");
        }
    }
    tracing::info!("audio thread exiting");
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
