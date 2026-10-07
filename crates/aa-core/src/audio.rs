//! Audio over the wire: Opus frames, 48 kHz stereo, 10 ms each.
//!
//! Why Opus: it is the codec every real-time system uses (WebRTC, Discord,
//! game streaming) because it sounds transparent at 128 kbps and encodes a
//! 10 ms frame in well under a millisecond. Why 10 ms frames: audio latency
//! is frame size plus network plus playout buffer, and 10 ms keeps the
//! whole chain near 30 ms, below the threshold where lips and sound part.
//!
//! Each `Kind::Audio` datagram carries one Opus packet with a tiny header:
//!
//! ```text
//!  0        4          6
//!  +--------+----------+
//!  | ts_ms  | frame_no |  opus payload...
//!  +--------+----------+
//! ```
//!
//! `frame_no` lets the player detect gaps and ask Opus for packet-loss
//! concealment instead of playing silence.

use bytes::{Buf, BufMut};

pub const SAMPLE_RATE: u32 = 48_000;
pub const CHANNELS: u16 = 2;
/// 10 ms at 48 kHz.
pub const FRAME_SAMPLES: usize = 480;
/// 256 kbps: Opus's "no audible difference even on good headphones" rate
/// for stereo music. Next to a 40 Mbps video stream it is a rounding error
/// (0.6%), so there is no reason to save bits here.
pub const DEFAULT_BITRATE: u32 = 256_000;
pub const HEADER_LEN: usize = 6;
/// Microphone stream (viewer → host): voice, so less than music needs,
/// but generous enough that game chat sounds natural.
pub const MIC_BITRATE: u32 = 96_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioHeader {
    /// Host monotonic time of the first sample, in ms (wraps; relative use only).
    pub ts_ms: u32,
    /// Per-stream counter; a gap means a lost packet.
    pub frame_no: u16,
}

impl AudioHeader {
    pub fn write(&self, out: &mut impl BufMut) {
        out.put_u32(self.ts_ms);
        out.put_u16(self.frame_no);
    }

    pub fn read(buf: &mut impl Buf) -> Option<Self> {
        if buf.remaining() < HEADER_LEN {
            return None;
        }
        Some(Self { ts_ms: buf.get_u32(), frame_no: buf.get_u16() })
    }
}

/// Tracks `frame_no` continuity for the player.
#[derive(Debug, Default, Clone, Copy)]
pub struct AudioSequence {
    next: Option<u16>,
}

impl AudioSequence {
    /// How many packets were lost just before this one (0 = contiguous),
    /// or `None` if this packet is late or a duplicate: it must be dropped,
    /// because its slot was already filled by concealment and playing it
    /// now would put old sound in the middle of new.
    ///
    /// A late packet never moves the expected number backwards; the first
    /// version did, so every packet after a reordered one counted as lost
    /// again and got concealed on top of the real audio.
    pub fn observe(&mut self, frame_no: u16) -> Option<u16> {
        let lost = match self.next {
            Some(n) => {
                let gap = frame_no.wrapping_sub(n);
                if gap >= u16::MAX / 2 {
                    return None;
                }
                gap
            }
            None => 0,
        };
        self.next = Some(frame_no.wrapping_add(1));
        Some(lost)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::BytesMut;

    #[test]
    fn header_round_trips() {
        let h = AudioHeader { ts_ms: 123_456, frame_no: 65_535 };
        let mut b = BytesMut::new();
        h.write(&mut b);
        assert_eq!(b.len(), HEADER_LEN);
        assert_eq!(AudioHeader::read(&mut b.freeze()), Some(h));
        assert_eq!(AudioHeader::read(&mut &[0u8; 2][..]), None);
    }

    #[test]
    fn sequence_counts_gaps_and_wraps() {
        let mut s = AudioSequence::default();
        assert_eq!(s.observe(0), Some(0));
        assert_eq!(s.observe(1), Some(0));
        assert_eq!(s.observe(4), Some(2));
        let mut w = AudioSequence::default();
        w.observe(u16::MAX);
        assert_eq!(w.observe(0), Some(0));
        assert_eq!(w.observe(1), Some(0));
        // Late packet: dropped, and the ones after it are not "lost".
        let mut r = AudioSequence::default();
        r.observe(10);
        r.observe(12);
        assert_eq!(r.observe(11), None);
        assert_eq!(r.observe(13), Some(0));
    }
}
