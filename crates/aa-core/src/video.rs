//! The data model of a video stream: codecs, pixel formats, frame metadata.

use serde::{Deserialize, Serialize};

/// Compressed video codecs we may negotiate. Order of the enum is also our
/// preference order (best first) when both sides support several.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[repr(u8)]
pub enum Codec {
    /// AV1: best quality per bit, hardware encode on M3+, RTX 40+, RX 7000+,
    /// Intel Arc / Core Ultra. Decode is broad on anything from ~2021 on.
    Av1 = 0,
    /// HEVC (H.265): hardware encode/decode on practically everything from
    /// the last 8 years. Our expected default in practice.
    Hevc = 1,
    /// H.264 (AVC): universal fallback. Highest bitrate for a given quality.
    H264 = 2,
}

impl Codec {
    /// All codecs, best first.
    pub const ALL: [Codec; 3] = [Codec::Av1, Codec::Hevc, Codec::H264];

    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Codec::Av1),
            1 => Some(Codec::Hevc),
            2 => Some(Codec::H264),
            _ => None,
        }
    }
}

/// Uncompressed pixel layouts a capture backend may produce. The encoder
/// backend declares which it accepts; the pipeline inserts a GPU conversion
/// only when they disagree (which costs ~0.3 ms and we'd rather avoid).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PixelFormat {
    /// 8-bit BGRA, what Desktop Duplication and `ScreenCaptureKit` hand out by default.
    Bgra8,
    /// 8-bit 4:2:0 semi-planar; what every hardware encoder actually wants.
    Nv12,
    /// 10-bit 4:2:0 semi-planar; required for HDR.
    P010,
}

/// Dynamic range of the content. HDR is a stage-6 feature but the type exists
/// now so nothing has to be refactored to add it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum ColorRange {
    #[default]
    Sdr,
    HdrPq,
}

/// Picture dimensions in pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Resolution {
    pub width: u32,
    pub height: u32,
}

impl Resolution {
    pub const fn new(width: u32, height: u32) -> Self {
        Self { width, height }
    }

    pub const fn pixels(self) -> u64 {
        self.width as u64 * self.height as u64
    }
}

/// Metadata that travels with every encoded frame. The payload bytes live
/// outside this struct so the hot path never copies them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EncodedFrameMeta {
    /// Monotonic frame counter, starts at 0 per stream. Wraps are not handled;
    /// at 240 fps a u32 lasts ~200 days.
    pub frame_id: u32,
    /// Host-side capture timestamp, microseconds on the host's monotonic clock.
    /// Only meaningful relative to other host timestamps.
    pub capture_ts_us: u64,
    /// Whether this frame can be decoded without any previous frame.
    /// With intra-refresh enabled this is rare after the first frame.
    pub is_keyframe: bool,
    pub codec: Codec,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codec_round_trips_through_u8() {
        for c in Codec::ALL {
            assert_eq!(Codec::from_u8(c as u8), Some(c));
        }
        assert_eq!(Codec::from_u8(99), None);
    }

    #[test]
    fn codec_order_is_preference_order() {
        assert!(Codec::Av1 < Codec::Hevc);
        assert!(Codec::Hevc < Codec::H264);
    }
}
