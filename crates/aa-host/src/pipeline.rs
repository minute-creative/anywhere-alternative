//! The capture → encode thread and the input-injection thread.

// In plain words: three worker threads of the sharing computer.
// - capture_thread: takes pictures of the screen, squeezes them (encodes)
//   and hands them to the network task. It also heals itself: if the screen
//   changes size or the graphics card resets, it builds everything again.
// - audio_thread: records what the computer plays and squeezes it with Opus.
// - input_thread: applies the viewer's mouse, keys and controllers here.
// The network loop (session.rs) steers them through PipelineControl, a set
// of shared switches that need no waiting.
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, AtomicU8, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use aa_core::share::ShareTarget;
use aa_core::video::{Codec, Resolution};

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
    /// Parity packets per data packet for video, in thousandths (100 =
    /// 10%). Set by the bitrate controller from reported loss.
    pub fec_permille: AtomicU32,
    /// Codec the current viewer negotiated (`Codec as u8`); `NO_CODEC` =
    /// keep whatever encoder is loaded.
    pub codec: AtomicU8,
    pub shutdown: AtomicBool,
    /// Why the screen can't be shown right now ("PC is locked"), for the
    /// viewer; the network task forwards changes.
    pub screen_status: Mutex<Option<&'static str>>,
    /// Frames per second to send (the frame-rate ladder's choice); 0 = as
    /// many as the screen makes.
    pub target_fps: AtomicU16,
    /// Largest picture the viewer wants (its Resolution setting); `None` =
    /// full size. `max_res_changed` tells the capture thread to apply it.
    pub max_res: Mutex<Option<Resolution>>,
    pub max_res_changed: AtomicBool,
    /// A new choice of what to share, for the capture thread to apply.
    pub share: Mutex<Option<ShareTarget>>,
    /// The same, for the sound thread.
    pub audio_share: Mutex<Option<ShareTarget>>,
    /// The same, for the input thread: `Some(None)` = whole screen,
    /// `Some(Some(t))` = keep the viewer inside `t`.
    #[allow(clippy::option_option)] // "no news" vs "no limit" vs "this limit"
    pub input_limit: Mutex<Option<Option<ShareTarget>>>,
    /// Size of the picture actually being sent (after sharing and scaling).
    pub out_res: Mutex<Option<Resolution>>,
    /// The viewer can resume from a repair frame (negotiated).
    pub loss_recovery: AtomicBool,
    /// The viewer lost something: repair from a long-term reference if the
    /// encoder can, else send a keyframe.
    pub recovery: AtomicBool,
    /// The viewer's newest good frame (`u32::MAX`: no news).
    pub last_good: AtomicU32,
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
            fec_permille: AtomicU32::new((aa_core::wire::FEC_RATIO * 1000.0) as u32),
            codec: AtomicU8::new(NO_CODEC),
            shutdown: AtomicBool::new(false),
            screen_status: Mutex::new(None),
            target_fps: AtomicU16::new(0),
            max_res: Mutex::new(None),
            max_res_changed: AtomicBool::new(false),
            share: Mutex::new(None),
            audio_share: Mutex::new(None),
            input_limit: Mutex::new(None),
            out_res: Mutex::new(None),
            loss_recovery: AtomicBool::new(false),
            recovery: AtomicBool::new(false),
            last_good: AtomicU32::new(u32::MAX),
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

/// When to re-encode a still screen, in ms after it stopped changing.
const REFINE_AFTER_MS: [u64; 6] = [120, 250, 400, 600, 900, 1300];

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
    // What the encoder was built for: picture size and frame rate.
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
    // Sharpening a still screen. Frames come only when something changes, so
    // after movement the last, rushed (soft, blocky) picture would stay on
    // the viewer for as long as nothing moves. Encoding that same picture
    // again lets the encoder add the detail it skipped; each pass adds more.
    // Only updates, never a fresh full picture: a full picture has to fit
    // the same per-frame budget and so starts blurry again (tried in 0.4.3).
    let mut last_frame: Option<aa_platform::CapturedFrame> = None;
    let mut still_since = Instant::now();
    let mut refined = 0u8;
    let mut skipped_in_row = 0u32;
    // Frame-rate limit: a 200 Hz screen makes 200 pictures a second, but
    // the connection may only afford 90 at full sharpness (the ladder in
    // the network task decides). Pictures that come early are held back;
    // the newest held one goes out when its turn comes, so the last
    // movement is never lost.
    let mut next_due = Instant::now();
    let mut held = false;
    // Long-term references the viewer hasn't confirmed yet: (wire frame
    // id, encoder token).
    let mut ltr_pending: VecDeque<(u32, i64)> = VecDeque::new();
    // What is shared and how big, so a rebuilt capture gets the same.
    let mut sharing = ShareTarget::Screen;
    let mut max_size: Option<Resolution> = None;
    let mut rebuilt = false;
    // Sharing just one app failed (it closed, or this computer can't): show
    // nothing at all rather than the whole screen.
    let mut share_blocked = false;
    let mut blocked_retry = Instant::now();

    // The main loop: runs until the program stops.
    while !ctl.shutdown.load(Ordering::Relaxed) {
        let streaming = ctl.streaming.load(Ordering::Relaxed);
        if streaming != awake {
            aa_platform::keep_awake(streaming);
            awake = streaming;
        }
        // Nobody watching: sleep and use no power or graphics card.
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

        // What to share changed (the person here, or a full-access viewer),
        // or the capture was rebuilt and must be told again.
        // Sharing choice: apply a new one, or re-apply after a rebuild, or retry an
        // app that could not be shown (every 3 seconds).
        let new_share = ctl.share.lock().expect("share").take();
        let retry = share_blocked && blocked_retry.elapsed() > Duration::from_secs(3);
        if retry {
            blocked_retry = Instant::now();
        }
        let new_share = new_share.or_else(|| (rebuilt || retry).then(|| sharing.clone()));
        if rebuilt {
            capture.set_max_size(max_size);
            rebuilt = false;
        }
        if let Some(target) = new_share {
            sharing = target.clone();
            match capture.set_share(&target) {
                Ok(()) => {
                    tracing::info!(sharing = target.label(), "now sharing");
                    share_blocked = false;
                }
                Err(e) => {
                    tracing::warn!(sharing = target.label(), "could not share just that ({e}); showing nothing");
                    share_blocked = !target.is_screen();
                }
            }
            last_frame = None;
            ctl.force_keyframe.store(true, Ordering::Relaxed);
        }
        if ctl.max_res_changed.swap(false, Ordering::Relaxed) {
            max_size = *ctl.max_res.lock().expect("max res");
            capture.set_max_size(max_size);
        }

        // Tell the viewer when the screen cannot be shown (locked, asleep...).
        let reason = if share_blocked {
            Some("The chosen app or window can't be shown right now, so nothing is shown")
        } else {
            capture.unavailable_reason()
        };
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

        // Frame rate the connection affords, never above what the screen makes.
        let refresh = capture.refresh_rate_hz().max(1);
        let target = ctl.target_fps.load(Ordering::Relaxed);
        let fps_now = if target == 0 { refresh } else { target.min(refresh) };
        let interval = Duration::from_secs_f64(1.0 / f64::from(fps_now));

        // While a picture is held back, wait only until its turn.
        let wait =
            if held { next_due.saturating_duration_since(Instant::now()).min(frame_timeout) } else { frame_timeout };
        // Ask the system for the next screen picture (waits up to "wait").
        let frame = match capture.next_frame(wait) {
            // A new picture.
            Ok(Some(f)) => {
                capture_failures = 0;
                still_since = Instant::now();
                refined = 0;
                last_frame = Some(f.clone());
                // Early: hold it; a newer picture may replace it before its turn.
                // (1 ms of slack so a 60 Hz screen isn't cut to 59.)
                if Instant::now() + Duration::from_millis(1) < next_due {
                    held = true;
                    continue;
                }
                held = false;
                f
            }
            // No new picture, but the one we held back is due now: send it.
            Ok(None) if held && Instant::now() >= next_due => {
                // The held picture's turn: nothing newer came.
                held = false;
                let Some(f) = last_frame.clone() else { continue };
                f
            }
            // Nothing new: maybe re-send the last picture sharper (see REFINE_AFTER_MS).
            Ok(None) => {
                // Nothing changed on screen. Time to sharpen the last picture?
                let still = still_since.elapsed();
                let due =
                    REFINE_AFTER_MS.get(usize::from(refined)).is_some_and(|&ms| still > Duration::from_millis(ms));
                let Some(mut f) = last_frame.clone().filter(|_| due && !held) else { continue };
                // Encoders insist on time moving forward between frames.
                f.capture_ts_us += still.as_micros() as u64;
                refined += 1;
                f
            }
            // The screen or graphics card went away (resolution change, sleep, driver
            // reset): build the whole capture again.
            Err(PlatformError::DeviceLost(why)) => {
                tracing::warn!("capture device lost ({why}); rebuilding the video pipeline");
                rebuilt = rebuild_pipeline(&mut capture, &mut factory, &mut encoder, &mut rebuild, ctl);
                last_frame = None;
                held = false;
                continue;
            }
            // Any other capture error: wait a moment; after 40 in a row, rebuild.
            Err(e) => {
                capture_failures += 1;
                if capture_failures == 1 || capture_failures % 20 == 0 {
                    tracing::warn!(capture_failures, "capture failed: {e}");
                }
                std::thread::sleep(Duration::from_millis(250));
                if capture_failures >= CAPTURE_GIVE_UP {
                    tracing::warn!("capture keeps failing; rebuilding the video pipeline");
                    capture_failures = 0;
                    rebuilt = rebuild_pipeline(&mut capture, &mut factory, &mut encoder, &mut rebuild, ctl);
                    last_frame = None;
                    held = false;
                }
                continue;
            }
        };
        if share_blocked {
            continue;
        }
        // Keep a steady beat; after a pause, start the beat again from now.
        let now = Instant::now();
        next_due = if now > next_due + interval { now + interval } else { next_due + interval };

        // The size we send: the picture as captured, fitted inside the
        // viewer's Resolution setting when the capture doesn't scale itself.
        let out_res = match *ctl.max_res.lock().expect("max res") {
            Some(max) if !capture.scales_itself() => aa_core::capability::fit_within(frame.resolution, max),
            _ => frame.resolution,
        };
        *ctl.out_res.lock().expect("out res") = Some(out_res);

        // The frame rate changed: tell the encoder (a Mac does that on the
        // fly; a PC encoder is rebuilt at the new rate).
        if fps_now != enc_fps {
            if let Some(enc) = encoder.as_mut().filter(|_| out_res == enc_res) {
                if enc.set_frame_rate(fps_now) {
                    tracing::info!(fps = fps_now, "frame rate changed");
                    enc_fps = fps_now;
                } else {
                    tracing::info!(fps = fps_now, "frame rate changed; new encoder");
                    encoder = None;
                }
            }
        }

        // The picture changed size (display settings, sharing one window,
        // the viewer's setting) or the encoder was dropped: build one for
        // what we send now.
        if encoder.is_none() || out_res != enc_res || fps_now != enc_fps {
            if encoder.is_some() {
                tracing::info!(res = ?out_res, fps = fps_now, "picture changed; rebuilding encoder");
            }
            match factory.as_mut().map(|f| f(current_codec, out_res, fps_now)) {
                Some(Ok(mut e)) => {
                    if kbps_now != 0 {
                        let _ = e.set_bitrate_kbps(kbps_now);
                    }
                    encoder = Some(e);
                    enc_res = out_res;
                    enc_fps = fps_now;
                    encode_failures = 0;
                    ltr_pending.clear();
                    ctl.force_keyframe.store(true, Ordering::Relaxed);
                }
                Some(Err(e)) => {
                    tracing::error!("could not build an encoder for {out_res:?} @ {fps_now} fps: {e}");
                    encoder = None;
                    std::thread::sleep(Duration::from_millis(500));
                    continue;
                }
                None => {
                    // No factory (fixed pipeline): carry on with what we have.
                    enc_res = out_res;
                    enc_fps = fps_now;
                }
            }
        }
        let Some(enc) = encoder.as_mut() else { continue };

        // The viewer confirmed frames up to `good`: their long-term
        // references may be built on from now on.
        // Long-term reference bookkeeping (Mac only): tell the encoder which of
        // its reference pictures the viewer is known to have.
        let good = ctl.last_good.swap(u32::MAX, Ordering::Relaxed);
        if good != u32::MAX && !ltr_pending.is_empty() {
            let mut confirmed = Vec::new();
            while let Some(&(id, token)) = ltr_pending.front() {
                if aa_core::wire::is_newer(id, good) {
                    break;
                }
                confirmed.push(token);
                ltr_pending.pop_front();
            }
            if !confirmed.is_empty() {
                enc.acknowledge(&confirmed);
            }
        }
        // The viewer lost something: repair frame if possible, else a keyframe.
        if ctl.recovery.swap(false, Ordering::Relaxed) {
            // Unconfirmed references may be among what was lost: never
            // build on them.
            ltr_pending.clear();
            if !enc.request_recovery() {
                ctl.intra_refresh.store(true, Ordering::Relaxed);
            }
        }
        if ctl.intra_refresh.swap(false, Ordering::Relaxed) {
            if let Err(e) = enc.request_intra_refresh() {
                tracing::warn!("intra refresh request failed: {e}");
            }
        }
        // Pick up a forced keyframe and a new bitrate, if the network task set them.
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

        // Encode this picture and give it a frame number that only ever goes up.
        match enc.encode(&frame, force_key) {
            Ok(mut packet) => {
                encode_failures = 0;
                skipped_in_row = 0;
                packet.meta.frame_id = next_frame_id;
                next_frame_id = next_frame_id.wrapping_add(1);
                if packet.meta.is_keyframe {
                    ltr_pending.clear();
                }
                if let Some(token) = packet.ltr_token {
                    ltr_pending.push_back((packet.meta.frame_id, token));
                    // Bounded: a viewer that never confirms (an old one) mustn't grow this.
                    while ltr_pending.len() > 64 {
                        ltr_pending.pop_front();
                    }
                }
                // The network task is behind: drop this frame rather than queue it.
                if tx.try_send(packet).is_err() {
                    tracing::debug!("network busy, dropped a frame");
                }
            }
            // The encoder chose to skip this frame (out of bitrate budget).
            Err(PlatformError::FrameSkipped) => {
                skipped_in_row += 1;
                if skipped_in_row % 60 == 0 {
                    tracing::warn!(skipped_in_row, kbps = kbps_now, "encoder keeps skipping frames (picture frozen)");
                }
            }
            // A real failure: ask for a keyframe; after 30 in a row build a new encoder,
            // after 120 rebuild everything.
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
                    rebuilt = rebuild_pipeline(&mut capture, &mut factory, &mut encoder, &mut rebuild, ctl);
                    last_frame = None;
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
    // Sharing one app on a system that can't separate its sound: send none
    // rather than everything (nothing should slip out that wasn't chosen).
    let mut silenced = false;

    // Loop: record 10 ms of sound, encode it, label it, queue it for sending.
    while !ctl.shutdown.load(Ordering::Relaxed) {
        if !ctl.streaming.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(50));
            continue;
        }
        // Sharing one app: limit the sound to that app, or go silent if this
        // system cannot separate one app's sound.
        let new_share = ctl.audio_share.lock().expect("audio share").take();
        if let Some(target) = new_share {
            silenced = match capture.set_share(&target) {
                Ok(only_that_app) => !target.is_screen() && !only_that_app,
                Err(e) => {
                    tracing::warn!("could not limit the sound to {} ({e})", target.label());
                    !target.is_screen()
                }
            };
            if silenced {
                tracing::info!("sound is off while sharing {}: this system can't send one app's sound", target.label());
            }
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
        if !has_sound || silenced {
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
    ctl: &PipelineControl,
) {
    // Loop: take the next input event and apply it.
    loop {
        // Sharing changed: keep the viewer inside the shared app (or not).
        if let Some(limit) = ctl.input_limit.lock().expect("limit").take() {
            injector.set_limit(limit.as_ref());
        }
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
        // Controller events go to the virtual controller (if this computer has
        // one); everything else (mouse, keys) to the input injector.
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
