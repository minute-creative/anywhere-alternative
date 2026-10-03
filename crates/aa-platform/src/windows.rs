//! Windows backends. Stage 1 needs only the *viewer* half (decoder +
//! presentation); the host half is stage 5.
//!
//! Viewer (stage 1):
//! * `decoder` — Media Foundation H.264/HEVC/AV1 decoder MFT with
//!   `MF_SA_D3D11_AWARE`, output as `ID3D11Texture2D` (NV12) for zero-copy
//!   presentation through `wgpu`'s DX12 backend or a DXGI swapchain.
//!
//! Host (stage 5):
//! * `capture` — DXGI Desktop Duplication (`IDXGIOutputDuplication`),
//!   `AcquireNextFrame` returns an `ID3D11Texture2D` per changed frame.
//! * `encoder` — one module per vendor behind the same trait:
//!   `nvenc` (NVIDIA Video Codec SDK), `amf` (AMD AMF), `qsv` (Intel oneVPL,
//!   covers Arc and Core Ultra), and `mf` (Media Foundation, generic
//!   fallback). The factory probes in that order and takes the first that
//!   opens a session.
//! * `input` — `SendInput` with `MOUSEEVENTF_ABSOLUTE` mapped to the virtual
//!   desktop, and scan codes for keys.
//! * `gamepad` — ViGEm bus: virtual DualShock 4 or Xbox 360 per user choice.
//!
//! Crates: `windows` (COM/DXGI/D3D11/MF), `vigem-client`, vendor SDK FFI.

use aa_core::capability::Capabilities;
use aa_core::video::{Codec, ColorRange, Resolution};

use crate::{HostBackends, PlatformError, Result, ViewerBackends};

pub mod capture {
    //! DXGI Desktop Duplication.
}

pub mod encoder {
    //! Vendor encoders: `nvenc`, `amf`, `qsv`, `mf`.
}

pub mod decoder {
    //! Media Foundation / D3D11 video decoder.
}

pub mod input {
    //! `SendInput` injection.
}

pub mod gamepad {
    //! ViGEm virtual controller.
}

pub fn probe_capabilities() -> Capabilities {
    Capabilities {
        codecs: vec![Codec::Hevc, Codec::H264],
        max_resolution: Resolution::new(3840, 2160),
        max_fps: 144,
        color_ranges: vec![ColorRange::Sdr],
        has_gamepad: false,
        can_emulate_gamepad: false,
    }
}

pub fn host_backends() -> Result<HostBackends> {
    Err(PlatformError::NotImplemented("Windows host backends (stage 5)"))
}

pub fn viewer_backends() -> Result<ViewerBackends> {
    Err(PlatformError::NotImplemented("Windows viewer backends (stage 1)"))
}
