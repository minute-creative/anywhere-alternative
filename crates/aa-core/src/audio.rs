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
pub const DEFAULT_BITRATE: u32 = 128_000;
pub const HEADER_LEN: usize = 6;

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
    /// Returns how many packets were lost before this one (0 = contiguous).
    /// Reordered or duplicate packets return 0 and are left to the caller.
    pub fn observe(&mut self, frame_no: u16) -> u16 {
        let lost = match self.next {
            Some(n) => {
                let gap = frame_no.wrapping_sub(n);
                if gap < u16::MAX / 2 {
                    gap
                } else {
                    0
                }
            }
            None => 0,
        };
        self.next = Some(frame_no.wrapping_add(1));
        lost
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
        assert_eq!(s.observe(0), 0);
        assert_eq!(s.observe(1), 0);
        assert_eq!(s.observe(4), 2);
        let mut w = AudioSequence::default();
        w.observe(u16::MAX);
        assert_eq!(w.observe(0), 0);
        assert_eq!(w.observe(1), 0);
        // Reordered: no loss counted.
        let mut r = AudioSequence::default();
        r.observe(10);
        assert_eq!(r.observe(9), 0);
    }
}
