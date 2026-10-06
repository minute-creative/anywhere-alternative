//! Viewer-side audio: Opus packets in, speaker out.
//!
//! Decoding a 10 ms Opus frame takes ~50 µs, so we do it right on the
//! network task rather than adding a thread and a queue (each of which
//! would add latency). Lost packets are concealed by the decoder instead of
//! played as silence, which is what makes Wi-Fi loss inaudible.

use aa_core::audio::{AudioHeader, AudioSequence};
use aa_platform::audio::{OpusDecoder, FRAME_LEN_I16};
use bytes::Bytes;

/// Max consecutive lost frames we conceal; beyond this it was a real gap
/// (host went silent or we were disconnected) and we just resync.
const MAX_CONCEAL: u16 = 5;

pub struct AudioSink {
    decoder: OpusDecoder,
    seq: AudioSequence,
    pcm: Vec<i16>,
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    player: Option<aa_platform::audio::Player>,
    pub frames: u64,
    pub concealed: u64,
}

impl std::fmt::Debug for AudioSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AudioSink")
            .field("frames", &self.frames)
            .field("concealed", &self.concealed)
            .finish_non_exhaustive()
    }
}

impl AudioSink {
    /// Never fails: without an output device we still count packets so
    /// stats stay honest, we just don't play.
    pub fn new() -> Self {
        let decoder = OpusDecoder::new().expect("opus decoder");
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        let player = match aa_platform::audio::Player::new() {
            Ok(p) => Some(p),
            Err(e) => {
                tracing::warn!("audio playback unavailable: {e}");
                None
            }
        };
        Self {
            decoder,
            seq: AudioSequence::default(),
            pcm: vec![0i16; FRAME_LEN_I16],
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            player,
            frames: 0,
            concealed: 0,
        }
    }

    /// One `Kind::Audio` datagram payload.
    pub fn handle(&mut self, mut payload: Bytes) {
        let Some(header) = AudioHeader::read(&mut payload) else {
            tracing::debug!("short audio packet");
            return;
        };
        let lost = self.seq.observe(header.frame_no);
        if lost > 0 && lost <= MAX_CONCEAL {
            for _ in 0..lost {
                if self.decoder.conceal(&mut self.pcm).is_ok() {
                    self.concealed += 1;
                    self.play();
                }
            }
        }
        match self.decoder.decode(&payload, &mut self.pcm) {
            Ok(_) => {
                self.frames += 1;
                self.play();
            }
            Err(e) => tracing::debug!("audio decode: {e}"),
        }
    }

    #[allow(clippy::unused_self)] // no-op on Linux, where there is no player
    fn play(&self) {
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        if let Some(p) = &self.player {
            p.push(&self.pcm);
        }
    }
}

impl Default for AudioSink {
    fn default() -> Self {
        Self::new()
    }
}
