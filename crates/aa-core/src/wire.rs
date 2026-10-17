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
// In plain words: a frame (one picture) is usually far bigger than one network
// packet, so it is cut into numbered pieces ("slices"). Each piece carries a
// 12-byte label saying what it is, which frame it belongs to and which piece
// it is. The viewer uses those labels to put the picture back together, notice
// what went missing, and ask for it again. Extra "parity" pieces let it
// rebuild one missing piece per group without asking at all.

use bytes::{Buf, BufMut, Bytes, BytesMut};
use std::collections::BTreeMap;

/// Safe datagram size across the public internet without fragmentation.
/// 1500 (Ethernet) − 20 (IPv4) − 8 (UDP) leaves 1472; we stay well under to
/// survive `PPPoE`, VPNs and IPv6-in-IPv4 tunnels.
// Largest packet we send (header + data), in bytes.
pub const MAX_DATAGRAM: usize = 1200;
// Size of the label at the front of every packet (see the drawing above).
pub const HEADER_LEN: usize = 12;
// How much picture data fits in one packet after the label.
pub const MAX_PAYLOAD: usize = MAX_DATAGRAM - HEADER_LEN;

// What a packet carries. The number is the first byte of every packet, so
// these numbers must never change or old and new versions stop understanding
// each other. Add new kinds at the end with the next free number.
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
    /// Parity for a video frame (forward error correction); see
    /// [`slice_frame_with_fec`]. `slice_index` is the stripe, `slice_count`
    /// the number of parity packets for this frame.
    VideoFec = 9,
    /// One piece of a clipboard item, either direction; see `clipboard`.
    /// `frame_id` is the transfer id.
    Clipboard = 10,
    /// "Got every piece of clipboard transfer `frame_id`."
    ClipboardAck = 11,
    /// Viewer → host: one Opus packet of the viewer's microphone (same
    /// layout as `Audio`).
    Mic = 12,
    /// `DualSense` raw pass-through, both directions; see `ds5`.
    Pad = 13,
    /// Viewer → host: "send these packets of frame `frame_id` again"; see
    /// [`ResendRequest`]. The host keeps the last second of video packets
    /// for exactly this. On a 5 ms link a resend lands long before the
    /// frame would otherwise be given up on, so a lost packet costs a few
    /// milliseconds instead of a keyframe.
    Resend = 14,
    /// Viewer → host: one slice of an encoded camera frame (H.264), for
    /// "Anywhere Camera" on the host; same slicing as `Video`, no parity.
    Camera = 15,
}

// Turn the first byte of a packet back into a Kind; unknown numbers (a newer
// version, or garbage) give None and the packet is ignored.
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
            9 => Self::VideoFec,
            10 => Self::Clipboard,
            11 => Self::ClipboardAck,
            12 => Self::Mic,
            13 => Self::Pad,
            14 => Self::Resend,
            15 => Self::Camera,
            _ => return None,
        })
    }
}

/// Header flag bits.
pub mod flags {
    /// This video frame is a keyframe (decodable on its own).
    pub const KEYFRAME: u8 = 1 << 0;
    /// This video frame repairs a loss without being a whole new picture:
    /// it only refers to a frame the viewer confirmed it has (a Mac
    /// "long-term reference"). A viewer waiting after a loss may resume here.
    pub const RECOVERY: u8 = 1 << 1;
}

// The 12-byte label, in the order it is written on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub kind: Kind,
    pub flags: u8,
    pub seq: u16,
    pub frame_id: u32,
    pub slice_index: u16,
    pub slice_count: u16,
}

// Ways a received packet can be unreadable.
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

// Writing and reading the label. Numbers are written "big-endian" (most
// significant byte first), the usual network order, on both machines.
impl Header {
    pub fn write(&self, out: &mut impl BufMut) {
        out.put_u8(self.kind as u8);
        out.put_u8(self.flags);
        out.put_u16(self.seq);
        out.put_u32(self.frame_id);
        out.put_u16(self.slice_index);
        out.put_u16(self.slice_count);
    }

    // Read a label from the front of a packet; refuses anything too short or
    // with a piece number that cannot exist.
    pub fn read(buf: &mut impl Buf) -> Result<Self, WireError> {
        if buf.remaining() < HEADER_LEN {
            return Err(WireError::Truncated);
        }
        // The real kind byte is filled in by Packet::parse, which still has it.
        let kind = Kind::from_u8(buf.get_u8()).ok_or(WireError::UnknownKind(0))?;
        let flags = buf.get_u8();
        let seq = buf.get_u16();
        let frame_id = buf.get_u32();
        let slice_index = buf.get_u16();
        let slice_count = buf.get_u16();
        // A frame always has at least one piece, and piece numbers count from 0.
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

// Split a received packet into its label and its data (no copying).
impl Packet {
    pub fn parse(mut datagram: Bytes) -> Result<Self, WireError> {
        // Remember the first byte so an "unknown kind" error can report it.
        let kind_byte = datagram.first().copied();
        let header = Header::read(&mut datagram).map_err(|e| match (e, kind_byte) {
            (WireError::UnknownKind(_), Some(k)) => WireError::UnknownKind(k),
            (e, _) => e,
        })?;
        Ok(Self { header, payload: datagram })
    }

    // Glue label and data back into one packet ready to send.
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
// Shortcut: a plain video frame, keyframe or not.
pub fn slice_frame(frame: &Bytes, frame_id: u32, keyframe: bool, seq: &SeqCounter) -> Result<Vec<Bytes>, WireError> {
    slice_frame_flagged(frame, frame_id, if keyframe { flags::KEYFRAME } else { 0 }, seq)
}

/// [`slice_frame`] with any header flags (keyframe, recovery).
// Shortcut: a video frame with any flags.
pub fn slice_frame_flagged(frame: &Bytes, frame_id: u32, flags: u8, seq: &SeqCounter) -> Result<Vec<Bytes>, WireError> {
    slice_frame_as(Kind::Video, frame, frame_id, flags, seq)
}

/// Slice a frame into packets of any frame-carrying kind (`Video`, `Camera`).
pub fn slice_frame_as(
    kind: Kind,
    frame: &Bytes,
    frame_id: u32,
    flags: u8,
    seq: &SeqCounter,
) -> Result<Vec<Bytes>, WireError> {
    // Number of pieces = frame size divided by piece size, rounded up, at
    // least one. More than 65,535 pieces would not fit the label: refuse.
    let slice_count = frame.len().div_ceil(MAX_PAYLOAD).max(1);
    let slice_count_u16 = u16::try_from(slice_count).map_err(|_| WireError::PayloadTooLarge(frame.len()))?;

    let mut out = Vec::with_capacity(slice_count);
    // One packet per chunk: label first, then that chunk of the frame.
    for (i, chunk) in frame.chunks(MAX_PAYLOAD).enumerate().take(slice_count) {
        let header =
            Header { kind, flags, seq: seq.take(), frame_id, slice_index: i as u16, slice_count: slice_count_u16 };
        let mut buf = BytesMut::with_capacity(HEADER_LEN + chunk.len());
        header.write(&mut buf);
        buf.extend_from_slice(chunk);
        out.push(buf.freeze());
    }
    if out.is_empty() {
        // Zero-length frame: still send one header so the frame counter advances.
        let header = Header { kind, flags, seq: seq.take(), frame_id, slice_index: 0, slice_count: 1 };
        let mut buf = BytesMut::with_capacity(HEADER_LEN);
        header.write(&mut buf);
        out.push(buf.freeze());
    }
    Ok(out)
}

/// Parity packets per data packet. 10%: at 0.5-1% random Wi-Fi loss a
/// frame of a few dozen slices almost never loses two in the same stripe,
/// and the cost (8 Mbps at an 80 Mbps stream) is far cheaper than a frozen
/// frame plus a recovery keyframe.
// Forward error correction (FEC): extra pieces made by XOR-ing data
// pieces together. If exactly one piece of a group is lost, XOR-ing the
// others with the parity piece gives the lost one back, with no round trip.
pub const FEC_RATIO: f64 = 0.10;
/// The most redundancy we will add: one parity packet per two data
/// packets. Past this the parity costs more airtime than it saves.
pub const FEC_RATIO_MAX: f64 = 0.50;

/// How many parity packets a frame of `data_slices` gets at the default
/// ratio: 10%, at least 1.
pub fn parity_count(data_slices: usize) -> usize {
    parity_count_at(data_slices, FEC_RATIO)
}

/// Parity packets for `data_slices` at `ratio` (clamped to 0.10..=0.50).
// Parity count = data pieces x ratio, rounded up, between 1 and the number
// of data pieces. The ratio is held between 10% and 50%.
pub fn parity_count_at(data_slices: usize, ratio: f64) -> usize {
    let ratio = ratio.clamp(FEC_RATIO, FEC_RATIO_MAX);
    ((data_slices as f64 * ratio).ceil() as usize).clamp(1, data_slices.max(1))
}

/// XOR parity for one stripe: data slices `k, k+G, k+2G, ...`.
///
/// Why interleaved stripes rather than consecutive groups: Wi-Fi tends to
/// lose packets in short bursts. With stripes, a burst of up to G
/// consecutive packets hits G *different* stripes, each of which can
/// rebuild its one missing slice.
///
/// Payload layout: `[u16 xor of slice lengths][xor of zero-padded data]`.
/// The length lets us rebuild a short last slice to its exact size.
fn stripe_parity(slices: &[&[u8]], stripe: usize, stripes: usize) -> BytesMut {
    // Members of this stripe: every "stripes"-th piece starting at "stripe".
    let members = slices.iter().skip(stripe).step_by(stripes);
    // Parity is as long as the longest member; shorter ones count as zero-padded.
    let width = members.clone().map(|s| s.len()).max().unwrap_or(0);
    let mut out = BytesMut::zeroed(2 + width);
    let mut len_xor = 0u16;
    // XOR every member in. Also XOR their lengths, so the lost piece's exact
    // length can be recovered too.
    for m in members {
        len_xor ^= m.len() as u16;
        for (o, b) in out[2..].iter_mut().zip(m.iter()) {
            *o ^= *b;
        }
    }
    out[..2].copy_from_slice(&len_xor.to_be_bytes());
    out
}

/// [`slice_frame`] plus FEC parity packets appended after the data.
pub fn slice_frame_with_fec(
    frame: &Bytes,
    frame_id: u32,
    keyframe: bool,
    seq: &SeqCounter,
) -> Result<Vec<Bytes>, WireError> {
    slice_frame_with_fec_at(frame, frame_id, keyframe, seq, FEC_RATIO)
}

/// Like [`slice_frame_with_fec`] with the parity ratio chosen by the
/// bitrate controller from the loss the viewer reports: a radio dropping
/// one packet in five needs far more parity than a clean cable.
pub fn slice_frame_with_fec_at(
    frame: &Bytes,
    frame_id: u32,
    keyframe: bool,
    seq: &SeqCounter,
    ratio: f64,
) -> Result<Vec<Bytes>, WireError> {
    slice_frame_with_fec_flagged(frame, frame_id, if keyframe { flags::KEYFRAME } else { 0 }, seq, ratio)
}

/// [`slice_frame_with_fec_at`] with any header flags (keyframe, repair).
pub fn slice_frame_with_fec_flagged(
    frame: &Bytes,
    frame_id: u32,
    flags: u8,
    seq: &SeqCounter,
    ratio: f64,
) -> Result<Vec<Bytes>, WireError> {
    // First the normal data pieces, then one parity piece per stripe after them.
    let mut out = slice_frame_flagged(frame, frame_id, flags, seq)?;
    let data: Vec<&[u8]> = out.iter().map(|d| &d[HEADER_LEN..]).collect();
    let stripes = parity_count_at(data.len(), ratio);
    let mut parity = Vec::with_capacity(stripes);
    for k in 0..stripes {
        let payload = stripe_parity(&data, k, stripes);
        let header = Header {
            kind: Kind::VideoFec,
            flags,
            seq: seq.take(),
            frame_id,
            slice_index: k as u16,
            slice_count: stripes as u16,
        };
        let mut buf = BytesMut::with_capacity(HEADER_LEN + payload.len());
        header.write(&mut buf);
        buf.extend_from_slice(&payload);
        parity.push(buf.freeze());
    }
    out.extend(parity);
    Ok(out)
}

/// Is frame `a` newer than frame `b`? Frame ids wrap after 2^32 (166 days
/// at 300 fps); comparing by distance, like TCP sequence numbers, keeps
/// "newer" meaningful across the wrap. A plain `a > b` would make every
/// frame after the wrap look ancient and be thrown away.
// "a - b" wraps around like a clock; less than half way round means newer.
pub fn is_newer(a: u32, b: u32) -> bool {
    a != b && a.wrapping_sub(b) < u32::MAX / 2
}

/// Which packets of one frame the viewer still needs. Data slices are
/// listed by index; parity packets with the high bit set (`0x8000 | k`).
// The viewer's "please send these pieces of frame X again".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResendRequest {
    pub frame_id: u32,
    pub items: Vec<u16>,
}

impl ResendRequest {
    pub const PARITY_BIT: u16 = 0x8000;
    /// "Every data slice": used when only parity arrived, so the viewer
    /// does not even know how many slices the frame has.
    pub const ALL_DATA: u16 = 0x7FFF;
    /// At most this many items per request (one datagram).
    pub const MAX_ITEMS: usize = 500;

    /// Payload only; the header carries kind and `frame_id` too, but the
    /// id is repeated so the payload stands alone.
    // Wire layout: 4-byte frame number, then 2 bytes per wanted piece.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(4 + self.items.len() * 2);
        out.extend_from_slice(&self.frame_id.to_be_bytes());
        for i in self.items.iter().take(Self::MAX_ITEMS) {
            out.extend_from_slice(&i.to_be_bytes());
        }
        out
    }

    // Read a request; anything with a broken length is ignored (None).
    pub fn decode(payload: &[u8]) -> Option<Self> {
        if payload.len() < 4 || (payload.len() - 4) % 2 != 0 {
            return None;
        }
        let frame_id = u32::from_be_bytes(payload[..4].try_into().ok()?);
        let items = payload[4..].chunks_exact(2).map(|c| u16::from_be_bytes([c[0], c[1]])).collect();
        Some(Self { frame_id, items })
    }
}

// A whole picture, ready for the decoder.
/// A frame that has been fully received.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompleteFrame {
    pub frame_id: u32,
    pub keyframe: bool,
    /// See [`flags::RECOVERY`].
    pub recovery: bool,
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

// A frame still being collected: which pieces arrived, which parity
// pieces arrived, and when we last heard of it or asked about it.
#[derive(Debug, Default)]
struct PartialFrame {
    keyframe: bool,
    recovery: bool,
    /// Data slices in the frame; 0 until the first data slice arrives
    /// (a parity packet can get here first).
    slice_count: u16,
    received: u16,
    slices: Vec<Option<Bytes>>,
    /// FEC parity by stripe; empty until the first parity packet.
    parity: Vec<Option<Bytes>>,
    /// When the last packet of this frame arrived (ms, caller's clock).
    last_packet_ms: u64,
    /// When a resend was last asked for, if ever.
    requested_ms: Option<u64>,
    /// How many times a resend was asked for.
    requests: u8,
}

impl PartialFrame {
    /// Data slices still missing (after FEC).
    fn missing_slices(&self) -> Vec<u16> {
        self.slices.iter().enumerate().filter(|(_, s)| s.is_none()).map(|(i, _)| i as u16).collect()
    }

    /// Rebuild every stripe that is missing exactly one data slice and has
    /// its parity. Returns how many slices were recovered.
    fn recover(&mut self) -> u16 {
        // Try each stripe that has its parity piece.
        let stripes = self.parity.len();
        if stripes == 0 || self.slice_count == 0 {
            return 0;
        }
        let mut recovered = 0;
        for k in 0..stripes {
            let Some(parity) = self.parity[k].clone() else { continue };
            // The data pieces of this stripe, and which of them are missing.
            let members: Vec<usize> = (k..self.slices.len()).step_by(stripes).collect();
            let missing: Vec<usize> = members.iter().copied().filter(|&i| self.slices[i].is_none()).collect();
            // Parity can rebuild only one missing piece per stripe.
            if missing.len() != 1 || parity.len() < 2 {
                continue;
            }
            // Start from the parity, XOR in every piece we have: what is left is the
            // missing piece (and its length).
            let mut len = u16::from_be_bytes([parity[0], parity[1]]);
            let mut buf = parity[2..].to_vec();
            for &i in &members {
                if let Some(d) = &self.slices[i] {
                    len ^= d.len() as u16;
                    for (o, b) in buf.iter_mut().zip(d.iter()) {
                        *o ^= *b;
                    }
                }
            }
            // A length longer than the parity means the pieces disagree: give up on
            // this stripe rather than make up data.
            let len = usize::from(len);
            if len > buf.len() {
                continue; // inconsistent parity; leave the frame to the NACK path
            }
            buf.truncate(len);
            self.slices[missing[0]] = Some(Bytes::from(buf));
            self.received += 1;
            recovered += 1;
        }
        recovered
    }
}

/// Rebuilds frames from slices, in arrival order, with no waiting.
///
/// Latency policy: the moment a frame completes it is handed out. If a newer
/// frame completes while an older one is still missing slices, the older one
/// is abandoned (and the caller should NACK so the host intra-refreshes).
/// We never hold a complete frame back to wait for an older incomplete one;
/// a 16 ms stall is worse than a brief artifact.
#[derive(Debug)]
pub struct Reassembler {
    partial: BTreeMap<u32, PartialFrame>,
    /// Complete frames waiting behind an older frame that is still being
    /// resent. A P-frame cannot be decoded without the one before it, so
    /// handing it over early would only break the picture.
    held: BTreeMap<u32, CompleteFrame>,
    /// Highest frame id delivered or abandoned; anything ≤ this is stale.
    last_delivered: Option<u32>,
    /// Frames abandoned since the last call to [`Self::take_abandoned`].
    abandoned: Vec<u32>,
    /// Slices rebuilt from FEC parity since the stream started.
    pub recovered: u64,
    /// Packets that arrived again after a resend request.
    pub resent: u64,
    /// How long an incomplete frame may wait for resends before it is given
    /// up on, in ms. Set from the measured RTT by [`Self::set_rtt_ms`].
    grace_ms: u64,
    /// Current RTT estimate in ms (for spacing repeat requests).
    rtt_ms: u64,
}

impl Default for Reassembler {
    fn default() -> Self {
        Self {
            // Start empty, with a cautious 5 ms round-trip guess until measured.
            partial: BTreeMap::new(),
            held: BTreeMap::new(),
            last_delivered: None,
            abandoned: Vec::new(),
            recovered: 0,
            resent: 0,
            grace_ms: Self::GRACE_MIN_MS,
            rtt_ms: 5,
        }
    }
}

impl Reassembler {
    /// How many incomplete frames we keep around before giving up on the oldest.
    const MAX_PARTIAL: usize = 8;
    /// Grace for resends: 2.5 × RTT, within these bounds. The floor covers
    /// a LAN where RTT is under a millisecond but the host still needs a
    /// moment to answer; the ceiling keeps a bad link from stalling the
    /// picture for long — past it a keyframe is the better repair.
    const GRACE_MIN_MS: u64 = 8;
    const GRACE_MAX_MS: u64 = 40;
    /// A frame whose packets stopped arriving this long ago has a gap
    /// worth asking about (the host sends a frame as one burst).
    const QUIET_MS: u64 = 2;
    /// …but never wait longer than this for a stalled frame's tail.
    const QUIET_MAX_MS: u64 = 15;
    /// Give up asking after this many tries.
    const MAX_REQUESTS: u8 = 3;

    /// Tell the reassembler the current round trip so it knows how long a
    /// resend takes.
    pub fn set_rtt_ms(&mut self, rtt_ms: f64) {
        // Grace = 2.5 x round trip: long enough for "ask" plus "resend" to
        // finish, kept between 8 and 40 ms.
        let rtt = rtt_ms.max(0.0) as u64;
        self.rtt_ms = rtt.max(1);
        self.grace_ms = (rtt * 5 / 2).clamp(Self::GRACE_MIN_MS, Self::GRACE_MAX_MS);
    }

    /// Feed one video or parity packet. `now_ms` is any monotonic clock
    /// in milliseconds; it is only compared with itself.
    pub fn push(&mut self, packet: Packet, now_ms: u64) -> Reassembly {
        let h = packet.header;
        debug_assert!(matches!(h.kind, Kind::Video | Kind::VideoFec | Kind::Camera));

        // Pieces of a frame we already showed or gave up on: nothing to do.
        if self.last_delivered.is_some_and(|last| !is_newer(h.frame_id, last)) {
            return Reassembly::Stale;
        }
        // Already complete and waiting its turn: this is a duplicate.
        if self.held.contains_key(&h.frame_id) {
            return Reassembly::Stale; // a resend of something already complete
        }

        // Find (or start) the record for this frame.
        let pf = self.partial.entry(h.frame_id).or_insert_with(|| PartialFrame {
            keyframe: h.flags & flags::KEYFRAME != 0,
            recovery: h.flags & flags::RECOVERY != 0,
            ..PartialFrame::default()
        });
        // Count pieces that arrive after we asked for them (shown as "resent").
        if pf.requested_ms.is_some() {
            self.resent += 1;
        }
        pf.last_packet_ms = now_ms;

        // A parity piece: store it under its stripe number.
        if h.kind == Kind::VideoFec {
            if pf.parity.is_empty() {
                pf.parity = vec![None; usize::from(h.slice_count)];
            }
            // Disagreeing headers within one frame: corruption; drop the packet.
            if pf.parity.len() != usize::from(h.slice_count) {
                return Reassembly::Stale;
            }
            pf.parity[usize::from(h.slice_index)] = Some(packet.payload);
        // A data piece: store it once (duplicates are ignored).
        } else {
            if pf.slice_count == 0 {
                pf.slice_count = h.slice_count;
                pf.slices = vec![None; usize::from(h.slice_count)];
            }
            if pf.slice_count != h.slice_count {
                return Reassembly::Stale;
            }
            let ix = usize::from(h.slice_index);
            if pf.slices[ix].is_none() {
                pf.slices[ix] = Some(packet.payload);
                pf.received += 1;
            }
        }
        // Still incomplete: see whether parity can fill a gap.
        if pf.slice_count > 0 && pf.received < pf.slice_count {
            self.recovered += u64::from(pf.recover());
        }

        // Every piece is here: join them in order into one frame.
        if pf.slice_count > 0 && pf.received == pf.slice_count {
            let pf = self.partial.remove(&h.frame_id).expect("just inserted");
            let total: usize = pf.slices.iter().flatten().map(Bytes::len).sum();
            let mut data = BytesMut::with_capacity(total);
            for s in pf.slices.into_iter().flatten() {
                data.extend_from_slice(&s);
            }
            let frame = CompleteFrame {
                frame_id: h.frame_id,
                keyframe: pf.keyframe,
                recovery: pf.recovery,
                data: data.freeze(),
            };
            // A keyframe needs nothing before it, nor does a repair frame
            // (built only on a frame the viewer has): whatever is older is moot.
            if frame.keyframe || frame.recovery {
                self.abandon_older_than(h.frame_id);
            }
            // Park it, then hand out frames strictly in order (a P-frame is useless
            // before the frame it builds on).
            self.held.insert(h.frame_id, frame);
            return self.deliver_next(now_ms).map_or(Reassembly::Pending, Reassembly::Complete);
        }

        // Bound memory: too many in flight means the oldest is never coming.
        while self.partial.len() > Self::MAX_PARTIAL {
            if let Some(id) = self.oldest_partial() {
                self.partial.remove(&id);
                self.abandoned.push(id);
                self.last_delivered = Some(self.last_delivered.map_or(id, |l| if is_newer(id, l) { id } else { l }));
            }
        }
        Reassembly::Pending
    }

    fn oldest_partial(&self) -> Option<u32> {
        self.partial.keys().copied().reduce(|a, b| if is_newer(b, a) { a } else { b })
    }

    fn abandon_older_than(&mut self, frame_id: u32) {
        // Drop incomplete frames older than this one...
        let older: Vec<u32> = self.partial.keys().copied().filter(|&id| is_newer(frame_id, id)).collect();
        for id in older {
            self.partial.remove(&id);
            self.abandoned.push(id);
        }
        // ...and complete ones waiting behind them.
        let older: Vec<u32> = self.held.keys().copied().filter(|&id| is_newer(frame_id, id)).collect();
        for id in older {
            self.held.remove(&id);
            self.abandoned.push(id);
        }
    }

    /// Hand over the oldest held frame if nothing older is still pending
    /// (or everything older has run out of grace).
    fn deliver_next(&mut self, now_ms: u64) -> Option<CompleteFrame> {
        // The oldest complete frame waiting to be handed out.
        let next = *self.held.keys().next()?;
        // Older partial frames: still worth waiting for?
        let waiting: Vec<u32> = self.partial.keys().copied().filter(|&id| is_newer(next, id)).collect();
        // Too many complete frames queued behind a gap: the clock we were
        // given may not be moving (tests), or the link is a mess. Either
        // way, stop waiting.
        let backlog = self.held.len() >= Self::MAX_PARTIAL;
        // For every older frame still missing pieces: wait for its resend unless
        // its time is up (no packet for longer than the grace, or asked three
        // times already and two round trips passed).
        for id in waiting {
            let pf = &self.partial[&id];
            let since = now_ms.saturating_sub(pf.last_packet_ms);
            let expired =
                backlog || since > self.grace_ms || pf.requests >= Self::MAX_REQUESTS && since > self.rtt_ms * 2;
            if expired {
                self.partial.remove(&id);
                self.abandoned.push(id);
            } else {
                return None; // give the resend a chance
            }
        }
        // Nothing older to wait for: hand this frame out.
        let frame = self.held.remove(&next)?;
        self.last_delivered = Some(next);
        Some(frame)
    }

    /// Call every few milliseconds: frames whose older siblings ran out of
    /// grace become deliverable, in order.
    pub fn poll(&mut self, now_ms: u64) -> Vec<CompleteFrame> {
        let mut out = Vec::new();
        while let Some(f) = self.deliver_next(now_ms) {
            out.push(f);
        }
        out
    }

    /// Which packets to ask the host for again, right now. A frame is
    /// asked about once its packets have stopped arriving (a newer frame
    /// has started, or nothing for a couple of ms), and again no sooner
    /// than one RTT later.
    pub fn resend_requests(&mut self, now_ms: u64) -> Vec<ResendRequest> {
        // The newest frame we have heard of, partial or complete. Packets of a
        // frame are sent before the next frame's, so a newer frame having started
        // means the older one's missing pieces are really lost.
        let newest = self.partial.keys().copied().reduce(|a, b| if is_newer(a, b) { a } else { b });
        let newest_held = self.held.keys().next_back().copied();
        let mut out = Vec::new();
        for (&id, pf) in &mut self.partial {
            // Complete frames need nothing.
            if pf.slice_count > 0 && pf.received == pf.slice_count {
                continue; // done
            }
            let later_frame_seen =
                newest.is_some_and(|n| is_newer(n, id)) || newest_held.is_some_and(|n| is_newer(n, id));
            // "Quiet" scales with the round trip: on Wi-Fi a frame's packets
            // often pause for several ms (radio airtime). With a fixed 2 ms
            // the PC asked the Mac for ~5,000 packets a second that were
            // merely late — 0% real loss — and the duplicates pushed the
            // stream from 80 to 140 Mbit/s and smeared the picture (0.5.4).
            let quiet_after = self.rtt_ms.clamp(Self::QUIET_MS, Self::QUIET_MAX_MS);
            let quiet = now_ms.saturating_sub(pf.last_packet_ms) >= quiet_after;
            if !(later_frame_seen || quiet) {
                continue;
            }
            // Asked three times already: stop asking, the grace timer will decide.
            if pf.requests >= Self::MAX_REQUESTS {
                continue;
            }
            // Asked less than one round trip ago: the answer cannot be here yet.
            if pf.requested_ms.is_some_and(|t| now_ms.saturating_sub(t) < self.rtt_ms.max(2)) {
                continue;
            }
            // Only parity so far: we do not know how many data slices there
            // are, so ask for all of them (small frames, usually just one).
            let mut items = if pf.slice_count == 0 { vec![ResendRequest::ALL_DATA] } else { pf.missing_slices() };
            // Parity we never got is worth having too: one parity packet can
            // stand in for any one data slice of its stripe.
            for (k, p) in pf.parity.iter().enumerate() {
                if p.is_none() {
                    items.push(ResendRequest::PARITY_BIT | k as u16);
                }
            }
            if items.is_empty() {
                continue;
            }
            // Remember when and how often we asked.
            pf.requested_ms = Some(now_ms);
            pf.requests += 1;
            out.push(ResendRequest { frame_id: id, items });
        }
        out
    }

    /// Frame ids given up on since the last call. The viewer sends these as NACKs.
    pub fn take_abandoned(&mut self) -> Vec<u32> {
        std::mem::take(&mut self.abandoned)
    }
}

// Tests: each one builds frames, loses or reorders pieces on purpose, and
// checks the reassembler still produces exactly the original frame.
#[cfg(test)]
mod tests {

    #[test]
    fn frames_keep_flowing_across_the_frame_id_wrap() {
        let seq = SeqCounter::default();
        let mut ra = Reassembler::default();
        let mut got = Vec::new();
        for id in (u32::MAX - 3..=u32::MAX).chain(0..4) {
            for d in slice_frame_with_fec(&Bytes::from(vec![1u8; 3000]), id, false, &seq).unwrap() {
                if let Reassembly::Complete(f) = ra.push(Packet::parse(d).unwrap(), 0) {
                    got.push(f.frame_id);
                }
            }
        }
        assert_eq!(got.len(), 8, "{got:?}");
        assert!(is_newer(0, u32::MAX) && !is_newer(u32::MAX, 0));
    }

    fn fec_packets(len: usize, id: u32) -> (Bytes, Vec<Packet>) {
        let frame: Bytes = (0..len).map(|i| (i * 7 + 3) as u8).collect::<Vec<u8>>().into();
        let seq = SeqCounter::default();
        let pkts = slice_frame_with_fec(&frame, id, false, &seq)
            .unwrap()
            .into_iter()
            .map(|d| Packet::parse(d).unwrap())
            .collect();
        (frame, pkts)
    }

    fn deliver(r: &mut Reassembler, pkts: Vec<Packet>) -> Option<CompleteFrame> {
        let mut done = None;
        for p in pkts {
            if let Reassembly::Complete(f) = r.push(p, 0) {
                done = Some(f);
            }
        }
        done
    }

    #[test]
    fn fec_rebuilds_any_single_lost_slice_including_the_short_last_one() {
        let (frame, pkts) = fec_packets(MAX_PAYLOAD * 23 + 77, 1);
        let data_slices = pkts.iter().filter(|p| p.header.kind == Kind::Video).count();
        for lost in 0..data_slices {
            let mut r = Reassembler::default();
            let kept = pkts.iter().cloned().enumerate().filter(|(i, _)| *i != lost).map(|(_, p)| p).collect();
            let got = deliver(&mut r, kept).unwrap_or_else(|| panic!("slice {lost} not recovered"));
            assert_eq!(got.data, frame);
        }
    }

    #[test]
    fn fec_survives_a_burst_as_long_as_the_stripe_count() {
        let (frame, pkts) = fec_packets(MAX_PAYLOAD * 40, 1);
        let stripes = parity_count(40);
        let mut r = Reassembler::default();
        let kept =
            pkts.into_iter().enumerate().filter(|(i, _)| !(10..10 + stripes).contains(i)).map(|(_, p)| p).collect();
        assert_eq!(deliver(&mut r, kept).unwrap().data, frame);
    }

    #[test]
    fn fec_cannot_fix_two_losses_in_one_stripe() {
        let (_, pkts) = fec_packets(MAX_PAYLOAD * 20, 1);
        let stripes = parity_count(20);
        let mut r = Reassembler::default();
        let kept = pkts.into_iter().enumerate().filter(|(i, _)| *i != 0 && *i != stripes).map(|(_, p)| p).collect();
        assert!(deliver(&mut r, kept).is_none());
    }

    #[test]
    fn fec_packet_arriving_first_is_fine() {
        let (frame, mut pkts) = fec_packets(MAX_PAYLOAD * 5 + 1, 1);
        pkts.rotate_right(1); // parity first
        pkts.remove(3); // and lose a data slice
        let mut r = Reassembler::default();
        assert_eq!(deliver(&mut r, pkts).unwrap().data, frame);
    }

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
            match r.push(Packet::parse(s).unwrap(), 0) {
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
        let last = slices.into_iter().map(|s| r.push(Packet::parse(s).unwrap(), 0)).last().unwrap();
        match last {
            Reassembly::Complete(f) => assert_eq!(f.data, frame),
            other => panic!("expected Complete, got {other:?}"),
        }
    }

    #[test]
    fn newer_complete_frame_waits_for_a_resend_then_gives_up() {
        let seq = SeqCounter::default();
        let old = slice_frame(&frame_of(MAX_PAYLOAD * 2), 10, false, &seq).unwrap();
        let new = slice_frame(&frame_of(10), 11, false, &seq).unwrap();

        let mut r = Reassembler::default();
        r.set_rtt_ms(4.0); // grace = 10 ms
        assert_eq!(r.push(Packet::parse(old[0].clone()).unwrap(), 0), Reassembly::Pending);
        // Frame 11 completes but 10 is missing a slice: hold 11, ask for 10's.
        assert_eq!(r.push(Packet::parse(new[0].clone()).unwrap(), 1), Reassembly::Pending);
        let asks = r.resend_requests(1);
        assert_eq!(asks, vec![ResendRequest { frame_id: 10, items: vec![1] }]);
        assert_eq!(r.resend_requests(2), Vec::new(), "no repeat within an RTT");
        // The resend arrives: 10 completes and 11 follows in order.
        assert!(
            matches!(r.push(Packet::parse(old[1].clone()).unwrap(), 5), Reassembly::Complete(f) if f.frame_id == 10)
        );
        assert_eq!(r.poll(5).into_iter().map(|f| f.frame_id).collect::<Vec<_>>(), vec![11]);
        assert_eq!(r.resent, 1);
        assert_eq!(r.take_abandoned(), Vec::<u32>::new());

        // Same again, but the resend never comes: after the grace, 10 is
        // abandoned and 11 delivered.
        let old = slice_frame(&frame_of(MAX_PAYLOAD * 2), 20, false, &seq).unwrap();
        let new = slice_frame(&frame_of(10), 21, false, &seq).unwrap();
        assert_eq!(r.push(Packet::parse(old[0].clone()).unwrap(), 100), Reassembly::Pending);
        assert_eq!(r.push(Packet::parse(new[0].clone()).unwrap(), 101), Reassembly::Pending);
        assert_eq!(r.poll(105).len(), 0);
        assert_eq!(r.poll(111).into_iter().map(|f| f.frame_id).collect::<Vec<_>>(), vec![21]);
        assert_eq!(r.take_abandoned(), vec![20]);
        assert_eq!(r.push(Packet::parse(old[1].clone()).unwrap(), 112), Reassembly::Stale);
    }

    #[test]
    fn keyframe_does_not_wait_for_older_frames() {
        let seq = SeqCounter::default();
        let old = slice_frame(&frame_of(MAX_PAYLOAD * 2), 30, false, &seq).unwrap();
        let key = slice_frame(&frame_of(10), 31, true, &seq).unwrap();
        let mut r = Reassembler::default();
        assert_eq!(r.push(Packet::parse(old[0].clone()).unwrap(), 0), Reassembly::Pending);
        assert!(
            matches!(r.push(Packet::parse(key[0].clone()).unwrap(), 1), Reassembly::Complete(f) if f.frame_id == 31)
        );
        assert_eq!(r.take_abandoned(), vec![30]);
    }

    #[test]
    fn resend_request_round_trips() {
        let rq = ResendRequest { frame_id: 7, items: vec![3, 9, ResendRequest::PARITY_BIT | 1] };
        assert_eq!(ResendRequest::decode(&rq.encode()), Some(rq));
        assert_eq!(ResendRequest::decode(&[1, 2, 3]), None);
    }

    #[test]
    fn duplicate_slices_are_harmless() {
        let frame = frame_of(MAX_PAYLOAD + 5);
        let seq = SeqCounter::default();
        let slices = slice_frame(&frame, 3, false, &seq).unwrap();
        let mut r = Reassembler::default();
        assert_eq!(r.push(Packet::parse(slices[0].clone()).unwrap(), 0), Reassembly::Pending);
        assert_eq!(r.push(Packet::parse(slices[0].clone()).unwrap(), 0), Reassembly::Pending);
        assert!(matches!(r.push(Packet::parse(slices[1].clone()).unwrap(), 0), Reassembly::Complete(_)));
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
