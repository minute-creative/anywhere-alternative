//! iPhone and iPad: viewer only (an iPhone can't be the computer being
//! watched). Decoding is the Mac's `VideoToolbox` decoder, compiled for iOS
//! unchanged: it only uses `CoreMedia`, `CoreVideo` and `VideoToolbox`,
//! which iOS has too.
//!
//! In plain words: this lets the iPhone app turn the stream back into
//! pictures with the phone's own video chip, the same way a Mac viewer does.

use aa_core::capability::Capabilities;
use aa_core::video::{Codec, ColorRange, Resolution};

use crate::{PlatformError, Result, ViewerBackends};

#[path = "macos/decoder.rs"]
pub mod decoder;

/// iPhones without ProMotion (like the 14 Plus) show 60 frames a second;
/// asking for more only wastes the connection. ProMotion models could ask
/// for 120 later (`UIScreen.maximumFramesPerSecond`).
pub fn display_refresh_hz() -> u16 {
    60
}

pub fn viewer_backends() -> Result<ViewerBackends> {
    let hardware = decoder::VtDecoder::new(Codec::H264).is_ok();
    let codecs = if hardware { vec![Codec::Hevc, Codec::H264] } else { vec![Codec::H264] };
    tracing::info!(hardware, ?codecs, "iPhone viewer decoder: VideoToolbox");
    let capabilities = Capabilities {
        codecs,
        // The phone's screen is about 2778 x 1284; 1080p is plenty and
        // keeps the stream light on mobile data.
        max_resolution: Resolution::new(1920, 1080),
        max_fps: display_refresh_hz(),
        color_ranges: vec![ColorRange::Sdr],
        has_gamepad: true,
        can_emulate_gamepad: false,
        fixed_fps: false,
        // Same decoder as the Mac, which resumes cleanly from repair frames.
        loss_recovery: true,
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
