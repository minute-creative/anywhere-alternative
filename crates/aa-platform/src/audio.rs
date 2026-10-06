//! Audio codec and playback, portable parts.
//!
//! [`OpusEncoder`] / [`OpusDecoder`] wrap libopus for 48 kHz stereo 10 ms
//! frames. [`Player`] (Mac/Windows) feeds decoded samples to the default
//! output through `cpal` with a small jitter buffer: audio needs *some*
//! buffering because a late sample is an audible click, unlike a late video
//! frame which just gets skipped. The buffer is adaptive: it starts at
//! 60 ms and grows only when the link actually makes it run dry.

use aa_core::audio::{CHANNELS, FRAME_SAMPLES, SAMPLE_RATE};

use crate::{PlatformError, Result};

/// Interleaved stereo i16 samples for one 10 ms frame.
pub const FRAME_LEN_I16: usize = FRAME_SAMPLES * CHANNELS as usize;

pub struct OpusEncoder {
    inner: opus::Encoder,
    out: Vec<u8>,
}

impl std::fmt::Debug for OpusEncoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpusEncoder").finish_non_exhaustive()
    }
}

impl OpusEncoder {
    pub fn new(bitrate: u32) -> Result<Self> {
        // Application::Audio (not LowDelay): at 10 ms frames the algorithmic
        // delay difference is ~2.5 ms and Audio mode sounds noticeably better
        // on music and game sound.
        let mut inner = opus::Encoder::new(SAMPLE_RATE, opus::Channels::Stereo, opus::Application::Audio)
            .map_err(|e| PlatformError::Backend(anyhow::anyhow!("opus encoder: {e}")))?;
        inner
            .set_bitrate(opus::Bitrate::Bits(i32::try_from(bitrate).unwrap_or(i32::MAX)))
            .map_err(|e| PlatformError::Backend(anyhow::anyhow!("opus bitrate: {e}")))?;
        // Inband FEC: each packet carries a low-quality copy of the previous
        // one, so a single lost packet costs nothing audible on Wi-Fi.
        let _ = inner.set_inband_fec(true);
        let _ = inner.set_packet_loss_perc(5);
        Ok(Self { inner, out: vec![0u8; 1500] })
    }

    /// Encode exactly one frame (`FRAME_LEN_I16` interleaved samples).
    pub fn encode(&mut self, pcm: &[i16]) -> Result<&[u8]> {
        debug_assert_eq!(pcm.len(), FRAME_LEN_I16);
        let n = self
            .inner
            .encode(pcm, &mut self.out)
            .map_err(|e| PlatformError::Backend(anyhow::anyhow!("opus encode: {e}")))?;
        Ok(&self.out[..n])
    }
}

pub struct OpusDecoder {
    inner: opus::Decoder,
}

impl std::fmt::Debug for OpusDecoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpusDecoder").finish_non_exhaustive()
    }
}

impl OpusDecoder {
    pub fn new() -> Result<Self> {
        let inner = opus::Decoder::new(SAMPLE_RATE, opus::Channels::Stereo)
            .map_err(|e| PlatformError::Backend(anyhow::anyhow!("opus decoder: {e}")))?;
        Ok(Self { inner })
    }

    /// Decode one packet into `out` (≥ `FRAME_LEN_I16`). Returns samples per channel.
    pub fn decode(&mut self, packet: &[u8], out: &mut [i16]) -> Result<usize> {
        self.inner.decode(packet, out, false).map_err(|e| PlatformError::Backend(anyhow::anyhow!("opus decode: {e}")))
    }

    /// Conceal one lost packet: Opus synthesises plausible audio from what
    /// came before, far less jarring than a 10 ms hole.
    pub fn conceal(&mut self, out: &mut [i16]) -> Result<usize> {
        self.inner.decode(&[], out, false).map_err(|e| PlatformError::Backend(anyhow::anyhow!("opus plc: {e}")))
    }
}

/// Source of system audio on the host.
pub trait AudioCapture: Send {
    /// Block until one 10 ms frame is available and write it to `pcm`
    /// (`FRAME_LEN_I16` interleaved i16). Returns false if nothing was
    /// playing (silence), so the host can skip sending.
    fn next_frame(&mut self, pcm: &mut [i16]) -> Result<bool>;
}

/// Control over the host's own speakers (not the stream): used to silence
/// the room while someone streams from the next desk over.
pub trait SpeakerControl: Send {
    /// Mute or restore the local output. Implementations remember the
    /// state they found and put it back on `restore`, so a crashed session
    /// never leaves the PC silently muted.
    fn set_muted(&mut self, muted: bool) -> Result<()>;
    fn restore(&mut self) -> Result<()>;
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
pub use player::Player;

#[cfg(any(target_os = "macos", target_os = "windows"))]
mod player {
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};
    use std::time::Instant;

    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

    use super::FRAME_LEN_I16;
    use crate::{PlatformError, Result};

    /// One 10 ms frame in interleaved samples.
    const FRAME: usize = FRAME_LEN_I16;
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

    /// Adaptive jitter buffer.
    ///
    /// Why adaptive: on a calm link 30 ms is plenty and anything more is
    /// avoidable lag; on a link whose delay swings by 150 ms, 30 ms means a
    /// click every few packets. Instead of guessing a number, we start low,
    /// grow each time we run dry, and creep back down while things are calm.
    /// The viewer always pays the smallest latency the link allows.
    struct Jitter {
        queue: VecDeque<i16>,
        /// Depth we try to hold, in frames.
        target: usize,
        /// After an underrun we stay silent until the queue refills to
        /// `target`; starting early just produces another click.
        filling: bool,
        last_underrun: Instant,
        pub underruns: u64,
    }

    impl Jitter {
        fn new() -> Self {
            Self {
                queue: VecDeque::with_capacity(FRAME * MAX_FRAMES),
                target: START_FRAMES,
                filling: true,
                last_underrun: Instant::now(),
                underruns: 0,
            }
        }

        fn push(&mut self, pcm: &[i16]) {
            // Burst arrived: drop the oldest down to target, not to zero, so
            // the next gap still has cushion.
            let cap = FRAME * (self.target + 10);
            if self.queue.len() + pcm.len() > cap {
                let keep = FRAME * self.target;
                let excess = (self.queue.len() + pcm.len()).saturating_sub(keep);
                let excess = excess.min(self.queue.len());
                self.queue.drain(..excess);
            }
            self.queue.extend(pcm.iter().copied());
            if self.filling && self.queue.len() >= FRAME * self.target {
                self.filling = false;
            }
            // Calm for a while: try a little less latency.
            if self.target > MIN_FRAMES && self.last_underrun.elapsed().as_secs() >= SHRINK_AFTER_SECS {
                self.target -= 1;
                self.last_underrun = Instant::now();
            }
        }

        fn pull(&mut self, out: &mut [f32]) {
            if self.filling || self.queue.len() < out.len() {
                if !self.filling && !self.queue.is_empty() {
                    // Ran dry mid-stream: the link stalled longer than our
                    // cushion. Hold more next time.
                    self.underruns += 1;
                    self.target = (self.target + GROW_FRAMES).min(MAX_FRAMES);
                    self.last_underrun = Instant::now();
                    self.filling = true;
                }
                out.fill(0.0);
                return;
            }
            for s in out.iter_mut() {
                *s = self.queue.pop_front().map_or(0.0, |v| f32::from(v) / 32768.0);
            }
        }
    }

    /// Plays decoded frames through the default output device.
    pub struct Player {
        jitter: Arc<Mutex<Jitter>>,
        _stream: cpal::Stream,
    }

    impl std::fmt::Debug for Player {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("Player").finish_non_exhaustive()
        }
    }

    impl Player {
        pub fn new() -> Result<Self> {
            let host = cpal::default_host();
            let device = host
                .default_output_device()
                .ok_or_else(|| PlatformError::Unavailable("no audio output device".into()))?;
            let config = cpal::StreamConfig {
                channels: 2,
                sample_rate: aa_core::audio::SAMPLE_RATE,
                // Small device buffer: the jitter buffer above is where the
                // latency budget lives, not the OS queue.
                buffer_size: cpal::BufferSize::Fixed(240),
            };
            let jitter = Arc::new(Mutex::new(Jitter::new()));
            let j = Arc::clone(&jitter);
            let build = |config: cpal::StreamConfig| {
                let j = Arc::clone(&j);
                device.build_output_stream(
                    config,
                    move |out: &mut [f32], _| j.lock().expect("jitter buffer").pull(out),
                    |e| tracing::warn!("audio output error: {e}"),
                    None,
                )
            };
            // Some devices refuse a fixed buffer size; fall back to default.
            let stream = match build(config) {
                Ok(s) => s,
                Err(_) => build(cpal::StreamConfig { buffer_size: cpal::BufferSize::Default, ..config })
                    .map_err(|e| PlatformError::Backend(anyhow::anyhow!("audio output stream: {e}")))?,
            };
            stream.play().map_err(|e| PlatformError::Backend(anyhow::anyhow!("audio play: {e}")))?;
            let name = device.description().map(|d| d.name().to_owned()).unwrap_or_default();
            tracing::info!(device = name, "audio output ready");
            Ok(Self { jitter, _stream: stream })
        }

        /// Queue one decoded frame.
        pub fn push(&self, pcm: &[i16]) {
            self.jitter.lock().expect("jitter buffer").push(pcm);
        }

        /// (current target depth in ms, underruns so far) for the stats line.
        pub fn stats(&self) -> (u32, u64) {
            let j = self.jitter.lock().expect("jitter buffer");
            ((j.target * 10) as u32, j.underruns)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_decode_round_trip_preserves_a_tone() {
        let mut enc = OpusEncoder::new(128_000).unwrap();
        let mut dec = OpusDecoder::new().unwrap();
        // 440 Hz sine, stereo.
        let mut pcm = vec![0i16; FRAME_LEN_I16];
        for i in 0..FRAME_SAMPLES {
            let v = ((i as f32 / SAMPLE_RATE as f32) * 440.0 * std::f32::consts::TAU).sin() * 10_000.0;
            pcm[i * 2] = v as i16;
            pcm[i * 2 + 1] = v as i16;
        }
        // Opus needs a few frames to settle; decode the last one.
        let mut out = vec![0i16; FRAME_LEN_I16];
        let mut n = 0;
        for _ in 0..5 {
            let pkt = enc.encode(&pcm).unwrap().to_vec();
            assert!(pkt.len() < 400, "10 ms at 128 kbps should be ~160 bytes, got {}", pkt.len());
            n = dec.decode(&pkt, &mut out).unwrap();
        }
        assert_eq!(n, FRAME_SAMPLES);
        let energy: f64 = out.iter().map(|&s| f64::from(s).powi(2)).sum::<f64>() / out.len() as f64;
        assert!(energy > 1.0e6, "decoded audio should carry the tone, energy={energy}");
    }

    #[test]
    fn concealment_produces_a_frame() {
        let mut dec = OpusDecoder::new().unwrap();
        let mut out = vec![0i16; FRAME_LEN_I16];
        assert_eq!(dec.conceal(&mut out).unwrap(), FRAME_SAMPLES);
    }
}
