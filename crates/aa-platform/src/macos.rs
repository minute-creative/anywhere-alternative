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

pub mod decoder {
    //! `VideoToolbox` decoder backend.
}

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
        max_fps: 120,
        color_ranges: vec![ColorRange::Sdr],
        has_gamepad: false,
        can_emulate_gamepad: false,
    }
}

pub fn host_backends() -> Result<HostBackends> {
    Err(PlatformError::NotImplemented("macOS host backends (stage 1)"))
}

pub fn viewer_backends() -> Result<ViewerBackends> {
    Err(PlatformError::NotImplemented("macOS viewer backends (stage 1)"))
}
