//! User-facing stream settings and the derived encoder parameters.

use serde::{Deserialize, Serialize};

use crate::video::Resolution;

/// What the user cares about. Everything the encoder needs is derived from
/// this plus the negotiated codec, so the UI never exposes codec knobs.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct StreamConfig {
    /// Hard cap on what the host will send. The controller stays under this
    /// and drops further when the network says so.
    pub max_bitrate_kbps: u32,
    pub target_fps: u16,
    /// Trade quality for latency. Games want `Speed`; After Effects wants `Quality`.
    pub profile: Profile,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Profile {
    /// Smallest frames, lowest encode latency, more visible compression.
    Speed,
    /// Balanced.
    #[default]
    Balanced,
    /// Highest fidelity (4:4:4 chroma where the codec allows, higher bitrate),
    /// accepts ~1 frame of extra encode latency.
    Quality,
}

impl Default for StreamConfig {
    fn default() -> Self {
        Self { max_bitrate_kbps: 80_000, target_fps: 60, profile: Profile::Balanced }
    }
}

impl StreamConfig {
    /// Starting bitrate before the network has told us anything: ~0.05
    /// bits per pixel per frame, about 16 Mbps at 2880x1800@60. The old
    /// 0.025 started visibly blocky and took ~30 s to clear; this plus the
    /// controller's probe phase (+50%/s until the first loss) reaches 80
    /// Mbps on a clean LAN in about 4 s, and a lossy link is cut back
    /// within a second of the first report.
    pub fn suggested_bitrate_kbps(res: Resolution, fps: u16) -> u32 {
        let bits_per_second = res.pixels() as f64 * f64::from(fps) * 0.05;
        (bits_per_second / 1000.0).clamp(2_000.0, 150_000.0) as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suggested_bitrate_scales_sensibly() {
        let p1080 = StreamConfig::suggested_bitrate_kbps(Resolution::new(1920, 1080), 60);
        let p1440 = StreamConfig::suggested_bitrate_kbps(Resolution::new(2560, 1440), 60);
        let p4k120 = StreamConfig::suggested_bitrate_kbps(Resolution::new(3840, 2160), 120);
        assert!(p1080 > 5_000 && p1080 < 8_000, "{p1080}");
        assert!(p1440 > p1080);
        assert!(p4k120 > p1440);
        assert!(p4k120 <= 150_000);
    }
}
