//! Windows backends.
//!
//! Host (available now, CPU-readback path):
//! * [`capture`] — DXGI Desktop Duplication. `AcquireNextFrame` hands us the
//!   desktop as a GPU texture; this first version copies it to a staging
//!   texture and reads it back to the CPU for the software encoder. The
//!   zero-copy path (texture straight into NVENC/AMF/QuickSync) replaces the
//!   readback once a hardware encoder backend exists.
//! * [`input`] — `SendInput` with scan codes for keys and absolute
//!   virtual-desktop coordinates for the mouse.
//! * `gamepad` — stage 2: `ViGEm` bus, virtual DualShock 4 or Xbox 360.
//!
//! Viewer: software decoder until the Media Foundation / D3D11 decoder
//! lands (stage 5 hardware work).
//!
//! Vendor encoders (`nvenc`, `amf`, `qsv`, `mf`) are stage 5 and slot in
//! behind [`crate::VideoEncoder`] without touching this file's shape.

pub mod capture;
pub mod input;

use aa_core::capability::Capabilities;
use aa_core::config::StreamConfig;
use aa_core::video::{Codec, ColorRange, Resolution};

use crate::{HostBackends, Result, ScreenCapture, ViewerBackends};

pub fn host_backends() -> Result<HostBackends> {
    let cap = capture::DxgiCapture::new(0)?;
    let res = cap.resolution();
    let fps = cap.refresh_rate_hz();
    let kbps = StreamConfig::suggested_bitrate_kbps(res, fps);
    tracing::info!(?res, fps, kbps, "windows host: DXGI capture + software H.264");
    let encoder = crate::sw::SwEncoder::new(res, fps, kbps)?;
    let input = input::SendInputInjector::new(cap.desktop_rect())?;
    Ok(HostBackends {
        capture: Box::new(cap),
        encoder: Box::new(encoder),
        input: Box::new(input),
        gamepad: None,
        capabilities: Capabilities {
            codecs: vec![Codec::H264],
            max_resolution: res,
            max_fps: fps,
            color_ranges: vec![ColorRange::Sdr],
            has_gamepad: false,
            can_emulate_gamepad: false,
        },
    })
}

pub fn viewer_backends() -> Result<ViewerBackends> {
    Ok(ViewerBackends {
        decoder: Box::new(crate::sw::SwDecoder::new()?),
        capabilities: Capabilities {
            codecs: vec![Codec::H264],
            max_resolution: Resolution::new(3840, 2160),
            max_fps: 240,
            color_ranges: vec![ColorRange::Sdr],
            has_gamepad: false,
            can_emulate_gamepad: false,
        },
    })
}
