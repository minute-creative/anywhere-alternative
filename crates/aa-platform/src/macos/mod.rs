//! `macOS` backends. The original plan, now built (gamepad still open):
//!
//! 1. `capture`  — `ScreenCaptureKit` (`SCStream`) delivering `CMSampleBuffer`s
//!    backed by `IOSurface`, at the display's native refresh rate. Needs the
//!    Screen Recording permission (System Settings → Privacy & Security).
//! 2. `encoder`  — `VideoToolbox` `VTCompressionSession` fed the same
//!    `IOSurface` with zero copies. Properties that matter for latency:
//!    `RealTime = true`, `AllowFrameReordering = false` (no B-frames),
//!    `MaxKeyFrameInterval` large + intra-refresh where supported,
//!    `AverageBitRate` + `DataRateLimits` for the controller.
//! 3. `input`    — `CGEventCreateMouseEvent` / `CGEventCreateKeyboardEvent`
//!    posted with `CGEventPost(kCGHIDEventTap)`. Needs the Accessibility
//!    permission.
//! 4. `decoder`  — `VideoToolbox` `VTDecompressionSession` → `CVPixelBuffer`
//!    (`IOSurface`-backed) → Metal texture for presentation.
//! 5. `gamepad`  — the week-one spike: a user-space virtual HID device via
//!    `IOKit` (`IOHIDUserDevice`). If that needs an entitlement we don't have,
//!    fall back to a `DriverKit` `IOUserHIDDevice` extension.
//!
//! Crates we will use: `objc2`, `objc2-screen-capture-kit`,
//! `objc2-video-toolbox`, `objc2-core-media`, `core-graphics`, `io-kit-sys`.

use aa_core::capability::Capabilities;
use aa_core::video::{Codec, ColorRange, Resolution};

use crate::{HostBackends, PlatformError, Result, ViewerBackends};

pub mod audio;
pub mod capture;
pub mod decoder;
pub mod encoder;
pub mod input;

pub mod gamepad {
    //! Virtual HID gamepad spike.
}

/// Query what this Mac can do. Until the backends exist this reports what a
/// current Apple Silicon machine supports so negotiation code can be tested.
pub fn probe_capabilities() -> Capabilities {
    Capabilities {
        codecs: vec![Codec::Hevc, Codec::H264],
        max_resolution: Resolution::new(3840, 2160),
        // Up to the fastest Mac displays (ProMotion 120 Hz, external 240 Hz);
        // the host caps this at its own screen's refresh rate.
        max_fps: aa_core::capability::MAX_FPS,
        color_ranges: vec![ColorRange::Sdr],
        has_gamepad: true,
        can_emulate_gamepad: false,
    }
}

/// Encoders for whatever codec and size the session asks for.
fn vt_factory() -> crate::EncoderFactory {
    Box::new(|codec, res, fps| {
        let kbps = aa_core::config::StreamConfig::suggested_bitrate_kbps(res, fps);
        Ok(Box::new(encoder::VtEncoder::new(codec, res, fps, kbps)?) as Box<dyn crate::VideoEncoder>)
    })
}

/// This Mac as the computer being watched: ScreenCaptureKit picture and
/// sound, VideoToolbox encoding, Quartz events for mouse and keyboard.
pub fn host_backends() -> Result<HostBackends> {
    let cap = capture::SckCapture::new()?;
    let res = crate::ScreenCapture::resolution(&cap);
    let fps = crate::ScreenCapture::refresh_rate_hz(&cap);
    // HEVC first (sharper per bit); H.264 for viewers that only have that
    // (the PC viewer today). The session switches on negotiation.
    let mut codecs = Vec::new();
    let mut encoder: Option<Box<dyn crate::VideoEncoder>> = None;
    for codec in [Codec::Hevc, Codec::H264] {
        let kbps = aa_core::config::StreamConfig::suggested_bitrate_kbps(res, fps);
        match encoder::VtEncoder::new(codec, res, fps, kbps) {
            Ok(e) => {
                codecs.push(codec);
                if encoder.is_none() {
                    encoder = Some(Box::new(e));
                }
            }
            Err(e) => tracing::warn!(?codec, "VideoToolbox can't encode this: {e}"),
        }
    }
    let encoder = encoder.ok_or_else(|| PlatformError::Unavailable("no VideoToolbox encoder".into()))?;
    let audio: Option<Box<dyn crate::audio::AudioCapture>> = match audio::SckAudio::new() {
        Ok(a) => Some(Box::new(a)),
        Err(e) => {
            tracing::warn!("system audio capture unavailable ({e}); streaming without sound");
            None
        }
    };
    Ok(HostBackends {
        capture: Box::new(cap),
        encoder,
        encoder_factory: Some(vt_factory()),
        rebuild: Some(Box::new(|| {
            Ok((Box::new(capture::SckCapture::new()?) as Box<dyn crate::ScreenCapture>, vt_factory()))
        })),
        input: Box::new(input::MacInput::new()?),
        // No virtual controllers on a Mac host yet: macOS needs a signed
        // driver extension for that.
        gamepad: None,
        audio,
        speaker: None,
        clipboard: crate::clipboard::system(),
        pad_attach: None,
        capabilities: Capabilities {
            codecs,
            max_resolution: res,
            max_fps: fps,
            color_ranges: vec![ColorRange::Sdr],
            has_gamepad: false,
            can_emulate_gamepad: false,
        },
    })
}

/// Keep the Mac awake with its display on while someone streams it, using
/// the system's own `caffeinate` tool (it holds the same power assertion an
/// app would, and the assertion ends by itself if we crash).
pub fn keep_awake(on: bool) {
    static CHILD: std::sync::Mutex<Option<std::process::Child>> = std::sync::Mutex::new(None);
    let mut child = CHILD.lock().expect("caffeinate");
    if let Some(mut c) = child.take() {
        let _ = c.kill();
        let _ = c.wait();
    }
    if on {
        // -d display, -i idle sleep, -u "user is active"; -w ends it with us.
        let pid = std::process::id().to_string();
        match std::process::Command::new("/usr/bin/caffeinate").args(["-d", "-i", "-u", "-w", &pid]).spawn() {
            Ok(c) => {
                *child = Some(c);
                tracing::info!("keeping the Mac awake while streaming");
            }
            Err(e) => tracing::warn!("could not keep the Mac awake: {e}"),
        }
    }
}

pub fn viewer_backends() -> Result<ViewerBackends> {
    let mut capabilities = probe_capabilities();
    // What we can decode depends on whether VideoToolbox opens; the real
    // decoder is built after negotiation, so probe once here.
    let hardware = decoder::VtDecoder::new(Codec::H264).is_ok();
    capabilities.codecs = if hardware { vec![Codec::Hevc, Codec::H264] } else { vec![Codec::H264] };
    tracing::info!(hardware, codecs = ?capabilities.codecs, "viewer decoder: VideoToolbox");
    let decoder: crate::DecoderFactory = Box::new(move |codec| -> Result<Box<dyn crate::VideoDecoder>> {
        if hardware {
            match decoder::VtDecoder::new(codec) {
                Ok(d) => return Ok(Box::new(d)),
                Err(e) => tracing::warn!("VideoToolbox unavailable ({e}); using software decode"),
            }
        }
        if codec != Codec::H264 {
            return Err(PlatformError::Unavailable(format!("software decoder only does H.264, not {codec:?}")));
        }
        Ok(Box::new(crate::sw::SwDecoder::new()?))
    });
    Ok(ViewerBackends { decoder, clipboard: crate::clipboard::system(), capabilities })
}
