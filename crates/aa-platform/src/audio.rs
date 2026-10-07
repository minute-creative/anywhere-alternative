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
        // Quality knobs. Each costs nothing that matters here: complexity 10
        // is still well under a millisecond per frame, full bandwidth keeps
        // the top octave (cymbals, sibilance), and Music stops Opus from
        // switching to its speech model and thinning out game sound.
        let _ = inner.set_complexity(10);
        let _ = inner.set_vbr(true);
        let _ = inner.set_max_bandwidth(opus::Bandwidth::Fullband);
        let _ = inner.set_signal(opus::Signal::Music);
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

    /// Rebuild the frame *before* `packet` from the low-quality copy the
    /// encoder tucks into every packet (inband FEC). Much closer to the real
    /// sound than plain concealment, which only guesses.
    pub fn recover_previous(&mut self, packet: &[u8], out: &mut [i16]) -> Result<usize> {
        self.inner.decode(packet, out, true).map_err(|e| PlatformError::Backend(anyhow::anyhow!("opus fec: {e}")))
    }
}

/// Turns `Kind::Audio`/`Kind::Mic` payloads back into 48 kHz stereo frames:
/// decode, conceal short gaps, rebuild the last lost frame from FEC, drop
/// late packets. The caller decides where the frames go.
pub struct Depacketizer {
    decoder: OpusDecoder,
    seq: aa_core::audio::AudioSequence,
    pcm: Vec<i16>,
    pub frames: u64,
    pub concealed: u64,
}

impl std::fmt::Debug for Depacketizer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Depacketizer")
            .field("frames", &self.frames)
            .field("concealed", &self.concealed)
            .finish_non_exhaustive()
    }
}

impl Depacketizer {
    /// Max consecutive lost frames we conceal; beyond this it was a real
    /// gap (silence or reconnect) and we just resync.
    const MAX_CONCEAL: u16 = 5;

    pub fn new() -> Result<Self> {
        Ok(Self {
            decoder: OpusDecoder::new()?,
            seq: aa_core::audio::AudioSequence::default(),
            pcm: vec![0i16; FRAME_LEN_I16],
            frames: 0,
            concealed: 0,
        })
    }

    pub fn handle(&mut self, mut payload: bytes::Bytes, mut play: impl FnMut(&[i16])) {
        let Some(header) = aa_core::audio::AudioHeader::read(&mut payload) else { return };
        let Some(lost) = self.seq.observe(header.frame_no) else { return };
        if lost > 0 && lost <= Self::MAX_CONCEAL {
            // Guess all but the last missing frame; the last one is rebuilt
            // from the copy carried inside this packet (inband FEC).
            for _ in 1..lost {
                if self.decoder.conceal(&mut self.pcm).is_ok() {
                    self.concealed += 1;
                    play(&self.pcm);
                }
            }
            if self.decoder.recover_previous(&payload, &mut self.pcm).is_ok()
                || self.decoder.conceal(&mut self.pcm).is_ok()
            {
                self.concealed += 1;
                play(&self.pcm);
            }
        }
        if self.decoder.decode(&payload, &mut self.pcm).is_ok() {
            self.frames += 1;
            play(&self.pcm);
        }
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

pub use crate::playout::{set_boost_db, DEFAULT_BOOST_DB};
#[cfg(any(target_os = "macos", target_os = "windows"))]
pub use player::{Mic, Player};

#[cfg(any(target_os = "macos", target_os = "windows"))]
mod player {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

    use crate::playout::{InputFramer, Jitter};
    use crate::{PlatformError, Result};

    fn err(what: &str, e: impl std::fmt::Display) -> PlatformError {
        PlatformError::Backend(anyhow::anyhow!("{what}: {e}"))
    }

    fn device_name(d: &cpal::Device) -> String {
        d.description().map(|d| d.name().to_owned()).unwrap_or_default()
    }

    /// Which output a player uses.
    #[derive(Debug, Clone)]
    enum Target {
        /// Whatever the OS calls the default right now, followed live.
        Default,
        /// The first output whose name contains one of these (a virtual
        /// microphone cable on the PC).
        Named(Vec<&'static str>),
    }

    /// Plays 48 kHz stereo frames on an output device, converting to
    /// whatever the device wants, and moving to the new default device when
    /// the user switches (Bluetooth headphones connect, cable unplugged).
    pub struct Player {
        jitter: Arc<Mutex<Jitter>>,
        stream: Option<cpal::Stream>,
        device: String,
        target: Target,
        /// Set by the stream's error callback (device unplugged).
        broken: Arc<AtomicBool>,
        last_check: Instant,
    }

    impl std::fmt::Debug for Player {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("Player").field("device", &self.device).finish_non_exhaustive()
        }
    }

    impl Player {
        /// The default output, with the viewer's volume boost.
        pub fn new() -> Result<Self> {
            Self::open(Target::Default, true)
        }

        /// The first output whose name contains any of `names`; no boost.
        pub fn on_device_named(names: &[&'static str]) -> Result<Self> {
            Self::open(Target::Named(names.to_vec()), false)
        }

        fn find(target: &Target) -> Option<cpal::Device> {
            let host = cpal::default_host();
            match target {
                Target::Default => host.default_output_device(),
                Target::Named(names) => host.output_devices().ok()?.find(|d| {
                    let n = device_name(d).to_ascii_lowercase();
                    names.iter().any(|w| n.contains(&w.to_ascii_lowercase()))
                }),
            }
        }

        fn open(target: Target, boosted: bool) -> Result<Self> {
            let mut p = Self {
                jitter: Arc::new(Mutex::new(Jitter::new(boosted))),
                stream: None,
                device: String::new(),
                target,
                broken: Arc::new(AtomicBool::new(false)),
                last_check: Instant::now(),
            };
            p.reopen()?;
            Ok(p)
        }

        /// (Re)build the stream on the target device.
        fn reopen(&mut self) -> Result<()> {
            self.stream = None;
            let device =
                Self::find(&self.target).ok_or_else(|| PlatformError::Unavailable("no such audio output".into()))?;
            let fallback = device.default_output_config().map_err(|e| err("output config", e))?;
            let stereo48 = |buffer| cpal::StreamConfig {
                channels: 2,
                sample_rate: aa_core::audio::SAMPLE_RATE,
                buffer_size: buffer,
            };
            // Preferred first: 48 kHz stereo with a small buffer (no
            // conversion, least latency). Then whatever the device wants:
            // e.g. Bluetooth hands-free is 16/24 kHz mono.
            let candidates = [
                stereo48(cpal::BufferSize::Fixed(240)),
                stereo48(cpal::BufferSize::Default),
                cpal::StreamConfig {
                    channels: fallback.channels(),
                    sample_rate: fallback.sample_rate(),
                    buffer_size: cpal::BufferSize::Default,
                },
            ];
            self.broken.store(false, Ordering::Relaxed);
            self.jitter.lock().expect("jitter").reset();
            let mut last_err = None;
            for config in candidates {
                let (channels, rate) = (usize::from(config.channels), config.sample_rate);
                let j = Arc::clone(&self.jitter);
                let broken = Arc::clone(&self.broken);
                match device.build_output_stream(
                    config,
                    move |out: &mut [f32], _| j.lock().expect("jitter").pull(out, channels, rate),
                    move |e| {
                        tracing::warn!("audio output error: {e}");
                        broken.store(true, Ordering::Relaxed);
                    },
                    None,
                ) {
                    Ok(stream) => {
                        stream.play().map_err(|e| err("audio play", e))?;
                        self.device = device_name(&device);
                        tracing::info!(device = self.device, channels, rate, "audio output ready");
                        self.stream = Some(stream);
                        return Ok(());
                    }
                    Err(e) => last_err = Some(e),
                }
            }
            Err(err("audio output stream", last_err.map_or_else(|| "no config".to_owned(), |e| e.to_string())))
        }

        /// Call often; at most once a second it checks whether the default
        /// output changed (or the device vanished) and moves the sound there.
        pub fn follow(&mut self) {
            if self.last_check.elapsed() < Duration::from_secs(1) {
                return;
            }
            self.last_check = Instant::now();
            let moved = matches!(self.target, Target::Default)
                && Self::find(&self.target).is_some_and(|d| device_name(&d) != self.device);
            if moved || self.broken.load(Ordering::Relaxed) || self.stream.is_none() {
                if let Err(e) = self.reopen() {
                    tracing::debug!("audio output not ready yet: {e}");
                }
            }
        }

        /// Queue one decoded 48 kHz stereo frame.
        pub fn push(&self, pcm: &[i16]) {
            self.jitter.lock().expect("jitter").push(pcm);
        }

        /// (current target depth in ms, underruns so far) for the stats line.
        pub fn stats(&self) -> (u32, u64) {
            let j = self.jitter.lock().expect("jitter");
            ((j.target * 10) as u32, j.underruns)
        }

        pub fn device(&self) -> &str {
            &self.device
        }
    }

    /// The default microphone, delivering 48 kHz stereo 10 ms frames.
    /// Like the player, it follows the default input when it changes (a
    /// headset connects).
    pub struct Mic {
        stream: Option<cpal::Stream>,
        frames: std::sync::mpsc::Receiver<Vec<i16>>,
        tx: std::sync::mpsc::SyncSender<Vec<i16>>,
        device: String,
        broken: Arc<AtomicBool>,
        last_check: Instant,
    }

    impl std::fmt::Debug for Mic {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("Mic").field("device", &self.device).finish_non_exhaustive()
        }
    }

    impl Mic {
        pub fn open() -> Result<Self> {
            // 20 frames = 200 ms of slack before we start dropping.
            let (tx, frames) = std::sync::mpsc::sync_channel(20);
            let mut m = Self {
                stream: None,
                frames,
                tx,
                device: String::new(),
                broken: Arc::new(AtomicBool::new(false)),
                last_check: Instant::now(),
            };
            m.reopen()?;
            Ok(m)
        }

        fn reopen(&mut self) -> Result<()> {
            self.stream = None;
            let device = cpal::default_host()
                .default_input_device()
                .ok_or_else(|| PlatformError::Unavailable("no microphone".into()))?;
            let config = device.default_input_config().map_err(|e| err("mic config", e))?;
            let (channels, rate) = (usize::from(config.channels()), config.sample_rate());
            let tx = self.tx.clone();
            let broken = Arc::clone(&self.broken);
            self.broken.store(false, Ordering::Relaxed);
            let mut framer = InputFramer::default();
            let stream = device
                .build_input_stream(
                    cpal::StreamConfig {
                        channels: config.channels(),
                        sample_rate: rate,
                        buffer_size: cpal::BufferSize::Default,
                    },
                    move |input: &[f32], _| {
                        framer.push(input, channels, rate, |f| {
                            let _ = tx.try_send(f.to_vec());
                        });
                    },
                    move |e| {
                        tracing::warn!("microphone error: {e}");
                        broken.store(true, Ordering::Relaxed);
                    },
                    None,
                )
                .map_err(|e| err("mic stream (on a Mac: allow Terminal under Privacy & Security > Microphone)", e))?;
            stream.play().map_err(|e| err("mic start", e))?;
            self.device = device_name(&device);
            tracing::info!(device = self.device, channels, rate, "microphone ready");
            self.stream = Some(stream);
            Ok(())
        }

        /// Next 10 ms frame, waiting up to `timeout`.
        pub fn next_frame(&mut self, timeout: Duration) -> Option<Vec<i16>> {
            if self.last_check.elapsed() >= Duration::from_secs(1) {
                self.last_check = Instant::now();
                let moved = cpal::default_host().default_input_device().is_some_and(|d| device_name(&d) != self.device);
                if moved || self.broken.load(Ordering::Relaxed) || self.stream.is_none() {
                    if let Err(e) = self.reopen() {
                        tracing::debug!("microphone not ready: {e}");
                    }
                }
            }
            self.frames.recv_timeout(timeout).ok()
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
    fn fec_rebuilds_a_lost_frame() {
        let mut enc = OpusEncoder::new(aa_core::audio::DEFAULT_BITRATE).unwrap();
        let mut dec = OpusDecoder::new().unwrap();
        let mut pcm = vec![0i16; FRAME_LEN_I16];
        let mut out = vec![0i16; FRAME_LEN_I16];
        // A few frames of tone so the encoder has something to protect.
        let mut last = Vec::new();
        for f in 0..5 {
            for (i, s) in pcm.iter_mut().enumerate() {
                let t = (f * FRAME_LEN_I16 + i) as f32 / 96_000.0;
                *s = ((t * 440.0 * std::f32::consts::TAU).sin() * 8000.0) as i16;
            }
            let packet = enc.encode(&pcm).unwrap().to_vec();
            if f < 3 {
                dec.decode(&packet, &mut out).unwrap();
            }
            last = packet;
        }
        // Frame 3 "lost": rebuild it from frame 4's packet.
        assert_eq!(dec.recover_previous(&last, &mut out).unwrap(), FRAME_SAMPLES);
    }

    #[test]
    fn concealment_produces_a_frame() {
        let mut dec = OpusDecoder::new().unwrap();
        let mut out = vec![0i16; FRAME_LEN_I16];
        assert_eq!(dec.conceal(&mut out).unwrap(), FRAME_SAMPLES);
    }
}
