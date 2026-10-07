//! Adaptive bitrate: the host decides how hard to push from what the viewer
//! reports back once a second.
//!
//! The rule is the classic AIMD shape that every real-time video system
//! converges on: cut quickly when the network complains, grow slowly when
//! it is quiet.
//!
//! The subtle part is telling *congestion* from *Wi-Fi noise*. Both lose
//! packets. Congestion means we are sending more than the link carries:
//! queues fill, so delay climbs, then packets drop; cutting helps. Wi-Fi
//! noise drops a packet here and there at any bitrate while delay stays
//! flat; cutting does nothing except make the picture blocky. The first
//! version treated all loss as congestion, and the simulation showed
//! 0.5% random loss walking the bitrate down to the 2 Mbps floor for good.
//! So light loss now counts as congestion only when RTT has risen above
//! the session's quietest RTT; heavy loss (5%+) is always cut.
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
    /// True until the link first shows loss. While probing we climb fast
    /// (+50%/s from the first clean second) because a fresh session on a
    /// good LAN should look sharp in seconds, not half a minute. The first
    /// loss ends probing for the rest of the session: from then on we know
    /// roughly where the ceiling is and grow gently.
    probing: bool,
    /// Lowest RTT seen this session, in tenths of a ms: the link with empty
    /// queues. RTT well above this means we are filling a queue somewhere.
    base_rtt: Option<u16>,
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
    /// Growth per clean second while probing (see `probing`).
    const PROBE_GROW: f64 = 0.5;
    /// Queueing delay that marks congestion: RTT this far above the base,
    /// in tenths of a ms (8 ms), or half the base, whichever is larger.
    const QUEUE_DELAY_TENTHS: u16 = 80;

    /// Has delay grown enough to say a queue is filling?
    fn queue_building(&mut self, rtt: u16) -> bool {
        if rtt == 0 {
            return false; // no measurement yet
        }
        let base = *self.base_rtt.get_or_insert(rtt);
        let base = base.min(rtt);
        self.base_rtt = Some(base);
        rtt.saturating_sub(base) > Self::QUEUE_DELAY_TENTHS.max(base / 2)
    }

    pub fn new(start_kbps: u32, min_kbps: u32, max_kbps: u32) -> Self {
        let max_kbps = max_kbps.max(min_kbps);
        Self {
            current_kbps: start_kbps.clamp(min_kbps, max_kbps),
            min_kbps,
            max_kbps,
            clean_streak: 0,
            probing: true,
            base_rtt: None,
        }
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
        let congested = self.queue_building(report.rtt_tenths_ms);
        // Light loss only counts when delay says a queue is filling; with
        // flat delay it is Wi-Fi noise, and cutting would only blur the picture.
        let lossy = (loss >= Self::LIGHT_LOSS || report.frames_abandoned > 0) && congested;
        let heavy = loss >= Self::HEAVY_LOSS || (report.frames_abandoned > 2 && congested);
        let next = if self.probing && !lossy && !heavy {
            // Probing: climb fast from the very first clean report.
            (f64::from(self.current_kbps) * (1.0 + Self::PROBE_GROW)) as u32
        } else if heavy {
            self.probing = false;
            self.clean_streak = 0;
            (f64::from(self.current_kbps) * Self::HEAVY_CUT) as u32
        } else if lossy {
            self.probing = false;
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

    /// A report with a congested-looking RTT whenever there is loss, so
    /// the cut rules apply; `noisy` below models Wi-Fi loss at flat RTT.
    fn report(loss_pct: f64, abandoned: u16) -> ReceiverReport {
        let lossy = loss_pct > 0.0 || abandoned > 0;
        ReceiverReport {
            loss_per_10k: (loss_pct * 100.0) as u16,
            frames_abandoned: abandoned,
            frames_received: 60,
            rtt_tenths_ms: if lossy { 400 } else { 50 },
        }
    }

    /// A controller past probing that already knows the quiet RTT (5 ms).
    fn settled(start: u32, min: u32, max: u32) -> BitrateController {
        let mut c = BitrateController::new(start, min, max);
        c.probing = false;
        c.base_rtt = Some(50);
        c
    }

    fn noisy(loss_pct: f64, abandoned: u16) -> ReceiverReport {
        ReceiverReport { rtt_tenths_ms: 50, ..report(loss_pct, abandoned) }
    }

    #[test]
    fn wifi_noise_does_not_cut_quality() {
        let mut c = BitrateController::new(10_000, 2_000, 60_000);
        c.on_report(&report(0.0, 0)); // learn the quiet RTT
        for _ in 0..10 {
            c.on_report(&noisy(0.8, 7));
        }
        assert!(c.current_kbps() >= 10_000, "{}", c.current_kbps());
    }

    #[test]
    fn heavy_loss_cuts_even_at_flat_rtt() {
        let mut c = settled(20_000, 2_000, 60_000);
        assert_eq!(c.on_report(&noisy(8.0, 0)), Some(10_000));
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
        let mut c = settled(20_000, 2_000, 60_000);
        assert_eq!(c.on_report(&report(20.0, 10)), Some(10_000));
        assert_eq!(c.on_report(&report(20.0, 10)), Some(5_000));
    }

    #[test]
    fn light_loss_trims() {
        let mut c = settled(20_000, 2_000, 60_000);
        assert_eq!(c.on_report(&report(2.0, 0)), Some(17_000));
    }

    #[test]
    fn probing_climbs_fast_until_first_loss() {
        let mut c = BitrateController::new(10_000, 2_000, 60_000);
        assert_eq!(c.on_report(&report(0.0, 0)), Some(15_000));
        assert_eq!(c.on_report(&report(0.0, 0)), Some(22_500));
        assert_eq!(c.on_report(&report(2.0, 0)), Some(19_125));
        // Probing is over: the next clean seconds pause, then grow 10%.
        assert_eq!(c.on_report(&report(0.0, 0)), None);
    }

    #[test]
    fn clean_reports_grow_slowly_after_a_pause() {
        let mut c = BitrateController::new(10_000, 2_000, 60_000);
        c.probing = false;
        assert_eq!(c.on_report(&report(0.0, 0)), None);
        assert_eq!(c.on_report(&report(0.0, 0)), None);
        assert_eq!(c.on_report(&report(0.0, 0)), Some(11_000));
        assert_eq!(c.on_report(&report(0.0, 0)), Some(12_100));
    }

    #[test]
    fn respects_bounds_and_cap_changes() {
        let mut c = settled(3_000, 2_000, 4_000);
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
        let mut c = settled(20_000, 2_000, 60_000);
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
