//! What "full quality" costs, and which frame rate the connection can afford.
//!
//! The owner's rules (October 2026):
//! 1. Frame rate goes as high as both screens allow (up to 240).
//! 2. When the connection weakens, frame rate gives way first, step by
//!    step, down to 60. Resolution and sharpness stay full.
//! 3. Never below 60. If even 60 at full sharpness doesn't fit, sharpness
//!    gives way (the bitrate controller just keeps lowering the bitrate).
//! 4. Steady beats twitchy: change rungs rarely, and only up after the
//!    connection has proved itself for a while.
//!
//! [`sharp_kbps`] estimates the bitrate at which a picture looks sharp for
//! a codec, size and frame rate; [`FpsLadder`] picks the rung from what
//! the bitrate controller says the connection carries. No OS code, so all
//! of it is unit-tested.
// In plain words: the frame rate (pictures per second) drops first when the
// network struggles, and only at 60 does the picture start to get softer.
// This file has the numbers behind that rule.

use crate::video::{Codec, Resolution};

/// The frame rates we step between, fastest first.
pub const FPS_RUNGS: [u16; 9] = [240, 200, 165, 144, 120, 100, 90, 75, 60];

/// The floor from rule 3.
pub const MIN_FPS: u16 = 60;

/// Bits per pixel per frame that look sharp at 60 fps. HEVC needs about
/// 30% less than H.264 for the same picture and AV1 about 20% less again.
fn bits_per_pixel(codec: Codec) -> f64 {
    match codec {
        Codec::H264 => 0.16,
        Codec::Hevc => 0.11,
        Codec::Av1 => 0.09,
    }
}

/// Bitrate (kbps) at which `res` at `fps` looks sharp in `codec`.
///
/// Doubling the frame rate doesn't double the bits: consecutive frames are
/// more alike the closer together they are, so each extra frame is cheaper.
/// The 0.75 power is the usual rule of thumb for screen and game content.
pub fn sharp_kbps(codec: Codec, res: Resolution, fps: u16) -> u32 {
    // Bits needed at 60 fps = pixels x 60 x bits-per-pixel; more frames cost
    // less than proportionally (power 0.75). Result in kbps, 2 to 400 Mbps.
    let base = res.pixels() as f64 * 60.0 * bits_per_pixel(codec);
    let scale = (f64::from(fps.max(1)) / 60.0).powf(0.75);
    (base * scale / 1000.0).clamp(2_000.0, 400_000.0) as u32
}

/// Picks the frame rate the connection can afford at full sharpness.
#[derive(Debug, Clone)]
pub struct FpsLadder {
    /// Highest allowed: the lower of both screens and the viewer's setting.
    max: u16,
    current: u16,
    /// The viewer chose a number rather than "Automatic": never move.
    fixed: bool,
    /// Consecutive reports in which the next rung up would have fitted.
    up_streak: u32,
}

impl FpsLadder {
    /// Clean reports (seconds) in a row before we climb.
    const UP_AFTER: u32 = 3;
    /// Step down when the bitrate falls below this share of what the
    /// current rung needs. Lower than the climb threshold (1.0) so the two
    /// never chase each other.
    const DOWN_BELOW: f64 = 0.8;

    /// `max_fps`: the cap. `fixed`: hold exactly that (viewer picked a number).
    pub fn new(max_fps: u16, fixed: bool) -> Self {
        // The lowest allowed is 1 frame a second (avoids dividing by zero).
        let max = max_fps.max(1);
        // Start at 60 (or the cap, if lower) and climb as the connection
        // proves itself: starting at 240 on a weak link would open with a
        // blurry, stuttering second.
        let current = if fixed { max } else { max.min(MIN_FPS) };
        Self { max, current, fixed, up_streak: 0 }
    }

    pub fn current(&self) -> u16 {
        self.current
    }

    pub fn max(&self) -> u16 {
        self.max
    }

    /// The rungs we may use, fastest first. Always includes `max` itself
    /// (a 165 Hz screen gets 165, a 50 Hz one gets 50).
    fn rungs(&self) -> Vec<u16> {
        // Steps below the screen's maximum and not below 60, plus the maximum.
        let mut r: Vec<u16> = FPS_RUNGS.iter().copied().filter(|&f| f < self.max && f >= MIN_FPS).collect();
        r.insert(0, self.max);
        r
    }

    /// Feed what the bitrate controller allows right now. `clean`: the
    /// last report had no trouble (no cut). Returns the new frame rate if
    /// it changed.
    pub fn on_bitrate(&mut self, codec: Codec, res: Resolution, kbps: u32, clean: bool) -> Option<u16> {
        // A fixed choice, or a screen that cannot go above 60: nothing to decide.
        if self.fixed || self.max <= MIN_FPS {
            return None;
        }
        let before = self.current;
        // Would this frame rate look sharp at the bitrate we have (with a margin)?
        let fits = |fps: u16, share: f64| f64::from(kbps) >= f64::from(sharp_kbps(codec, res, fps)) * share;
        let rungs = self.rungs();
        // Not enough for the current rate: drop.
        if !fits(self.current, Self::DOWN_BELOW) && self.current > MIN_FPS {
            // Straight to the fastest rung that fits (not one step a second:
            // a sudden drop in bandwidth needs a sudden answer).
            self.current = rungs
                .iter()
                .copied()
                .filter(|&f| f < self.current)
                .find(|&f| fits(f, 0.9))
                .unwrap_or(MIN_FPS.min(self.max));
            self.up_streak = 0;
        } else if let Some(up) = rungs.iter().copied().filter(|&f| f > self.current).find(|&f| fits(f, 1.0)) {
            // Climb to the fastest rung the connection has room for, once it
            // has stayed clean for a few seconds.
            if clean {
                self.up_streak += 1;
                if self.up_streak >= Self::UP_AFTER {
                    self.current = up;
                    self.up_streak = 0;
                }
            } else {
                self.up_streak = 0;
            }
        } else {
            self.up_streak = 0;
        }
        (self.current != before).then_some(self.current)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const P1080: Resolution = Resolution { width: 1920, height: 1080 };

    #[test]
    fn sharp_bitrates_are_sensible() {
        let h264 = sharp_kbps(Codec::H264, P1080, 60);
        let hevc = sharp_kbps(Codec::Hevc, P1080, 60);
        let av1 = sharp_kbps(Codec::Av1, P1080, 60);
        assert!(h264 > hevc && hevc > av1);
        assert!((10_000..25_000).contains(&hevc), "{hevc}");
        let hevc240 = sharp_kbps(Codec::Hevc, P1080, 240);
        assert!(hevc240 > 2 * hevc && hevc240 < 4 * hevc, "{hevc240}");
    }

    #[test]
    fn climbs_after_a_few_clean_seconds_and_stops_at_the_cap() {
        let mut l = FpsLadder::new(200, false);
        assert_eq!(l.current(), 60);
        // A trouble report in between restarts the count.
        assert_eq!(l.on_bitrate(Codec::Hevc, P1080, 120_000, true), None);
        assert_eq!(l.on_bitrate(Codec::Hevc, P1080, 120_000, false), None);
        assert_eq!(l.on_bitrate(Codec::Hevc, P1080, 120_000, true), None);
        assert_eq!(l.on_bitrate(Codec::Hevc, P1080, 120_000, true), None);
        // Room for 200: straight there, not one rung at a time.
        assert_eq!(l.on_bitrate(Codec::Hevc, P1080, 120_000, true), Some(200));
        for _ in 0..50 {
            assert_eq!(l.on_bitrate(Codec::Hevc, P1080, 120_000, true), None);
        }
        // A connection with room for ~100 only climbs that far.
        let mut m = FpsLadder::new(240, false);
        let room = sharp_kbps(Codec::Hevc, P1080, 100);
        for _ in 0..20 {
            m.on_bitrate(Codec::Hevc, P1080, room, true);
        }
        assert_eq!(m.current(), 100);
    }

    #[test]
    fn weak_link_drops_straight_to_what_fits_but_never_below_60() {
        let mut l = FpsLadder::new(240, false);
        for _ in 0..100 {
            l.on_bitrate(Codec::Hevc, P1080, 200_000, true);
        }
        assert_eq!(l.current(), 240);
        // The link falls to ~20 Mbps: one step down to a rung that fits.
        let to = l.on_bitrate(Codec::Hevc, P1080, 20_000, false).unwrap();
        assert!(f64::from(sharp_kbps(Codec::Hevc, P1080, to)) * 0.9 <= 20_000.0, "{to}");
        assert!(to >= 60);
        // Collapse: 60, not lower. Sharpness gives way from here.
        l.on_bitrate(Codec::Hevc, P1080, 3_000, false);
        assert_eq!(l.current(), 60);
        l.on_bitrate(Codec::Hevc, P1080, 2_000, false);
        assert_eq!(l.current(), 60);
    }

    #[test]
    fn small_wobbles_dont_move_it() {
        let mut l = FpsLadder::new(144, false);
        for _ in 0..100 {
            l.on_bitrate(Codec::Hevc, P1080, 60_000, true);
        }
        assert_eq!(l.current(), 144);
        let need = sharp_kbps(Codec::Hevc, P1080, 144);
        // 15% under what 144 needs: still above the step-down line.
        for _ in 0..30 {
            assert_eq!(l.on_bitrate(Codec::Hevc, P1080, need * 85 / 100, true), None);
        }
    }

    #[test]
    fn a_chosen_number_is_kept_and_a_slow_screen_is_respected() {
        let mut fixed = FpsLadder::new(120, true);
        assert_eq!(fixed.current(), 120);
        assert_eq!(fixed.on_bitrate(Codec::Hevc, P1080, 2_000, false), None);
        let mut slow = FpsLadder::new(50, false);
        assert_eq!(slow.current(), 50);
        assert_eq!(slow.on_bitrate(Codec::Hevc, P1080, 2_000, false), None);
        // A 165 Hz screen can reach exactly 165.
        let mut odd = FpsLadder::new(165, false);
        for _ in 0..200 {
            odd.on_bitrate(Codec::Hevc, P1080, 200_000, true);
        }
        assert_eq!(odd.current(), 165);
    }
}

/// Lowers the picture size while the host can't compress fast enough, and
/// raises it again once it can.
///
/// In plain words: on the owner's PC (Intel Arc 130T, 2880x1800, Valorant
/// running) compressing one picture took 15 to 25 ms, so the stream topped
/// out near 40 fps and every picture arrived late. A smaller picture is
/// quicker to compress. This watches the compress time every couple of
/// seconds while the screen is busy (a game, a video) and steps the height
/// down (1440, 1080, 900, 720) until it fits the frame budget, then back up
/// when there is clearly room. A still screen never counts: a slow compress
/// of one picture a second costs nothing.
///
/// The second reason to go smaller is the connection (`on_bitrate`): the
/// owner's PC-to-Mac Wi-Fi carried only 11-16 Mbps cleanly, a third of what
/// 2880x1800 needs to look sharp, so every picture was blurry. A smaller
/// picture with enough bits per pixel looks sharper than a big starved one.
/// The size sent is the smaller of the two limits.
#[derive(Debug, Clone)]
pub struct SizeGovernor {
    /// Index into [`Self::STEPS`] from compress speed; 0 = full size.
    step: usize,
    /// Consecutive windows that were too slow / had room.
    slow: u32,
    roomy: u32,
    /// Windows left before stepping up is considered (after a step down).
    hold: u32,
    /// Index into [`Self::STEPS`] from the bitrate the connection allows.
    bits_step: usize,
    /// Consecutive windows starved of bits / with plenty for a bigger size.
    starved: u32,
    plenty: u32,
    /// Was the last window a busy screen?
    busy: bool,
}

impl Default for SizeGovernor {
    fn default() -> Self {
        Self::new()
    }
}

impl SizeGovernor {
    /// Height limits, `0` = none (the picture's own size).
    pub const STEPS: [u32; 5] = [0, 1440, 1080, 900, 720];
    /// Use this share of a frame's time at most for compressing it.
    const BUSY: f64 = 0.85;
    /// Step up only if the bigger picture would still fit in this share.
    const ROOM: f64 = 0.6;
    /// Windows in a row before acting: down quickly, up slowly.
    const DOWN_AFTER: u32 = 2;
    const UP_AFTER: u32 = 8;
    /// Windows to wait after a step down before trying bigger again.
    const HOLD_AFTER_DOWN: u32 = 15;

    pub fn new() -> Self {
        Self { step: 0, slow: 0, roomy: 0, hold: 0, bits_step: 0, starved: 0, plenty: 0, busy: false }
    }

    /// The current height limit (None: full size).
    pub fn limit(&self) -> Option<u32> {
        Some(Self::STEPS[self.step.max(self.bits_step)]).filter(|&h| h > 0)
    }

    /// Step down for bits when the bitrate is under this share of what the
    /// current size needs to look sharp...
    const STARVED: f64 = 0.7;
    /// ...to the biggest size that gets at least this share.
    const ENOUGH: f64 = 0.85;
    /// Step up when the bitrate covers all of what the bigger size needs.
    const PLENTY: f64 = 1.0;
    const BITS_DOWN_AFTER: u32 = 2;
    const BITS_UP_AFTER: u32 = 4;

    /// The picture at step `i` for a screen of size `full`.
    fn res_at(full: Resolution, i: usize) -> Resolution {
        let h = Self::STEPS[i];
        if h == 0 || h >= full.height {
            return full;
        }
        let w = (u64::from(full.width) * u64::from(h) / u64::from(full.height.max(1))) as u32;
        Resolution::new(w & !1, h)
    }

    /// Feed the bitrate the connection allows right now, once per measuring
    /// window, after [`Self::update`]. `full`: the screen's own size.
    /// Returns the new limit when it changes. Steps down only on a busy
    /// screen (a still picture sharpens over a few frames at any bitrate);
    /// steps back up whenever there is plenty.
    pub fn on_bitrate(&mut self, codec: Codec, full: Resolution, fps: u16, kbps: u32) -> Option<Option<u32>> {
        if kbps == 0 || full.height == 0 {
            return None;
        }
        let before = self.limit();
        let have = f64::from(kbps);
        let need = |i: usize| f64::from(sharp_kbps(codec, Self::res_at(full, i), fps));
        if self.busy && have < need(self.bits_step) * Self::STARVED {
            self.plenty = 0;
            self.starved += 1;
            if self.starved >= Self::BITS_DOWN_AFTER && self.bits_step + 1 < Self::STEPS.len() {
                let now = Self::res_at(full, self.bits_step);
                let mut next = self.bits_step + 1;
                while next + 1 < Self::STEPS.len()
                    && (Self::res_at(full, next) == now || have < need(next) * Self::ENOUGH)
                {
                    next += 1;
                }
                self.bits_step = next;
                self.starved = 0;
            }
        } else if self.bits_step > 0 && have >= need(self.bits_step - 1) * Self::PLENTY {
            self.starved = 0;
            self.plenty += 1;
            if self.plenty >= Self::BITS_UP_AFTER {
                self.bits_step -= 1;
                self.plenty = 0;
            }
        } else {
            self.starved = 0;
            self.plenty = 0;
        }
        let after = self.limit();
        (after != before).then_some(after)
    }

    /// Feed one measuring window. `target_fps`: what we try to send (the
    /// lower of the ladder and the screen). `screen_fps`: new pictures the
    /// screen gave; `sent_fps`: pictures compressed (new ones plus resends
    /// of a still picture). `encode_ms`: average compress time. `height`:
    /// the height we encoded at. Returns the new limit when it changes.
    pub fn update(
        &mut self,
        target_fps: u16,
        screen_fps: f64,
        sent_fps: f64,
        encode_ms: f64,
        height: u32,
    ) -> Option<Option<u32>> {
        let target = f64::from(target_fps.max(1));
        let budget_ms = 1000.0 / target;
        // Only a busy screen tells us anything (games, video, scrolling):
        // nearly every picture we compressed was a new one. Not "the screen
        // gave many": capture shares a loop with compressing, so a slow
        // encoder also caps how many new pictures we collect (the owner's
        // PC in Valorant: 30 new, 33 sent, at 15 ms each).
        let busy = screen_fps >= 10.0 && screen_fps >= sent_fps * 0.7;
        self.busy = busy;
        self.hold = self.hold.saturating_sub(1);
        if !busy || encode_ms <= 0.0 {
            self.slow = 0;
            self.roomy = 0;
            return None;
        }
        let before = self.limit();
        if encode_ms > budget_ms * Self::BUSY {
            self.roomy = 0;
            self.slow += 1;
            if self.slow >= Self::DOWN_AFTER && self.step + 1 < Self::STEPS.len() {
                // Jump straight to the step whose pixel count fits (time
                // grows about with pixels), at least one step.
                let mut next = self.step + 1;
                while next + 1 < Self::STEPS.len() {
                    let h = f64::from(Self::STEPS[next].min(height.max(1)));
                    let ratio = (h / f64::from(height.max(1))).powi(2);
                    if encode_ms * ratio <= budget_ms * Self::BUSY {
                        break;
                    }
                    next += 1;
                }
                self.step = next;
                self.slow = 0;
                self.hold = Self::HOLD_AFTER_DOWN;
            }
        } else {
            self.slow = 0;
            // Would the next bigger size still fit comfortably?
            let bigger = if self.step == 0 { None } else { Some(Self::STEPS[self.step - 1]) };
            let fits = bigger.is_some_and(|b| {
                let b = if b == 0 { height * 2 } else { b }; // full size: assume up to twice as tall
                let ratio = (f64::from(b) / f64::from(height.max(1))).powi(2);
                encode_ms * ratio <= budget_ms * Self::ROOM
            });
            if fits && self.hold == 0 {
                self.roomy += 1;
                if self.roomy >= Self::UP_AFTER {
                    self.step -= 1;
                    self.roomy = 0;
                }
            } else {
                self.roomy = 0;
            }
        }
        let after = self.limit();
        (after != before).then_some(after)
    }
}

#[cfg(test)]
mod size_tests {
    use super::*;

    #[test]
    fn slow_compress_on_a_busy_screen_steps_down_far_enough() {
        // The owner's PC: 2880x1800, 60 fps target, 23 ms per picture.
        let mut g = SizeGovernor::new();
        assert_eq!(g.update(60, 55.0, 55.0, 23.0, 1800), None);
        let new = g.update(60, 55.0, 55.0, 23.0, 1800);
        // 16.7 ms budget x 0.85 = 14.2 ms; 1440/1800 squared = 0.64 -> 14.7
        // ms (too slow), 1080 -> 8.3 ms: straight to 1080.
        assert_eq!(new, Some(Some(1080)));
    }

    #[test]
    fn a_still_screen_never_changes_the_size() {
        let mut g = SizeGovernor::new();
        for _ in 0..20 {
            assert_eq!(g.update(60, 2.0, 8.0, 25.0, 1800), None);
        }
        assert_eq!(g.limit(), None);
    }

    #[test]
    fn steps_back_up_when_there_is_clearly_room() {
        let mut g = SizeGovernor::new();
        g.update(60, 55.0, 55.0, 23.0, 1800);
        g.update(60, 55.0, 55.0, 23.0, 1800);
        assert_eq!(g.limit(), Some(1080));
        // Game closed / lighter scene: 3 ms at 1080. 1440 would be ~5.3 ms.
        let mut changed = None;
        for _ in 0..40 {
            if let Some(c) = g.update(60, 55.0, 55.0, 3.0, 1080) {
                changed = Some(c);
                break;
            }
        }
        assert_eq!(changed, Some(Some(1440)));
    }

    const FULL: Resolution = Resolution { width: 2880, height: 1800 };

    #[test]
    fn a_thin_connection_on_a_busy_screen_sends_a_smaller_sharp_picture() {
        // The owner's Wi-Fi: about 12 Mbps clean. Full size needs ~34 Mbps;
        // 1440 ~22 (too much); 1080 ~12.3: 12 is over 85% of that.
        let mut g = SizeGovernor::new();
        g.update(60, 55.0, 55.0, 8.0, 1800); // busy, compressing fast enough
        assert_eq!(g.on_bitrate(Codec::Hevc, FULL, 60, 12_000), None);
        g.update(60, 55.0, 55.0, 8.0, 1800);
        assert_eq!(g.on_bitrate(Codec::Hevc, FULL, 60, 12_000), Some(Some(1080)));
        // The same wobbly link (11-16 Mbps) holds it there.
        for kbps in [11_000, 16_000, 13_000, 11_500, 15_000, 12_000] {
            g.update(60, 55.0, 55.0, 4.0, 1080);
            assert_eq!(g.on_bitrate(Codec::Hevc, FULL, 60, kbps), None);
        }
    }

    #[test]
    fn a_still_screen_keeps_full_size_on_a_thin_connection() {
        let mut g = SizeGovernor::new();
        for _ in 0..10 {
            g.update(60, 2.0, 8.0, 20.0, 1800);
            assert_eq!(g.on_bitrate(Codec::Hevc, FULL, 60, 5_000), None);
        }
    }

    #[test]
    fn plenty_of_bits_brings_full_size_back() {
        let mut g = SizeGovernor::new();
        for _ in 0..2 {
            g.update(60, 55.0, 55.0, 8.0, 1800);
            g.on_bitrate(Codec::Hevc, FULL, 60, 12_000);
        }
        assert_eq!(g.limit(), Some(1080));
        // A cable: 80 Mbps. Back up a step at a time, 4 windows each.
        let mut seen = vec![];
        for _ in 0..12 {
            g.update(60, 55.0, 55.0, 4.0, 1080);
            if let Some(l) = g.on_bitrate(Codec::Hevc, FULL, 60, 80_000) {
                seen.push(l);
            }
        }
        assert_eq!(seen, vec![Some(1440), None]);
    }

    #[test]
    fn the_smaller_of_speed_and_bits_wins() {
        let mut g = SizeGovernor::new();
        // Bits say 1080...
        for _ in 0..2 {
            g.update(60, 55.0, 55.0, 8.0, 1800);
            g.on_bitrate(Codec::Hevc, FULL, 60, 12_000);
        }
        assert_eq!(g.limit(), Some(1080));
        // ...then compressing is too slow even at 1080 (12 ms vs 14.2
        // budget is fine; 16 ms is not): speed says smaller still.
        g.update(60, 55.0, 55.0, 16.0, 1080);
        let c = g.update(60, 55.0, 55.0, 16.0, 1080);
        assert!(matches!(c, Some(Some(h)) if h < 1080), "{c:?}");
    }

    #[test]
    fn fast_enough_at_full_size_stays_full() {
        let mut g = SizeGovernor::new();
        for _ in 0..20 {
            assert_eq!(g.update(60, 60.0, 60.0, 6.0, 1800), None);
        }
        assert_eq!(g.limit(), None);
    }
}
