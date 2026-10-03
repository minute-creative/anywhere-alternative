//! What each peer can do, and how the two sides settle on a shared setup.
//!
//! Negotiation is deliberately simple: the host announces what it can
//! *encode*, the viewer announces what it can *decode*, and we pick the
//! best codec both support. Everything else (resolution, fps, bitrate) is
//! clamped to the smaller of the two sides' limits.

use serde::{Deserialize, Serialize};

use crate::video::{Codec, ColorRange, Resolution};

/// Announced by each side during the handshake.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    /// Codecs this side can handle (encode for host, decode for viewer),
    /// with hardware acceleration. Software-only codecs are not listed:
    /// we would rather drop resolution than burn a CPU core.
    pub codecs: Vec<Codec>,
    /// Largest picture this side can process at `max_fps`.
    pub max_resolution: Resolution,
    pub max_fps: u16,
    pub color_ranges: Vec<ColorRange>,
    /// Viewer only: it has a game controller attached it can forward.
    pub has_gamepad: bool,
    /// Host only: it can present a virtual game controller to the OS.
    pub can_emulate_gamepad: bool,
}

/// The result of a successful negotiation. Both sides compute this
/// independently from the same two `Capabilities` and must agree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Negotiated {
    pub codec: Codec,
    pub resolution: Resolution,
    pub fps: u16,
    pub color_range: ColorRange,
    pub gamepad: bool,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum NegotiationError {
    #[error("no video codec supported by both host and viewer")]
    NoCommonCodec,
}

/// Pick the best setup both sides support.
///
/// `host` describes the machine being streamed, `viewer` the one watching.
/// The negotiated resolution is the host's *actual* display resolution
/// clamped to what the viewer can decode; we never upscale on the host.
pub fn negotiate(host: &Capabilities, viewer: &Capabilities) -> Result<Negotiated, NegotiationError> {
    let codec = Codec::ALL
        .into_iter()
        .find(|c| host.codecs.contains(c) && viewer.codecs.contains(c))
        .ok_or(NegotiationError::NoCommonCodec)?;

    let resolution = fit_within(host.max_resolution, viewer.max_resolution);
    let fps = host.max_fps.min(viewer.max_fps).max(1);

    let color_range =
        if host.color_ranges.contains(&ColorRange::HdrPq) && viewer.color_ranges.contains(&ColorRange::HdrPq) {
            ColorRange::HdrPq
        } else {
            ColorRange::Sdr
        };

    Ok(Negotiated { codec, resolution, fps, color_range, gamepad: viewer.has_gamepad && host.can_emulate_gamepad })
}

/// Shrink `src` to fit inside `bound`, keeping aspect ratio. Returns `src`
/// unchanged when it already fits. Dimensions are rounded down to even
/// numbers because 4:2:0 chroma subsampling requires it.
fn fit_within(src: Resolution, bound: Resolution) -> Resolution {
    if src.width <= bound.width && src.height <= bound.height {
        return even(src);
    }
    let scale_w = f64::from(bound.width) / f64::from(src.width);
    let scale_h = f64::from(bound.height) / f64::from(src.height);
    let scale = scale_w.min(scale_h);
    even(Resolution::new((f64::from(src.width) * scale).floor() as u32, (f64::from(src.height) * scale).floor() as u32))
}

fn even(r: Resolution) -> Resolution {
    Resolution::new(r.width & !1, r.height & !1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(codecs: &[Codec], w: u32, h: u32, fps: u16) -> Capabilities {
        Capabilities {
            codecs: codecs.to_vec(),
            max_resolution: Resolution::new(w, h),
            max_fps: fps,
            color_ranges: vec![ColorRange::Sdr],
            has_gamepad: false,
            can_emulate_gamepad: false,
        }
    }

    #[test]
    fn picks_best_common_codec() {
        let host = caps(&[Codec::Hevc, Codec::H264], 3840, 2160, 120);
        let viewer = caps(&[Codec::Av1, Codec::Hevc], 2560, 1440, 165);
        let n = negotiate(&host, &viewer).unwrap();
        assert_eq!(n.codec, Codec::Hevc);
        assert_eq!(n.fps, 120);
    }

    #[test]
    fn fails_without_common_codec() {
        let host = caps(&[Codec::Av1], 1920, 1080, 60);
        let viewer = caps(&[Codec::H264], 1920, 1080, 60);
        assert_eq!(negotiate(&host, &viewer), Err(NegotiationError::NoCommonCodec));
    }

    #[test]
    fn resolution_is_clamped_to_viewer_keeping_aspect() {
        let host = caps(&[Codec::H264], 3840, 2160, 60);
        let viewer = caps(&[Codec::H264], 1920, 1200, 60);
        let n = negotiate(&host, &viewer).unwrap();
        assert_eq!(n.resolution, Resolution::new(1920, 1080));
    }

    #[test]
    fn resolution_is_never_upscaled() {
        let host = caps(&[Codec::H264], 1280, 720, 60);
        let viewer = caps(&[Codec::H264], 3840, 2160, 60);
        let n = negotiate(&host, &viewer).unwrap();
        assert_eq!(n.resolution, Resolution::new(1280, 720));
    }

    #[test]
    fn odd_dimensions_are_rounded_down_to_even() {
        let host = caps(&[Codec::H264], 1365, 767, 60);
        let viewer = caps(&[Codec::H264], 4096, 4096, 60);
        let n = negotiate(&host, &viewer).unwrap();
        assert_eq!(n.resolution, Resolution::new(1364, 766));
    }

    #[test]
    fn both_sides_compute_identical_result() {
        let host = caps(&[Codec::Hevc, Codec::H264], 2560, 1440, 144);
        let viewer = caps(&[Codec::Hevc], 1920, 1080, 60);
        assert_eq!(negotiate(&host, &viewer), negotiate(&host, &viewer));
    }

    #[test]
    fn gamepad_requires_both_sides() {
        let mut host = caps(&[Codec::H264], 1920, 1080, 60);
        let mut viewer = caps(&[Codec::H264], 1920, 1080, 60);
        viewer.has_gamepad = true;
        assert!(!negotiate(&host, &viewer).unwrap().gamepad);
        host.can_emulate_gamepad = true;
        assert!(negotiate(&host, &viewer).unwrap().gamepad);
    }
}
