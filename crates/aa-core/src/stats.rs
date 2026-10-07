//! Running measurements that drive adaptive bitrate and the on-screen overlay.
//!
//! All values are plain integers/floats updated from the hot path; nothing
//! here allocates or locks.

/// Exponentially weighted moving average, the standard way to smooth a noisy
/// per-frame measurement without storing history.
#[derive(Debug, Clone, Copy)]
pub struct Ewma {
    value: Option<f64>,
    /// Weight of each new sample, 0 < alpha ≤ 1. Smaller = smoother/slower.
    alpha: f64,
}

impl Ewma {
    pub const fn new(alpha: f64) -> Self {
        Self { value: None, alpha }
    }

    pub fn push(&mut self, sample: f64) {
        self.value = Some(match self.value {
            None => sample,
            Some(v) => v + self.alpha * (sample - v),
        });
    }

    pub fn get(&self) -> Option<f64> {
        self.value
    }
}

/// Tracks datagram loss from the `seq` field in every header.
#[derive(Debug, Default, Clone, Copy)]
pub struct LossTracker {
    expected_next: Option<u16>,
    pub received: u64,
    pub lost: u64,
}

impl LossTracker {
    pub fn observe(&mut self, seq: u16) {
        self.received += 1;
        if let Some(exp) = self.expected_next {
            // Distance forward from what we expected, treating wrap correctly.
            let gap = seq.wrapping_sub(exp);
            if gap < u16::MAX / 2 {
                // On time or ahead: anything skipped is (for now) lost.
                self.lost += u64::from(gap);
                self.expected_next = Some(seq.wrapping_add(1));
            } else {
                // Behind: a packet we already counted as lost turned up late
                // (Wi-Fi reorders now and then). Give it back, and do *not*
                // move `expected_next` backwards, or every packet after it
                // gets counted as lost a second time. That double count read
                // as ~30-45% loss on a jittery link and made the bitrate
                // controller cut quality for nothing.
                self.lost = self.lost.saturating_sub(1);
            }
            return;
        }
        self.expected_next = Some(seq.wrapping_add(1));
    }

    /// Fraction of datagrams lost, 0.0 ..= 1.0.
    pub fn loss_ratio(&self) -> f64 {
        let total = self.received + self.lost;
        if total == 0 {
            0.0
        } else {
            self.lost as f64 / total as f64
        }
    }
}

/// Everything the viewer's overlay shows and the bitrate controller reads.
#[derive(Debug, Clone, Copy)]
pub struct StreamStats {
    /// Host capture → viewer decode-complete, in ms (needs clock offset; stage 3).
    pub glass_to_glass_ms: Ewma,
    /// Time from a frame's first slice arriving to its last, in ms.
    pub frame_assembly_ms: Ewma,
    /// Round-trip from ping/pong, in ms.
    pub rtt_ms: Ewma,
    pub loss: LossTracker,
    pub frames_decoded: u64,
    pub frames_dropped: u64,
}

impl Default for StreamStats {
    fn default() -> Self {
        Self {
            glass_to_glass_ms: Ewma::new(0.1),
            frame_assembly_ms: Ewma::new(0.2),
            rtt_ms: Ewma::new(0.3),
            loss: LossTracker::default(),
            frames_decoded: 0,
            frames_dropped: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn reordering_is_not_loss() {
        let mut t = super::LossTracker::default();
        for seq in [1u16, 2, 4, 3, 5, 7, 6, 8] {
            t.observe(seq);
        }
        assert_eq!(t.lost, 0);
        assert_eq!(t.received, 8);
    }

    #[test]
    fn real_gaps_still_count() {
        let mut t = super::LossTracker::default();
        for seq in [1u16, 2, 5, 6] {
            t.observe(seq);
        }
        assert_eq!(t.lost, 2);
    }

    use super::*;

    #[test]
    fn ewma_starts_at_first_sample_and_smooths() {
        let mut e = Ewma::new(0.5);
        assert_eq!(e.get(), None);
        e.push(10.0);
        assert_eq!(e.get(), Some(10.0));
        e.push(20.0);
        assert_eq!(e.get(), Some(15.0));
    }

    #[test]
    fn loss_counts_gaps_and_handles_wrap() {
        let mut l = LossTracker::default();
        for s in [0u16, 1, 2, 5] {
            l.observe(s);
        }
        assert_eq!(l.lost, 2);
        let mut w = LossTracker::default();
        w.observe(u16::MAX);
        w.observe(0);
        assert_eq!(w.lost, 0);
        w.observe(2);
        assert_eq!(w.lost, 1);
    }

    #[test]
    fn reordered_packet_is_not_loss() {
        let mut l = LossTracker::default();
        l.observe(5);
        l.observe(4);
        assert_eq!(l.lost, 0);
    }

    #[test]
    fn loss_ratio() {
        let mut l = LossTracker::default();
        assert!(l.loss_ratio().abs() < f64::EPSILON);
        l.observe(0);
        l.observe(2);
        assert!((l.loss_ratio() - 1.0 / 3.0).abs() < 1e-9);
    }
}
