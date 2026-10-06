//! The wire format: one small fixed header on every UDP datagram, and the
//! split/reassemble logic that turns an encoded frame into datagrams and back.
//!
//! Why our own format instead of RTP: RTP's header is 12 bytes too, but the
//! tooling around it (jitter buffers, RTCP) is designed for conferencing where
//! 100 ms of buffering is fine. We want zero buffering, so we own every byte.
//!
//! Every datagram:
//!
//! ```text
//!  0       1       2               4                       8          10        12
//!  +-------+-------+---------------+-----------------------+----------+----------+
//!  | kind  | flags | seq (u16)     | frame_id (u32)        | slice_ix | slice_n  |  payload...
//!  +-------+-------+---------------+-----------------------+----------+----------+
//! ```
//!
//! * `seq` increments per datagram regardless of kind, so loss is visible.
//! * `frame_id` / `slice_ix` / `slice_n` let the viewer rebuild a frame from
//!   its slices and know the instant it is complete. Non-video kinds set
//!   `slice_ix = 0, slice_n = 1`.
//!
//! Encryption is applied to the whole datagram *after* this layer (stage 3),
//! so nothing here is secret-aware.

use bytes::{Buf, BufMut, Bytes, BytesMut};
use std::collections::BTreeMap;

/// Safe datagram size across the public internet without fragmentation.
/// 1500 (Ethernet) − 20 (IPv4) − 8 (UDP) leaves 1472; we stay well under to
/// survive `PPPoE`, VPNs and IPv6-in-IPv4 tunnels.
pub const MAX_DATAGRAM: usize = 1200;
pub const HEADER_LEN: usize = 12;
pub const MAX_PAYLOAD: usize = MAX_DATAGRAM - HEADER_LEN;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Kind {
    /// One slice of an encoded video frame.
    Video = 1,
    /// One Opus packet.
    Audio = 2,
    /// One or more [`crate::input::InputEvent`]s back-to-back.
    Input = 3,
    /// Handshake, capability exchange, bitrate changes. Reliable (acked).
    Control = 4,
    /// Viewer → host: highest frame fully received; drives congestion control.
    Ack = 5,
    /// Viewer → host: a frame was lost, please intra-refresh.
    Nack = 6,
    /// Round-trip measurement.
    Ping = 7,
    Pong = 8,
}

impl Kind {
    fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            1 => Self::Video,
            2 => Self::Audio,
            3 => Self::Input,
            4 => Self::Control,
            5 => Self::Ack,
            6 => Self::Nack,
            7 => Self::Ping,
            8 => Self::Pong,
            _ => return None,
        })
    }
}

/// Header flag bits.
pub mod flags {
    /// This video frame is a keyframe (decodable on its own).
    pub const KEYFRAME: u8 = 1 << 0;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub kind: Kind,
    pub flags: u8,
    pub seq: u16,
    pub frame_id: u32,
    pub slice_index: u16,
    pub slice_count: u16,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum WireError {
    #[error("datagram shorter than header")]
    Truncated,
    #[error("unknown packet kind {0}")]
    UnknownKind(u8),
    #[error("slice index {index} out of range for {count} slices")]
    BadSlice { index: u16, count: u16 },
    #[error("payload of {0} bytes exceeds MAX_PAYLOAD")]
    PayloadTooLarge(usize),
}

impl Header {
    pub fn write(&self, out: &mut impl BufMut) {
        out.put_u8(self.kind as u8);
        out.put_u8(self.flags);
        out.put_u16(self.seq);
        out.put_u32(self.frame_id);
        out.put_u16(self.slice_index);
        out.put_u16(self.slice_count);
    }

    pub fn read(buf: &mut impl Buf) -> Result<Self, WireError> {
        if buf.remaining() < HEADER_LEN {
            return Err(WireError::Truncated);
        }
        let kind = Kind::from_u8(buf.get_u8()).ok_or(WireError::UnknownKind(0))?;
        let flags = buf.get_u8();
        let seq = buf.get_u16();
        let frame_id = buf.get_u32();
        let slice_index = buf.get_u16();
        let slice_count = buf.get_u16();
        if slice_count == 0 || slice_index >= slice_count {
            return Err(WireError::BadSlice { index: slice_index, count: slice_count });
        }
        Ok(Self { kind, flags, seq, frame_id, slice_index, slice_count })
    }
}

/// A parsed datagram: header plus a zero-copy view of its payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Packet {
    pub header: Header,
    pub payload: Bytes,
}

impl Packet {
    pub fn parse(mut datagram: Bytes) -> Result<Self, WireError> {
        let kind_byte = datagram.first().copied();
        let header = Header::read(&mut datagram).map_err(|e| match (e, kind_byte) {
            (WireError::UnknownKind(_), Some(k)) => WireError::UnknownKind(k),
            (e, _) => e,
        })?;
        Ok(Self { header, payload: datagram })
    }

    pub fn to_bytes(&self) -> Bytes {
        let mut out = BytesMut::with_capacity(HEADER_LEN + self.payload.len());
        self.header.write(&mut out);
        out.extend_from_slice(&self.payload);
        out.freeze()
    }
}

/// Hands out monotonically increasing datagram sequence numbers.
#[derive(Debug, Default)]
pub struct SeqCounter(std::sync::atomic::AtomicU16);

impl SeqCounter {
    /// Next sequence number. Atomic so one counter can be shared by every
    /// task that sends on a socket: the receiver's loss tracker assumes a
    /// single monotonic sequence per sender, and two independent counters
    /// interleaved on the wire read as near-total loss.
    pub fn take(&self) -> u16 {
        self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }
}

/// Split one encoded video frame into ready-to-send datagrams.
///
/// `frame` is the raw codec bitstream (Annex-B NAL units for H.264/HEVC, OBUs
/// for AV1). Slices are cut at byte boundaries; the decoder only sees the
/// reassembled whole, so it never needs to know.
pub fn slice_frame(frame: &Bytes, frame_id: u32, keyframe: bool, seq: &SeqCounter) -> Result<Vec<Bytes>, WireError> {
    let slice_count = frame.len().div_ceil(MAX_PAYLOAD).max(1);
    let slice_count_u16 = u16::try_from(slice_count).map_err(|_| WireError::PayloadTooLarge(frame.len()))?;
    let flags = if keyframe { flags::KEYFRAME } else { 0 };

    let mut out = Vec::with_capacity(slice_count);
    for (i, chunk) in frame.chunks(MAX_PAYLOAD).enumerate().take(slice_count) {
        let header = Header {
            kind: Kind::Video,
            flags,
            seq: seq.take(),
            frame_id,
            slice_index: i as u16,
            slice_count: slice_count_u16,
        };
        let mut buf = BytesMut::with_capacity(HEADER_LEN + chunk.len());
        header.write(&mut buf);
        buf.extend_from_slice(chunk);
        out.push(buf.freeze());
    }
    if out.is_empty() {
        // Zero-length frame: still send one header so the frame counter advances.
        let header = Header { kind: Kind::Video, flags, seq: seq.take(), frame_id, slice_index: 0, slice_count: 1 };
        let mut buf = BytesMut::with_capacity(HEADER_LEN);
        header.write(&mut buf);
        out.push(buf.freeze());
    }
    Ok(out)
}

/// A frame that has been fully received.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompleteFrame {
    pub frame_id: u32,
    pub keyframe: bool,
    pub data: Bytes,
}

/// What the reassembler concluded after one packet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reassembly {
    /// Need more slices.
    Pending,
    /// A frame just completed. Decode it now.
    Complete(CompleteFrame),
    /// Packet belonged to a frame already delivered or abandoned.
    Stale,
}

#[derive(Debug, Default)]
struct PartialFrame {
    keyframe: bool,
    slice_count: u16,
    received: u16,
    slices: Vec<Option<Bytes>>,
}

/// Rebuilds frames from slices, in arrival order, with no waiting.
///
/// Latency policy: the moment a frame completes it is handed out. If a newer
/// frame completes while an older one is still missing slices, the older one
/// is abandoned (and the caller should NACK so the host intra-refreshes).
/// We never hold a complete frame back to wait for an older incomplete one;
/// a 16 ms stall is worse than a brief artifact.
#[derive(Debug, Default)]
pub struct Reassembler {
    partial: BTreeMap<u32, PartialFrame>,
    /// Highest frame id delivered or abandoned; anything ≤ this is stale.
    last_delivered: Option<u32>,
    /// Frames abandoned since the last call to [`Self::take_abandoned`].
    abandoned: Vec<u32>,
}

impl Reassembler {
    /// How many incomplete frames we keep around before giving up on the oldest.
    const MAX_PARTIAL: usize = 4;

    pub fn push(&mut self, packet: Packet) -> Reassembly {
        let h = packet.header;
        debug_assert_eq!(h.kind, Kind::Video);

        if self.last_delivered.is_some_and(|last| h.frame_id <= last) {
            return Reassembly::Stale;
        }

        let pf = self.partial.entry(h.frame_id).or_insert_with(|| PartialFrame {
            keyframe: h.flags & flags::KEYFRAME != 0,
            slice_count: h.slice_count,
            received: 0,
            slices: vec![None; usize::from(h.slice_count)],
        });

        // A header that disagrees with earlier slices of the same frame is
        // corruption or an attack; drop the packet rather than the frame.
        if pf.slice_count != h.slice_count {
            return Reassembly::Stale;
        }
        let ix = usize::from(h.slice_index);
        if pf.slices[ix].is_none() {
            pf.slices[ix] = Some(packet.payload);
            pf.received += 1;
        }

        if pf.received == pf.slice_count {
            let pf = self.partial.remove(&h.frame_id).expect("just inserted");
            let total: usize = pf.slices.iter().flatten().map(Bytes::len).sum();
            let mut data = BytesMut::with_capacity(total);
            for s in pf.slices.into_iter().flatten() {
                data.extend_from_slice(&s);
            }
            // Everything older than this frame is now abandoned.
            let older: Vec<u32> = self.partial.range(..h.frame_id).map(|(id, _)| *id).collect();
            for id in older {
                self.partial.remove(&id);
                self.abandoned.push(id);
            }
            self.last_delivered = Some(h.frame_id);
            return Reassembly::Complete(CompleteFrame {
                frame_id: h.frame_id,
                keyframe: pf.keyframe,
                data: data.freeze(),
            });
        }

        // Bound memory: too many in flight means the oldest is never coming.
        while self.partial.len() > Self::MAX_PARTIAL {
            if let Some((&id, _)) = self.partial.iter().next() {
                self.partial.remove(&id);
                self.abandoned.push(id);
                self.last_delivered = Some(self.last_delivered.map_or(id, |l| l.max(id)));
            }
        }
        Reassembly::Pending
    }

    /// Frame ids given up on since the last call. The viewer sends these as NACKs.
    pub fn take_abandoned(&mut self) -> Vec<u32> {
        std::mem::take(&mut self.abandoned)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_round_trips_and_is_12_bytes() {
        let h = Header { kind: Kind::Audio, flags: 0, seq: 65535, frame_id: 42, slice_index: 0, slice_count: 1 };
        let mut buf = BytesMut::new();
        h.write(&mut buf);
        assert_eq!(buf.len(), HEADER_LEN);
        assert_eq!(Header::read(&mut buf.freeze()).unwrap(), h);
    }

    #[test]
    fn header_rejects_bad_input() {
        assert_eq!(Header::read(&mut &[0u8; 5][..]), Err(WireError::Truncated));
        let mut bad_slice = BytesMut::new();
        Header { kind: Kind::Video, flags: 0, seq: 0, frame_id: 0, slice_index: 3, slice_count: 3 }
            .write(&mut bad_slice);
        assert_eq!(Header::read(&mut bad_slice.freeze()), Err(WireError::BadSlice { index: 3, count: 3 }));
        let unknown = Bytes::from_static(&[77, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);
        assert_eq!(Packet::parse(unknown), Err(WireError::UnknownKind(77)));
    }

    fn frame_of(len: usize) -> Bytes {
        (0..len).map(|i| (i % 251) as u8).collect::<Vec<u8>>().into()
    }

    #[test]
    fn slices_never_exceed_mtu_and_reassemble_exactly() {
        let frame = frame_of(MAX_PAYLOAD * 3 + 17);
        let seq = SeqCounter::default();
        let slices = slice_frame(&frame, 7, true, &seq).unwrap();
        assert_eq!(slices.len(), 4);
        assert!(slices.iter().all(|s| s.len() <= MAX_DATAGRAM));

        let mut r = Reassembler::default();
        let mut result = None;
        for s in slices {
            match r.push(Packet::parse(s).unwrap()) {
                Reassembly::Complete(f) => result = Some(f),
                Reassembly::Pending => {}
                Reassembly::Stale => panic!("fresh frame marked stale"),
            }
        }
        let f = result.expect("frame should complete");
        assert_eq!(f.frame_id, 7);
        assert!(f.keyframe);
        assert_eq!(f.data, frame);
    }

    #[test]
    fn out_of_order_slices_still_complete() {
        let frame = frame_of(MAX_PAYLOAD * 2 + 1);
        let seq = SeqCounter::default();
        let mut slices = slice_frame(&frame, 1, false, &seq).unwrap();
        slices.reverse();
        let mut r = Reassembler::default();
        let last = slices.into_iter().map(|s| r.push(Packet::parse(s).unwrap())).last().unwrap();
        match last {
            Reassembly::Complete(f) => assert_eq!(f.data, frame),
            other => panic!("expected Complete, got {other:?}"),
        }
    }

    #[test]
    fn newer_complete_frame_abandons_older_incomplete_one() {
        let seq = SeqCounter::default();
        let old = slice_frame(&frame_of(MAX_PAYLOAD * 2), 10, false, &seq).unwrap();
        let new = slice_frame(&frame_of(10), 11, false, &seq).unwrap();

        let mut r = Reassembler::default();
        assert_eq!(r.push(Packet::parse(old[0].clone()).unwrap()), Reassembly::Pending);
        assert!(matches!(r.push(Packet::parse(new[0].clone()).unwrap()), Reassembly::Complete(_)));
        assert_eq!(r.take_abandoned(), vec![10]);
        // The late slice of frame 10 is now stale, not resurrected.
        assert_eq!(r.push(Packet::parse(old[1].clone()).unwrap()), Reassembly::Stale);
    }

    #[test]
    fn duplicate_slices_are_harmless() {
        let frame = frame_of(MAX_PAYLOAD + 5);
        let seq = SeqCounter::default();
        let slices = slice_frame(&frame, 3, false, &seq).unwrap();
        let mut r = Reassembler::default();
        assert_eq!(r.push(Packet::parse(slices[0].clone()).unwrap()), Reassembly::Pending);
        assert_eq!(r.push(Packet::parse(slices[0].clone()).unwrap()), Reassembly::Pending);
        assert!(matches!(r.push(Packet::parse(slices[1].clone()).unwrap()), Reassembly::Complete(_)));
    }

    #[test]
    fn empty_frame_still_produces_one_datagram() {
        let seq = SeqCounter::default();
        let s = slice_frame(&Bytes::new(), 0, false, &seq).unwrap();
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].len(), HEADER_LEN);
    }

    #[test]
    fn seq_counter_wraps() {
        let c = SeqCounter(std::sync::atomic::AtomicU16::new(u16::MAX));
        assert_eq!(c.take(), u16::MAX);
        assert_eq!(c.take(), 0);
    }
}
