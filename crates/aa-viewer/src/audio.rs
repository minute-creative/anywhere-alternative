//! Viewer-side audio: the PC's sound in, speaker out; and the Mac's
//! microphone out to the PC.
//!
//! Decoding a 10 ms Opus frame takes ~50 µs, so we do it right on the
//! network task rather than adding a thread and a queue (each of which
//! would add latency). Lost packets are concealed or rebuilt by the shared
//! `Depacketizer` instead of played as silence.

use aa_platform::audio::Depacketizer;
use bytes::Bytes;

pub struct AudioSink {
    depack: Depacketizer,
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    player: Option<aa_platform::audio::Player>,
}

impl std::fmt::Debug for AudioSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AudioSink").field("depack", &self.depack).finish_non_exhaustive()
    }
}

impl AudioSink {
    /// Never fails: without an output device we still count packets so
    /// stats stay honest, we just don't play.
    pub fn new() -> Self {
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        let player = match aa_platform::audio::Player::new() {
            Ok(p) => Some(p),
            Err(e) => {
                tracing::warn!("audio playback unavailable: {e}");
                None
            }
        };
        Self {
            depack: Depacketizer::new().expect("opus decoder"),
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            player,
        }
    }

    pub fn frames(&self) -> u64 {
        self.depack.frames
    }

    pub fn concealed(&self) -> u64 {
        self.depack.concealed
    }

    /// One `Kind::Audio` datagram payload.
    pub fn handle(&mut self, payload: Bytes) {
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        if let Some(p) = self.player.as_mut() {
            // Headphones connected or unplugged: move the sound there.
            p.follow();
            self.depack.handle(payload, |pcm| p.push(pcm));
            return;
        }
        self.depack.handle(payload, |_| {});
    }

    /// Current jitter-buffer depth in ms (0 without a player).
    #[allow(clippy::unused_self)]
    pub fn buffer_ms(&self) -> u32 {
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        if let Some(p) = &self.player {
            return p.stats().0;
        }
        0
    }

    /// Times playback ran dry (0 without a player).
    #[allow(clippy::unused_self)]
    pub fn underruns(&self) -> u64 {
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        if let Some(p) = &self.player {
            return p.stats().1;
        }
        0
    }
}

impl Default for AudioSink {
    fn default() -> Self {
        Self::new()
    }
}

/// Sends this machine's microphone to the host while switched on.
///
/// The microphone is opened on its own thread (the audio stream objects
/// are not allowed to move between threads on every OS) and each 10 ms
/// frame is Opus-encoded there. Encoded packets come back to the network
/// task over a channel. Dropping the handle stops the thread and closes the
/// microphone, so the Mac's mic indicator goes off.
#[derive(Debug)]
pub struct MicSender {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl MicSender {
    /// Start capturing; packets (audio header + Opus) arrive on the returned receiver.
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    pub fn start() -> (Self, tokio::sync::mpsc::Receiver<Bytes>) {
        use std::sync::atomic::Ordering;
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (tx, rx) = tokio::sync::mpsc::channel(32);
        let flag = std::sync::Arc::clone(&stop);
        let _ = std::thread::Builder::new().name("aa-mic".into()).spawn(move || {
            use aa_core::audio::{AudioHeader, HEADER_LEN, MIC_BITRATE};
            use bytes::BufMut;
            let mut mic = match aa_platform::audio::Mic::open() {
                Ok(m) => m,
                Err(e) => {
                    tracing::warn!("microphone unavailable: {e}");
                    return;
                }
            };
            let mut enc = match aa_platform::audio::OpusEncoder::new(MIC_BITRATE) {
                Ok(e) => e,
                Err(e) => {
                    tracing::warn!("mic encoder: {e}");
                    return;
                }
            };
            let started = std::time::Instant::now();
            let mut frame_no: u16 = 0;
            while !flag.load(Ordering::Relaxed) {
                let Some(pcm) = mic.next_frame(std::time::Duration::from_millis(100)) else { continue };
                let Ok(payload) = enc.encode(&pcm) else { continue };
                let mut buf = bytes::BytesMut::with_capacity(HEADER_LEN + payload.len());
                #[allow(clippy::cast_possible_truncation)] // wraps by design
                AudioHeader { ts_ms: started.elapsed().as_millis() as u32, frame_no }.write(&mut buf);
                buf.put_slice(payload);
                frame_no = frame_no.wrapping_add(1);
                if tx.blocking_send(buf.freeze()).is_err() {
                    break;
                }
            }
            tracing::info!("microphone off");
        });
        (Self { stop }, rx)
    }

    /// No microphone support on this OS.
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    pub fn start() -> (Self, tokio::sync::mpsc::Receiver<Bytes>) {
        let (_tx, rx) = tokio::sync::mpsc::channel(1);
        (Self { stop: std::sync::Arc::default() }, rx)
    }
}

impl MicSender {
    /// `--test-mic`: a steady 440 Hz tone instead of a real microphone, so a
    /// mock session exercises the whole mic path on any OS.
    pub fn start_tone() -> (Self, tokio::sync::mpsc::Receiver<Bytes>) {
        use std::sync::atomic::Ordering;
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (tx, rx) = tokio::sync::mpsc::channel(32);
        let flag = std::sync::Arc::clone(&stop);
        let _ = std::thread::Builder::new().name("aa-test-mic".into()).spawn(move || {
            use aa_core::audio::{AudioHeader, FRAME_SAMPLES, HEADER_LEN, MIC_BITRATE};
            use bytes::BufMut;
            let Ok(mut enc) = aa_platform::audio::OpusEncoder::new(MIC_BITRATE) else { return };
            let mut frame_no: u16 = 0;
            let mut t = 0f32;
            let mut next = std::time::Instant::now();
            while !flag.load(Ordering::Relaxed) {
                let mut pcm = Vec::with_capacity(FRAME_SAMPLES * 2);
                for _ in 0..FRAME_SAMPLES {
                    #[allow(clippy::cast_possible_truncation)]
                    let v = ((t * 440.0 * std::f32::consts::TAU).sin() * 6000.0) as i16;
                    pcm.extend_from_slice(&[v, v]);
                    t += 1.0 / 48_000.0;
                }
                let Ok(payload) = enc.encode(&pcm) else { continue };
                let mut buf = bytes::BytesMut::with_capacity(HEADER_LEN + payload.len());
                AudioHeader { ts_ms: 0, frame_no }.write(&mut buf);
                buf.put_slice(payload);
                frame_no = frame_no.wrapping_add(1);
                if tx.blocking_send(buf.freeze()).is_err() {
                    break;
                }
                next += std::time::Duration::from_millis(10);
                std::thread::sleep(next.saturating_duration_since(std::time::Instant::now()));
            }
        });
        (Self { stop }, rx)
    }
}

impl Drop for MicSender {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}
