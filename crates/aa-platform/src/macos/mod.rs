//! `macOS` backends. Stage 1 fills these in, in this order:
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

pub mod capture {
    //! `ScreenCaptureKit` backend. See module docs in `macos.rs`.
}

pub mod encoder {
    //! `VideoToolbox` encoder backend.
}

pub mod decoder;

pub mod input {
    //! `CGEvent` input injection.
}

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

pub fn host_backends() -> Result<HostBackends> {
    Err(PlatformError::NotImplemented("macOS host backends (stage 1)"))
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
