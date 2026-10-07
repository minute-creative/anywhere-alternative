//! Frame pacing: how evenly frames arrive, which is what "smooth" means.
//!
//! Average fps hides the thing you feel in a game: one 50 ms gap in an
//! otherwise perfect 120 fps second still reads as "119 fps" but looks like
//! a hitch. So per second we keep every gap between completed frames and
//! report the typical gap (p50), the bad-case gap (p99), the worst gap, and
//! how many gaps were long enough to skip at least one frame ("stutters").

use std::time::Instant;

#[derive(Debug, Default)]
pub struct FramePacing {
    last: Option<Instant>,
    gaps_ms: Vec<f64>,
    /// Stutters since the stream started.
    pub total_stutters: u64,
}

/// One second's summary.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct PacingReport {
    pub p50_ms: f64,
    pub p99_ms: f64,
    pub max_ms: f64,
    /// Gaps longer than two frame intervals this second.
    pub stutters: u32,
}

impl FramePacing {
    /// A frame finished arriving now.
    pub fn frame(&mut self) {
        self.frame_at(Instant::now());
    }

    pub fn frame_at(&mut self, now: Instant) {
        if let Some(prev) = self.last.replace(now) {
            self.gaps_ms.push(now.duration_since(prev).as_secs_f64() * 1000.0);
        }
    }

    /// Summarise and reset for the next second. `fps` is the negotiated rate.
    /// Note: a still desktop sends no frames at all, so stutters only mean
    /// something while the picture is moving (a game, a video).
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss, clippy::cast_precision_loss)]
    pub fn take(&mut self, fps: u16) -> PacingReport {
        if self.gaps_ms.is_empty() {
            return PacingReport::default();
        }
        let budget = 2000.0 / f64::from(fps.max(1));
        let stutters = self.gaps_ms.iter().filter(|g| **g > budget).count() as u32;
        self.total_stutters += u64::from(stutters);
        self.gaps_ms.sort_by(f64::total_cmp);
        let at = |q: f64| self.gaps_ms[((self.gaps_ms.len() - 1) as f64 * q).round() as usize];
        let report =
            PacingReport { p50_ms: at(0.5), p99_ms: at(0.99), max_ms: *self.gaps_ms.last().unwrap_or(&0.0), stutters };
        self.gaps_ms.clear();
        report
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn even_frames_have_no_stutter() {
        let mut p = FramePacing::default();
        let t0 = Instant::now();
        for i in 0..120 {
            p.frame_at(t0 + Duration::from_micros(8_333 * i));
        }
        let r = p.take(120);
        assert!((r.p50_ms - 8.333).abs() < 0.01);
        assert_eq!(r.stutters, 0);
    }

    #[test]
    fn a_long_gap_is_a_stutter() {
        let mut p = FramePacing::default();
        let t0 = Instant::now();
        let mut t = t0;
        for i in 0..60 {
            t += Duration::from_micros(if i == 30 { 40_000 } else { 8_333 });
            p.frame_at(t);
        }
        let r = p.take(120);
        assert_eq!(r.stutters, 1);
        assert!(r.max_ms > 39.0);
        assert_eq!(p.total_stutters, 1);
    }
}
