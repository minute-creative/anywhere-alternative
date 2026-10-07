//! The capture → encode thread and the input-injection thread.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering};
use std::time::Duration;

use aa_core::video::Codec;

use aa_core::input::InputEvent;
use aa_platform::{EncodedFrame, InputInjector, PlatformError, ScreenCapture, VideoEncoder, VirtualGamepad};
use tokio::sync::mpsc;

/// Flags the network task flips to steer the capture thread without locks.
#[derive(Debug)]
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
    /// Codec the current viewer negotiated (`Codec as u8`); `NO_CODEC` =
    /// keep whatever encoder is loaded.
    pub codec: AtomicU8,
    pub shutdown: AtomicBool,
    /// Why the screen can't be shown right now ("PC is locked"), for the
    /// viewer; the network task forwards changes.
    pub screen_status: std::sync::Mutex<Option<&'static str>>,
}

/// Sentinel for `PipelineControl::codec`: no change requested.
pub const NO_CODEC: u8 = u8::MAX;

impl Default for PipelineControl {
    fn default() -> Self {
        Self {
            streaming: AtomicBool::new(false),
            intra_refresh: AtomicBool::new(false),
            force_keyframe: AtomicBool::new(false),
            target_kbps: AtomicU32::new(0),
            codec: AtomicU8::new(NO_CODEC),
            shutdown: AtomicBool::new(false),
            screen_status: std::sync::Mutex::new(None),
        }
    }
}

/// Consecutive capture errors before we give up on this capture and build
/// a fresh one (~10 s at the 250 ms back-off).
const CAPTURE_GIVE_UP: u32 = 40;
/// Consecutive encode errors before rebuilding the encoder, and before
/// rebuilding everything.
const ENCODE_REBUILD: u32 = 30;
const ENCODE_GIVE_UP: u32 = 120;

/// The capture side of a host: what grabs the screen, what encodes it, and
/// how to make new ones. Owned by the capture thread.
pub struct Pipeline {
    pub capture: Box<dyn ScreenCapture>,
    pub encoder: Box<dyn VideoEncoder>,
    pub codec: Codec,
    pub factory: Option<aa_platform::EncoderFactory>,
    pub rebuild: Option<aa_platform::PipelineFactory>,
}

/// Runs forever on its own OS thread. Frames go to `tx`; if the network task
/// falls behind, `try_send` drops the frame rather than growing a queue,
/// because a late frame is worth nothing.
///
/// Self-healing, because a host nobody is sitting at must never need a
/// restart: the screen changing size or refresh rate rebuilds the encoder at
/// the new size; capture or encode failing for long rebuilds the whole
/// pipeline (new GPU device, display re-picked). The first version did
/// neither and a resolution change or GPU reset left it stuck for good.
#[allow(clippy::too_many_lines)] // one loop, one place to read the recovery rules
#[allow(clippy::needless_pass_by_value)]
pub fn capture_thread(p: Pipeline, ctl: &PipelineControl, tx: &mpsc::Sender<EncodedFrame>) {
    let Pipeline { mut capture, encoder, codec: initial_codec, mut factory, mut rebuild } = p;
    let mut encoder: Option<Box<dyn VideoEncoder>> = Some(encoder);
    let idle_poll = Duration::from_millis(50);
    let frame_timeout = Duration::from_millis(100);
    let mut encode_failures = 0u32;
    let mut capture_failures = 0u32;
    let mut current_codec = initial_codec;
    let mut enc_res = capture.resolution();
    let mut enc_fps = capture.refresh_rate_hz();
    // Last bitrate applied, to give a rebuilt encoder the same target.
    let mut kbps_now = 0u32;
    let mut awake = false;
    let mut status: Option<&'static str> = None;
    // Set when the viewer asked for a codec we could not load. While set we
    // send nothing: frames in the wrong codec decode to garbage or black,
    // which is worse than an honest "no video" plus an error in this log.
    let mut wrong_codec: Option<Codec> = None;
    // Frame numbers on the wire must only ever go up: the viewer drops
    // anything not newer than what it already showed. Each new encoder
    // counts from 0, so we number frames here instead. (Forgetting this made
    // the viewer discard every frame after the first encoder rebuild.)
    let mut next_frame_id: u32 = 0;

    while !ctl.shutdown.load(Ordering::Relaxed) {
        let streaming = ctl.streaming.load(Ordering::Relaxed);
        if streaming != awake {
            aa_platform::keep_awake(streaming);
            awake = streaming;
        }
        if !streaming {
            std::thread::sleep(idle_poll);
            continue;
        }

        // A viewer negotiated a codec: make sure that is what we encode.
        let wanted = ctl.codec.swap(NO_CODEC, Ordering::Relaxed);
        if let Some(codec) = Codec::from_u8(wanted) {
            wrong_codec = None;
            if codec == current_codec {
                tracing::info!(?codec, "encoder already matches the negotiated codec");
            } else {
                match factory.as_mut().map(|f| f(codec, enc_res, enc_fps)) {
                    Some(Ok(enc)) => {
                        encoder = Some(enc);
                        current_codec = codec;
                        tracing::info!(?codec, "encoder switched");
                    }
                    Some(Err(e)) => {
                        tracing::error!(?codec, "could not build encoder ({e}); sending no video this session");
                        wrong_codec = Some(codec);
                    }
                    None => {
                        tracing::error!(?codec, "no encoder factory; sending no video this session");
                        wrong_codec = Some(codec);
                    }
                }
            }
        }
        if wrong_codec.is_some() {
            std::thread::sleep(idle_poll);
            continue;
        }

        let reason = capture.unavailable_reason();
        if reason != status {
            status = reason;
            if let Some(r) = reason {
                tracing::info!("screen unavailable: {r}");
            } else {
                tracing::info!("screen available again");
            }
            *ctl.screen_status.lock().expect("status") = reason;
            if reason.is_none() {
                ctl.force_keyframe.store(true, Ordering::Relaxed);
            }
        }

        let frame = match capture.next_frame(frame_timeout) {
            Ok(Some(f)) => {
                capture_failures = 0;
                f
            }
            Ok(None) => continue, // nothing changed on screen
            Err(PlatformError::DeviceLost(why)) => {
                tracing::warn!("capture device lost ({why}); rebuilding the video pipeline");
                rebuild_pipeline(&mut capture, &mut factory, &mut encoder, &mut rebuild, ctl);
                continue;
            }
            Err(e) => {
                capture_failures += 1;
                if capture_failures == 1 || capture_failures % 20 == 0 {
                    tracing::warn!(capture_failures, "capture failed: {e}");
                }
                std::thread::sleep(Duration::from_millis(250));
                if capture_failures >= CAPTURE_GIVE_UP {
                    tracing::warn!("capture keeps failing; rebuilding the video pipeline");
                    capture_failures = 0;
                    rebuild_pipeline(&mut capture, &mut factory, &mut encoder, &mut rebuild, ctl);
                }
                continue;
            }
        };

        // The screen changed size or refresh rate (display settings, a
        // monitor plugged in, lid closed onto an external screen), or the
        // encoder was dropped: build one for what the screen is now.
        let fps_now = capture.refresh_rate_hz();
        if encoder.is_none() || frame.resolution != enc_res || fps_now != enc_fps {
            if encoder.is_some() {
                tracing::info!(res = ?frame.resolution, fps = fps_now, "screen changed; rebuilding encoder");
            }
            match factory.as_mut().map(|f| f(current_codec, frame.resolution, fps_now)) {
                Some(Ok(mut e)) => {
                    if kbps_now != 0 {
                        let _ = e.set_bitrate_kbps(kbps_now);
                    }
                    encoder = Some(e);
                    enc_res = frame.resolution;
                    enc_fps = fps_now;
                    encode_failures = 0;
                    ctl.force_keyframe.store(true, Ordering::Relaxed);
                }
                Some(Err(e)) => {
                    tracing::error!("could not build an encoder for {:?} @ {fps_now} Hz: {e}", frame.resolution);
                    encoder = None;
                    std::thread::sleep(Duration::from_millis(500));
                    continue;
                }
                None => {
                    // No factory (fixed pipeline): carry on with what we have.
                    enc_res = frame.resolution;
                    enc_fps = fps_now;
                }
            }
        }
        let Some(enc) = encoder.as_mut() else { continue };

        if ctl.intra_refresh.swap(false, Ordering::Relaxed) {
            if let Err(e) = enc.request_intra_refresh() {
                tracing::warn!("intra refresh request failed: {e}");
            }
        }
        let force_key = ctl.force_keyframe.swap(false, Ordering::Relaxed);
        let kbps = ctl.target_kbps.swap(0, Ordering::Relaxed);
        if kbps != 0 {
            match enc.set_bitrate_kbps(kbps) {
                Ok(()) => {
                    kbps_now = kbps;
                    tracing::info!(kbps, "encoder bitrate updated");
                }
                Err(e) => tracing::warn!("set bitrate failed: {e}"),
            }
        }

        match enc.encode(&frame, force_key) {
            Ok(mut packet) => {
                encode_failures = 0;
                packet.meta.frame_id = next_frame_id;
                next_frame_id = next_frame_id.wrapping_add(1);
                if tx.try_send(packet).is_err() {
                    tracing::debug!("network busy, dropped a frame");
                }
            }
            Err(e) => {
                encode_failures += 1;
                if encode_failures == 1 || encode_failures % 60 == 0 {
                    tracing::error!(encode_failures, "encode failed: {e}");
                }
                // Force a keyframe so the viewer resyncs once we recover.
                ctl.force_keyframe.store(true, Ordering::Relaxed);
                if encode_failures >= ENCODE_GIVE_UP {
                    tracing::warn!("encoder keeps failing; rebuilding the video pipeline");
                    encode_failures = 0;
                    rebuild_pipeline(&mut capture, &mut factory, &mut encoder, &mut rebuild, ctl);
                } else if encode_failures % ENCODE_REBUILD == 0 {
                    tracing::warn!("encoder keeps failing; building a new one");
                    encoder = None;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
    if awake {
        aa_platform::keep_awake(false);
    }
    tracing::info!("capture thread exiting");
}

/// Replace capture + encoder factory with brand-new ones. Retries every 2 s
/// while a viewer is connected (the GPU may need a moment after a reset or
/// resume). Returns false if there is no way to rebuild.
fn rebuild_pipeline(
    capture: &mut Box<dyn ScreenCapture>,
    factory: &mut Option<aa_platform::EncoderFactory>,
    encoder: &mut Option<Box<dyn VideoEncoder>>,
    rebuild: &mut Option<aa_platform::PipelineFactory>,
    ctl: &PipelineControl,
) -> bool {
    let Some(r) = rebuild.as_mut() else { return false };
    // Release the old GPU objects first; some drivers refuse a second
    // duplication of the same screen while the first is alive.
    *encoder = None;
    let mut warned = false;
    while !ctl.shutdown.load(Ordering::Relaxed) && ctl.streaming.load(Ordering::Relaxed) {
        match r() {
            Ok((cap, fac)) => {
                tracing::info!(res = ?cap.resolution(), fps = cap.refresh_rate_hz(), "video pipeline rebuilt");
                *capture = cap;
                *factory = Some(fac);
                ctl.force_keyframe.store(true, Ordering::Relaxed);
                return true;
            }
            Err(e) => {
                if !warned {
                    tracing::warn!("video pipeline not ready yet ({e}); retrying every 2 s");
                    warned = true;
                }
                std::thread::sleep(Duration::from_secs(2));
            }
        }
    }
    false
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
    rumble_tx: &mpsc::Sender<aa_core::input::Rumble>,
) {
    loop {
        // With virtual controllers, wake every 2 ms to pass on rumble the
        // game sent them; without, just wait for input.
        let ev = if gamepad.is_some() {
            match rx.try_recv() {
                Ok(ev) => ev,
                Err(mpsc::error::TryRecvError::Empty) => {
                    if let Some(pad) = gamepad.as_mut() {
                        while let Ok(Some(r)) = pad.poll_rumble() {
                            let _ = rumble_tx.try_send(r);
                        }
                    }
                    std::thread::sleep(std::time::Duration::from_millis(2));
                    continue;
                }
                Err(mpsc::error::TryRecvError::Disconnected) => break,
            }
        } else {
            match rx.blocking_recv() {
                Some(ev) => ev,
                None => break,
            }
        };
        let r = match (ev, gamepad.as_mut()) {
            (InputEvent::Gamepad { slot, state }, Some(pad)) => pad.update(slot, state),
            (InputEvent::GamepadAttach { slot, kind }, Some(pad)) => pad.attach(slot, kind),
            (InputEvent::GamepadDetach { slot }, Some(pad)) => pad.detach(slot),
            (
                InputEvent::Gamepad { .. } | InputEvent::GamepadAttach { .. } | InputEvent::GamepadDetach { .. },
                None,
            ) => Ok(()),
            (ev, _) => injector.inject(ev),
        };
        if let Err(e) = r {
            tracing::warn!("input injection failed: {e}");
        }
    }
    tracing::info!("input thread exiting");
}
