//! iPhone backends: only the watching side (the iPhone app, `aa-mobile`).
//!
//! The picture is decoded by the same `VideoToolbox` code as on a Mac
//! (`macos/decoder.rs`, compiled here under another name), because iOS and
//! macOS share those frameworks. Sound playback, the clipboard, controllers
//! and the screen are the Swift app's job, so there is nothing else here.
//!
//! In plain words: on the phone, Rust does the network, security and video
//! decoding; Swift draws, plays sound and reads the controller.

#![allow(clippy::doc_markdown)]

use aa_core::capability::Capabilities;
use aa_core::video::{Codec, ColorRange, Resolution};

use crate::{PlatformError, Result, ViewerBackends};

#[path = "../macos/decoder.rs"]
pub mod decoder;

/// What an iPhone can take: 1080p at most (the app offers 1080 and 720),
/// 75 frames a second at most (owner's choice: the phone's own screen does
/// 60 or 120, and 75 is the ceiling that keeps heat and battery sane).
pub const MAX_FPS: u16 = 75;

pub fn viewer_backends() -> Result<ViewerBackends> {
    let hardware = decoder::VtDecoder::new(Codec::H264).is_ok();
    let mut codecs = if hardware { vec![Codec::Hevc, Codec::H264] } else { vec![Codec::H264] };
    // AV1 in hardware: iPhone 15 Pro and later.
    if hardware && decoder::av1_supported() {
        codecs.insert(0, Codec::Av1);
    }
    tracing::info!(hardware, ?codecs, "viewer decoder: VideoToolbox (iOS)");
    let capabilities = Capabilities {
        codecs,
        max_resolution: Resolution::new(1920, 1080),
        max_fps: MAX_FPS,
        color_ranges: vec![ColorRange::Sdr],
        has_gamepad: true,
        can_emulate_gamepad: false,
        fixed_fps: false,
        loss_recovery: true,
        // Opens its keyboard when the Mac focuses a text field.
        text_focus: true,
    };
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
    Ok(ViewerBackends { decoder, clipboard: None, capabilities })
}
