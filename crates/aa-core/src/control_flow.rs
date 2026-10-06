//! Adaptive bitrate: the host decides how hard to push from what the viewer
//! reports back once a second.
//!
//! The rule is the classic AIMD shape that every real-time video system
//! converges on: cut quickly when the network complains, grow slowly when
//! it is quiet. Loss is the complaint signal (RTT is too noisy on Wi-Fi to
//! steer by alone), and the caps come from the user's settings.
//!
//! No OS code, so this is fully unit-tested.

use bytes::{Buf, BufMut};

/// Sent viewer → host once a second as a `Kind::Ack` payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReceiverReport {
    /// Datagrams lost in the last interval, per 10 000 received (so a u16
    /// covers 0–100% without floats on the wire).
    pub loss_per_10k: u16,
    /// Frames the viewer gave up on in the last interval.
    pub frames_abandoned: u16,
    /// Frames fully received in the last interval.
    pub frames_received: u16,
    /// Smoothed RTT the viewer measured, in tenths of a millisecond.
    pub rtt_tenths_ms: u16,
}

impl ReceiverReport {
    pub const ENCODED_LEN: usize = 8;

    pub fn encode(&self, out: &mut impl BufMut) {
        out.put_u16(self.loss_per_10k);
        out.put_u16(self.frames_abandoned);
        out.put_u16(self.frames_received);
        out.put_u16(self.rtt_tenths_ms);
    }

    pub fn decode(buf: &mut impl Buf) -> Option<Self> {
        if buf.remaining() < Self::ENCODED_LEN {
            return None;
        }
        Some(Self {
            loss_per_10k: buf.get_u16(),
            frames_abandoned: buf.get_u16(),
            frames_received: buf.get_u16(),
            rtt_tenths_ms: buf.get_u16(),
        })
    }

    pub fn loss_ratio(&self) -> f64 {
        f64::from(self.loss_per_10k) / 10_000.0
    }
}

/// Decides the encoder's target bitrate from receiver reports.
#[derive(Debug, Clone)]
pub struct BitrateController {
    current_kbps: u32,
    min_kbps: u32,
    max_kbps: u32,
    /// Consecutive clean reports; growth accelerates a little with streak.
    clean_streak: u32,
}

impl BitrateController {
    /// Loss above this is "the link is hurting": cut hard.
    const HEAVY_LOSS: f64 = 0.05;
    /// Loss above this is "a little congested": cut gently.
    const LIGHT_LOSS: f64 = 0.01;
    /// Multiplicative decrease factors.
    const HEAVY_CUT: f64 = 0.5;
    const LIGHT_CUT: f64 = 0.85;
    /// Increase per clean second, as a fraction of current (~10%/s doubles
    /// in about 7 s; loss cuts are much bigger, so this stays stable).
    const GROW: f64 = 0.10;
    /// Clean seconds before growth starts; avoids oscillating right after a cut.
    const GROW_AFTER: u32 = 2;

    pub fn new(start_kbps: u32, min_kbps: u32, max_kbps: u32) -> Self {
        let max_kbps = max_kbps.max(min_kbps);
        Self { current_kbps: start_kbps.clamp(min_kbps, max_kbps), min_kbps, max_kbps, clean_streak: 0 }
    }

    pub fn current_kbps(&self) -> u32 {
        self.current_kbps
    }

    /// Lower the ceiling (user moved the slider). Takes effect at once.
    pub fn set_max_kbps(&mut self, max_kbps: u32) {
        self.max_kbps = max_kbps.max(self.min_kbps);
        self.current_kbps = self.current_kbps.min(self.max_kbps);
    }

    /// Feed one report. Returns the new target if it changed.
    pub fn on_report(&mut self, report: &ReceiverReport) -> Option<u32> {
        let loss = report.loss_ratio();
        let before = self.current_kbps;
        let next = if loss >= Self::HEAVY_LOSS || report.frames_abandoned > 2 {
            self.clean_streak = 0;
            (f64::from(self.current_kbps) * Self::HEAVY_CUT) as u32
        } else if loss >= Self::LIGHT_LOSS || report.frames_abandoned > 0 {
            self.clean_streak = 0;
            (f64::from(self.current_kbps) * Self::LIGHT_CUT) as u32
        } else {
            self.clean_streak += 1;
            if self.clean_streak > Self::GROW_AFTER {
                let step = (f64::from(self.current_kbps) * Self::GROW).max(250.0);
                (f64::from(self.current_kbps) + step) as u32
            } else {
                self.current_kbps
            }
        };
        self.current_kbps = next.clamp(self.min_kbps, self.max_kbps);
        (self.current_kbps != before).then_some(self.current_kbps)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::BytesMut;

    fn report(loss_pct: f64, abandoned: u16) -> ReceiverReport {
        ReceiverReport {
            loss_per_10k: (loss_pct * 100.0) as u16,
            frames_abandoned: abandoned,
            frames_received: 60,
            rtt_tenths_ms: 50,
        }
    }

    #[test]
    fn report_round_trips() {
        let r = report(12.5, 3);
        let mut b = BytesMut::new();
        r.encode(&mut b);
        assert_eq!(b.len(), ReceiverReport::ENCODED_LEN);
        assert_eq!(ReceiverReport::decode(&mut b.freeze()), Some(r));
        assert_eq!(ReceiverReport::decode(&mut &[0u8; 3][..]), None);
    }

    #[test]
    fn heavy_loss_halves() {
        let mut c = BitrateController::new(20_000, 2_000, 60_000);
        assert_eq!(c.on_report(&report(20.0, 10)), Some(10_000));
        assert_eq!(c.on_report(&report(20.0, 10)), Some(5_000));
    }

    #[test]
    fn light_loss_trims() {
        let mut c = BitrateController::new(20_000, 2_000, 60_000);
        assert_eq!(c.on_report(&report(2.0, 0)), Some(17_000));
    }

    #[test]
    fn clean_reports_grow_slowly_after_a_pause() {
        let mut c = BitrateController::new(10_000, 2_000, 60_000);
        assert_eq!(c.on_report(&report(0.0, 0)), None);
        assert_eq!(c.on_report(&report(0.0, 0)), None);
        assert_eq!(c.on_report(&report(0.0, 0)), Some(11_000));
        assert_eq!(c.on_report(&report(0.0, 0)), Some(12_100));
    }

    #[test]
    fn respects_bounds_and_cap_changes() {
        let mut c = BitrateController::new(3_000, 2_000, 4_000);
        assert_eq!(c.on_report(&report(50.0, 20)), Some(2_000));
        assert_eq!(c.on_report(&report(50.0, 20)), None);
        for _ in 0..40 {
            c.on_report(&report(0.0, 0));
        }
        assert_eq!(c.current_kbps(), 4_000);
        c.set_max_kbps(3_000);
        assert_eq!(c.current_kbps(), 3_000);
    }

    #[test]
    fn a_single_abandoned_frame_counts_as_light_loss() {
        let mut c = BitrateController::new(20_000, 2_000, 60_000);
        assert_eq!(c.on_report(&report(0.0, 1)), Some(17_000));
    }
}

/// Spreads a frame's datagrams across (most of) the frame interval instead
/// of firing them in one burst.
///
/// Why: a 2880×1800 keyframe is ~250 datagrams. Sent back-to-back they
/// arrive at the Wi-Fi adapter faster than it can transmit, its queue
/// overflows and the tail is dropped, which is exactly the loss pattern
/// seen on the first Wi-Fi test. Spacing them over ~75% of the interval
/// leaves headroom for the next frame and keeps the per-frame latency cost
/// under 12 ms at 60 fps.
#[derive(Debug, Clone, Copy)]
pub struct Pacer {
    /// Fraction of the frame interval to spread sends over.
    budget: f64,
}

impl Pacer {
    pub const fn new() -> Self {
        Self { budget: 0.75 }
    }

    /// Offset from the frame's send start at which datagram `index` of
    /// `count` should go out, for a stream running at `fps`.
    pub fn offset(&self, index: usize, count: usize, fps: u16) -> std::time::Duration {
        if count <= 1 || fps == 0 {
            return std::time::Duration::ZERO;
        }
        let interval = 1.0 / f64::from(fps);
        let span = interval * self.budget;
        let frac = index as f64 / (count - 1) as f64;
        std::time::Duration::from_secs_f64(span * frac)
    }

    /// How many datagrams can go out together before the first pause; sending
    /// in small groups cuts syscall overhead without recreating the burst.
    pub const fn group_size(count: usize) -> usize {
        if count > 64 {
            4
        } else {
            2
        }
    }
}

impl Default for Pacer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod pacer_tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn single_packet_has_no_delay() {
        assert_eq!(Pacer::new().offset(0, 1, 60), Duration::ZERO);
    }

    #[test]
    fn spreads_over_three_quarters_of_the_interval() {
        let p = Pacer::new();
        let last = p.offset(249, 250, 60);
        let interval = Duration::from_secs_f64(1.0 / 60.0);
        assert!(last < interval, "{last:?}");
        assert!(last > interval.mul_f64(0.7), "{last:?}");
        assert_eq!(p.offset(0, 250, 60), Duration::ZERO);
        assert!(p.offset(125, 250, 60) < p.offset(126, 250, 60));
    }

    #[test]
    fn higher_fps_means_tighter_spacing() {
        let p = Pacer::new();
        assert!(p.offset(9, 10, 240) < p.offset(9, 10, 60));
    }
}
