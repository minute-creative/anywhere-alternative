//! The sound-shaping part of playback, with no device code, so it builds
//! and is tested everywhere: the adaptive jitter buffer, conversion to
//! whatever rate and channel count the output device wants, the fade-in
//! after a gap, and the loudness limiter.
//!
//! Why resampling lives here: the stream is always 48 kHz stereo, but
//! devices are not. Bluetooth headphones switch to 16 or 24 kHz mono the
//! moment their microphone is in use (the "hands-free" profile), and some
//! USB headsets only do 44.1 kHz. The first player asked for 48 kHz stereo
//! and simply failed on those.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Instant;

use aa_core::audio::{CHANNELS, FRAME_SAMPLES, SAMPLE_RATE};

/// One 10 ms frame in interleaved stereo samples.
const FRAME: usize = FRAME_SAMPLES * CHANNELS as usize;
/// Starting depth. 60 ms covers ordinary Wi-Fi jitter; a bad link will
/// push it up from here.
const START_FRAMES: usize = 6;
const MIN_FRAMES: usize = 4;
/// Never buffer more than this (300 ms): past it, sync with the picture
/// is clearly gone and we would rather resync.
const MAX_FRAMES: usize = 30;
/// Each underrun adds this much (20 ms).
const GROW_FRAMES: usize = 2;
/// Shrink by one frame after this long without an underrun.
const SHRINK_AFTER_SECS: u64 = 20;

/// Default boost: +6 dB (about twice as loud). PC game and video audio is
/// mastered with lots of headroom, and the limiter stops the loud parts
/// from clipping, so this is safe.
pub const DEFAULT_BOOST_DB: f32 = 6.0;

/// Extra loudness on top of what arrives, in hundredths of a dB. Written by
/// the viewer's overlay, read by the audio callback.
static BOOST_CENTI_DB: AtomicU32 = AtomicU32::new(600);

/// Set the volume boost in dB (0 = exactly what arrives, max 18).
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub fn set_boost_db(db: f32) {
    BOOST_CENTI_DB.store((db.clamp(0.0, 18.0) * 100.0).round() as u32, Ordering::Relaxed);
}

#[allow(clippy::cast_precision_loss)]
fn boost() -> f32 {
    10f32.powf(BOOST_CENTI_DB.load(Ordering::Relaxed) as f32 / 2000.0)
}

/// Loudest a sample may get after the boost; a hair under full scale.
const CEILING: f32 = 0.97;
/// Fade-in length after (re)starting playback, in output samples.
const FADE_SAMPLES: usize = 480;

/// Makes quiet sound louder without letting loud sound clip.
///
/// A plain gain would distort on explosions and music peaks. The limiter
/// looks at each output block; if the boosted peak would pass the ceiling
/// it turns the gain down *just enough*, instantly, then lets it recover
/// slowly (about half a second) so you don't hear it pumping. Gain changes
/// are ramped across the block so there is no zipper noise.
#[derive(Debug)]
struct Limiter {
    gain: f32,
    /// False for the microphone path: the PC's apps do their own levels.
    boosted: bool,
}

impl Limiter {
    #[allow(clippy::cast_precision_loss)] // block sizes are a few hundred samples
    fn process(&mut self, out: &mut [f32]) {
        let boost = if self.boosted { boost() } else { 1.0 };
        let peak = out.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        let safe = if peak * boost > CEILING { CEILING / peak } else { boost };
        let target = if safe < self.gain { safe } else { self.gain + (safe - self.gain) * 0.01 };
        let start = self.gain;
        let n = out.len().max(1) as f32;
        for (i, s) in out.iter_mut().enumerate() {
            let g = start + (target - start) * (i as f32 / n);
            *s = (*s * g).clamp(-CEILING, CEILING);
        }
        self.gain = target;
    }
}

/// Adaptive jitter buffer plus output conversion.
///
/// Why adaptive: on a calm link 30 ms is plenty and anything more is
/// avoidable lag; on a link whose delay swings by 150 ms, 30 ms means a
/// click every few packets. We start low, grow each time we run dry, and
/// creep back down while things are calm.
#[derive(Debug)]
pub struct Jitter {
    /// 48 kHz interleaved stereo.
    queue: VecDeque<i16>,
    /// Depth we try to hold, in frames.
    pub target: usize,
    /// After an underrun we stay silent until the queue refills to
    /// `target`; starting early just produces another click.
    filling: bool,
    last_underrun: Instant,
    pub underruns: u64,
    fade_left: usize,
    limiter: Limiter,
    /// Resampler read position, in input frames, between queue[0] and queue[1].
    pos: f64,
}

impl Jitter {
    /// `boosted`: apply the viewer's volume boost (speakers yes, mic no).
    pub fn new(boosted: bool) -> Self {
        Self {
            queue: VecDeque::with_capacity(FRAME * MAX_FRAMES),
            target: START_FRAMES,
            filling: true,
            last_underrun: Instant::now(),
            underruns: 0,
            fade_left: FADE_SAMPLES,
            limiter: Limiter { gain: if boosted { boost() } else { 1.0 }, boosted },
            pos: 0.0,
        }
    }

    /// Queue 48 kHz interleaved stereo samples.
    pub fn push(&mut self, pcm: &[i16]) {
        // Burst arrived: drop the oldest down to target, not to zero, so
        // the next gap still has cushion.
        let cap = FRAME * (self.target + 10);
        if self.queue.len() + pcm.len() > cap {
            let keep = FRAME * self.target;
            let excess = (self.queue.len() + pcm.len()).saturating_sub(keep).min(self.queue.len());
            // Whole stereo frames only, or left and right swap places.
            self.queue.drain(..excess & !1);
        }
        self.queue.extend(pcm.iter().copied());
        if self.filling && self.queue.len() >= FRAME * self.target {
            self.filling = false;
            self.fade_left = FADE_SAMPLES;
        }
        if self.target > MIN_FRAMES && self.last_underrun.elapsed().as_secs() >= SHRINK_AFTER_SECS {
            self.target -= 1;
            self.last_underrun = Instant::now();
        }
    }

    /// Forget everything queued (the output device changed).
    pub fn reset(&mut self) {
        self.queue.clear();
        self.filling = true;
        self.pos = 0.0;
    }

    fn sample(&self, frame: usize, ch: usize) -> f32 {
        self.queue.get(frame * 2 + ch).map_or(0.0, |&v| f32::from(v) / 32768.0)
    }

    /// Fill `out` (interleaved, `channels` per frame, at `rate` Hz).
    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    pub fn pull(&mut self, out: &mut [f32], channels: usize, rate: u32) {
        let channels = channels.max(1);
        let frames_out = out.len() / channels;
        let step = f64::from(SAMPLE_RATE) / f64::from(rate.max(1));
        // Input frames this block consumes, plus one for interpolation.
        let need = (self.pos + frames_out as f64 * step).ceil() as usize + 1;
        if self.filling || self.queue.len() / 2 < need {
            if !self.filling && !self.queue.is_empty() {
                // Ran dry mid-stream: the link stalled longer than our cushion.
                self.underruns += 1;
                self.target = (self.target + GROW_FRAMES).min(MAX_FRAMES);
                self.last_underrun = Instant::now();
                self.filling = true;
            }
            out.fill(0.0);
            return;
        }
        for f in 0..frames_out {
            let i = self.pos.floor() as usize;
            let t = (self.pos - self.pos.floor()) as f32;
            let left = self.sample(i, 0) * (1.0 - t) + self.sample(i + 1, 0) * t;
            let right = self.sample(i, 1) * (1.0 - t) + self.sample(i + 1, 1) * t;
            let frame = &mut out[f * channels..(f + 1) * channels];
            if channels == 1 {
                frame[0] = 0.5 * (left + right);
            } else {
                frame[0] = left;
                frame[1] = right;
                for extra in &mut frame[2..] {
                    *extra = 0.0; // surround channels stay silent
                }
            }
            self.pos += step;
        }
        let consumed = self.pos.floor() as usize;
        self.queue.drain(..(consumed * 2).min(self.queue.len()));
        self.pos -= consumed as f64;

        if self.fade_left > 0 {
            for s in out.iter_mut() {
                if self.fade_left == 0 {
                    break;
                }
                *s *= 1.0 - self.fade_left as f32 / FADE_SAMPLES as f32;
                self.fade_left -= 1;
            }
        }
        self.limiter.process(out);
    }
}

/// Turn device input (any rate, any channel count, f32) into 48 kHz stereo
/// 10 ms frames for the encoder. Used for the microphone.
#[derive(Debug, Default)]
pub struct InputFramer {
    /// Mono input samples not yet resampled.
    pending: VecDeque<f32>,
    pos: f64,
    /// 48 kHz stereo i16 waiting to fill a frame.
    out: Vec<i16>,
}

impl InputFramer {
    /// Feed one callback's worth of samples; complete frames go to `emit`.
    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    pub fn push(&mut self, input: &[f32], channels: usize, rate: u32, mut emit: impl FnMut(&[i16])) {
        let channels = channels.max(1);
        // A microphone is one voice: average the channels.
        for frame in input.chunks_exact(channels) {
            self.pending.push_back(frame.iter().sum::<f32>() / channels as f32);
        }
        let step = f64::from(rate.max(1)) / f64::from(SAMPLE_RATE);
        while self.pos.floor() as usize + 1 < self.pending.len() {
            let i = self.pos.floor() as usize;
            let t = (self.pos - self.pos.floor()) as f32;
            let v = self.pending[i] * (1.0 - t) + self.pending[i + 1] * t;
            let s = (v.clamp(-1.0, 1.0) * 32767.0) as i16;
            self.out.extend_from_slice(&[s, s]);
            self.pos += step;
            if self.out.len() == FRAME {
                emit(&self.out);
                self.out.clear();
            }
        }
        let consumed = self.pos.floor() as usize;
        self.pending.drain(..consumed.min(self.pending.len()));
        self.pos -= consumed as f64;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(frames: usize) -> Vec<i16> {
        (0..frames)
            .flat_map(|i| {
                let v = ((i as f32 / 48_000.0) * 440.0 * std::f32::consts::TAU).sin() * 8_000.0;
                [v as i16, (v * 0.5) as i16]
            })
            .collect()
    }

    fn filled() -> Jitter {
        let mut j = Jitter::new(false);
        j.push(&tone(48_000 / 10)); // 100 ms
        j
    }

    #[test]
    fn same_rate_stereo_passes_samples_through() {
        let mut j = filled();
        j.fade_left = 0;
        let mut out = vec![0.0f32; 480];
        j.pull(&mut out, 2, 48_000);
        let src = tone(240);
        for (o, s) in out.iter().zip(src.iter()) {
            assert!((o - f32::from(*s) / 32768.0).abs() < 1e-4);
        }
    }

    #[test]
    fn hands_free_24k_mono_consumes_input_twice_as_fast() {
        let mut j = filled();
        let before = j.queue.len();
        let mut out = vec![0.0f32; 240]; // 10 ms at 24 kHz mono
        j.pull(&mut out, 1, 24_000);
        let used_frames = (before - j.queue.len()) / 2;
        assert_eq!(used_frames, 480, "10 ms of 48 kHz input");
        assert!(out.iter().any(|s| s.abs() > 0.01), "sound came out");
    }

    #[test]
    fn surround_devices_get_left_right_and_silence() {
        let mut j = filled();
        j.fade_left = 0;
        let mut out = vec![1.0f32; 6 * 100];
        j.pull(&mut out, 6, 48_000);
        assert!(out.chunks(6).all(|f| f[2..].iter().all(|s| *s == 0.0)));
    }

    #[test]
    fn mic_framer_makes_48k_stereo_frames_from_44_1k_mono() {
        let mut fr = InputFramer::default();
        let mut frames = 0;
        let input: Vec<f32> =
            (0..44_100).map(|i| ((i as f32) / 44_100.0 * 300.0 * std::f32::consts::TAU).sin() * 0.3).collect();
        fr.push(&input, 1, 44_100, |f| {
            assert_eq!(f.len(), FRAME);
            assert_eq!(f[0], f[1], "mono duplicated to both channels");
            frames += 1;
        });
        assert!((99..=100).contains(&frames), "{frames} frames for one second");
    }

    #[test]
    fn limiter_never_clips() {
        let mut j = Jitter::new(true);
        j.push(&vec![i16::MAX; FRAME * 20]);
        let mut out = vec![0.0f32; 960];
        j.pull(&mut out, 2, 48_000);
        assert!(out.iter().all(|s| s.abs() <= CEILING));
    }
}
