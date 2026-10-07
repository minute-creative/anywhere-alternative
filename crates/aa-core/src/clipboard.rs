//! Shared clipboard: what gets copied on one machine becomes pasteable on
//! the other, both ways.
//!
//! Clipboard items can be large (a screenshot is megabytes) and must arrive
//! whole or not at all, unlike video where a late frame is simply skipped.
//! A transfer is cut into `Kind::Clipboard` pieces like a video frame and
//! sent paced (a few hundred pieces per 100 ms, about 30 Mbps), so a big
//! paste never swamps the picture. The receiver answers `ClipboardAck`:
//! empty when it has everything, or listing the pieces still missing once
//! the flow pauses, and only those are sent again.
//!
//! The first version repeated the *whole* transfer until one copy arrived
//! intact. A 4 MB image is ~3,400 pieces; at 1% loss the odds of a flawless
//! copy are about 10^-15, so big images never arrived.
//!
//! Item encoding: one format byte, then the data.
//! * `1` text, UTF-8
//! * `2` image, PNG

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use bytes::{BufMut, Bytes, BytesMut};

use crate::wire::{Header, Kind, Packet, SeqCounter, HEADER_LEN, MAX_PAYLOAD};

/// Largest item we will send: 32 MB covers a 4K screenshot as PNG with lots
/// of room; past that the transfer would stall the link for seconds.
pub const MAX_ITEM_BYTES: usize = 32 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClipItem {
    Text(String),
    /// PNG-encoded image.
    Png(Bytes),
}

impl ClipItem {
    pub fn encode(&self) -> Bytes {
        let (tag, data): (u8, &[u8]) = match self {
            Self::Text(t) => (1, t.as_bytes()),
            Self::Png(p) => (2, p),
        };
        let mut out = BytesMut::with_capacity(1 + data.len());
        out.put_u8(tag);
        out.extend_from_slice(data);
        out.freeze()
    }

    pub fn decode(bytes: &Bytes) -> Option<Self> {
        let (&tag, rest) = bytes.split_first()?;
        match tag {
            1 => String::from_utf8(rest.to_vec()).ok().map(Self::Text),
            2 => Some(Self::Png(bytes.slice(1..))),
            _ => None,
        }
    }

    /// Short description for logs (never the content: it may be a password).
    pub fn describe(&self) -> String {
        match self {
            Self::Text(t) => format!("text, {} chars", t.chars().count()),
            Self::Png(p) => format!("image, {} KB", p.len() / 1024),
        }
    }
}

/// Cut an encoded item into datagrams for transfer `id`.
fn datagrams(item: &Bytes, id: u32) -> Option<Vec<Bytes>> {
    let count = item.len().div_ceil(MAX_PAYLOAD).max(1);
    let count = u16::try_from(count).ok()?;
    let mut out = Vec::with_capacity(usize::from(count));
    for i in 0..count {
        let start = usize::from(i) * MAX_PAYLOAD;
        let chunk = &item[start..(start + MAX_PAYLOAD).min(item.len())];
        let mut buf = BytesMut::with_capacity(HEADER_LEN + chunk.len());
        Header { kind: Kind::Clipboard, flags: 0, seq: 0, frame_id: id, slice_index: i, slice_count: count }
            .write(&mut buf);
        buf.extend_from_slice(chunk);
        out.push(buf.freeze());
    }
    Some(out)
}

/// Stamp a fresh sequence number into an already-built datagram, so the
/// receiver's loss tracker sees one monotonic sequence even on resends.
fn restamp(d: &Bytes, seq: &SeqCounter) -> Bytes {
    let mut b = BytesMut::from(&d[..]);
    b[2..4].copy_from_slice(&seq.take().to_be_bytes());
    b.freeze()
}

/// Pieces sent per `tick` (every ~100 ms): ~350 × 1.2 KB ≈ 33 Mbps.
const PACE: usize = 350;

/// Sending side: one transfer at a time; a newer copy replaces an older one
/// still being sent (only the latest clipboard matters).
#[derive(Debug, Default)]
pub struct ClipSender {
    next_id: u32,
    in_flight: Option<InFlight>,
}

#[derive(Debug)]
struct InFlight {
    id: u32,
    datagrams: Vec<Bytes>,
    /// Pieces still to (re)send, in order.
    queue: std::collections::VecDeque<u16>,
    last_heard_or_sent: Instant,
    /// Full repeats done because the receiver went silent.
    tries: u32,
}

impl ClipSender {
    /// If the receiver says nothing for this long after we finished
    /// sending, we send everything again (it may have missed all of it).
    /// Grows each time; after the last we give up.
    const SILENT_RETRY: [Duration; 4] =
        [Duration::from_millis(600), Duration::from_secs(1), Duration::from_secs(2), Duration::from_secs(4)];

    /// Start sending `item`; pieces go out on the following `poll`s.
    /// Returns false if the item is too large.
    pub fn start(&mut self, item: &ClipItem) -> bool {
        let encoded = item.encode();
        if encoded.len() > MAX_ITEM_BYTES {
            return false;
        }
        self.next_id = self.next_id.wrapping_add(1);
        let id = self.next_id;
        let Some(datagrams) = datagrams(&encoded, id) else { return false };
        let queue = (0..u16::try_from(datagrams.len()).unwrap_or(u16::MAX)).collect();
        self.in_flight = Some(InFlight { id, datagrams, queue, last_heard_or_sent: Instant::now(), tries: 0 });
        true
    }

    /// Call every ~100 ms: the next paced batch, or a full repeat if the
    /// receiver has gone silent.
    pub fn poll(&mut self, seq: &SeqCounter) -> Vec<Bytes> {
        let Some(f) = self.in_flight.as_mut() else { return Vec::new() };
        if f.queue.is_empty() {
            let Some(wait) = Self::SILENT_RETRY.get(f.tries as usize) else {
                self.in_flight = None;
                return Vec::new();
            };
            if f.last_heard_or_sent.elapsed() < *wait {
                return Vec::new();
            }
            f.tries += 1;
            f.queue = (0..u16::try_from(f.datagrams.len()).unwrap_or(u16::MAX)).collect();
        }
        let n = f.queue.len().min(PACE);
        let out = f.queue.drain(..n).map(|i| restamp(&f.datagrams[usize::from(i)], seq)).collect();
        f.last_heard_or_sent = Instant::now();
        out
    }

    /// The receiver confirmed transfer `id` complete.
    pub fn ack(&mut self, id: u32) {
        if self.in_flight.as_ref().is_some_and(|f| f.id == id) {
            self.in_flight = None;
        }
    }

    /// The receiver is missing these pieces of transfer `id`.
    pub fn missing(&mut self, id: u32, pieces: &[u16]) {
        let Some(f) = self.in_flight.as_mut().filter(|f| f.id == id) else { return };
        for &p in pieces {
            if usize::from(p) < f.datagrams.len() && !f.queue.contains(&p) {
                f.queue.push_back(p);
            }
        }
        f.last_heard_or_sent = Instant::now();
        f.tries = 0;
    }

    pub fn busy(&self) -> bool {
        self.in_flight.is_some()
    }
}

/// Receiving side.
#[derive(Debug, Default)]
pub struct ClipReceiver {
    partial: BTreeMap<u32, Partial>,
    /// Last transfer applied, so a repeat (our ack got lost) is acked again
    /// but not pasted twice.
    last_done: Option<u32>,
}

#[derive(Debug)]
struct Partial {
    slots: Vec<Option<Bytes>>,
    last_piece: Instant,
    last_nack: Option<Instant>,
}

/// What a received clipboard datagram produced.
#[derive(Debug, PartialEq, Eq)]
pub enum ClipEvent {
    /// Nothing yet.
    Pending,
    /// Send this ack; the transfer was already applied.
    AckOnly(Bytes),
    /// A new item arrived: apply it and send the ack.
    Item(ClipItem, Bytes),
}

fn ack_datagram(id: u32, missing: &[u16], seq: &SeqCounter) -> Bytes {
    let mut b = BytesMut::with_capacity(HEADER_LEN + missing.len() * 2);
    Header { kind: Kind::ClipboardAck, flags: 0, seq: seq.take(), frame_id: id, slice_index: 0, slice_count: 1 }
        .write(&mut b);
    for m in missing {
        b.put_u16(*m);
    }
    b.freeze()
}

impl ClipReceiver {
    /// After this long without a new piece, ask for what is missing.
    const QUIET: Duration = Duration::from_millis(150);

    pub fn push(&mut self, packet: &Packet, seq: &SeqCounter) -> ClipEvent {
        let h = packet.header;
        let id = h.frame_id;
        if self.last_done == Some(id) {
            // Only answer once per full repeat (on its last piece).
            return if h.slice_index + 1 == h.slice_count {
                ClipEvent::AckOnly(ack_datagram(id, &[], seq))
            } else {
                ClipEvent::Pending
            };
        }
        // A new transfer supersedes any older partial ones.
        self.partial.retain(|&k, _| k >= id);
        let p = self.partial.entry(id).or_insert_with(|| Partial {
            slots: vec![None; usize::from(h.slice_count)],
            last_piece: Instant::now(),
            last_nack: None,
        });
        if p.slots.len() != usize::from(h.slice_count) {
            return ClipEvent::Pending;
        }
        p.slots[usize::from(h.slice_index)] = Some(packet.payload.clone());
        p.last_piece = Instant::now();
        if p.slots.iter().any(Option::is_none) {
            return ClipEvent::Pending;
        }
        let p = self.partial.remove(&id).expect("present");
        let mut data = BytesMut::new();
        for s in p.slots.into_iter().flatten() {
            data.extend_from_slice(&s);
        }
        self.last_done = Some(id);
        let ack = ack_datagram(id, &[], seq);
        match ClipItem::decode(&data.freeze()) {
            Some(item) => ClipEvent::Item(item, ack),
            None => ClipEvent::AckOnly(ack),
        }
    }

    /// Call every ~100 ms: asks for missing pieces of any transfer whose
    /// flow has paused.
    pub fn poll(&mut self, seq: &SeqCounter) -> Vec<Bytes> {
        let mut out = Vec::new();
        for (&id, p) in &mut self.partial {
            let quiet = p.last_piece.elapsed() >= Self::QUIET;
            let not_just_asked = p.last_nack.is_none_or_elapsed(Self::QUIET);
            if quiet && not_just_asked {
                let missing: Vec<u16> = p
                    .slots
                    .iter()
                    .enumerate()
                    .filter(|(_, s)| s.is_none())
                    .filter_map(|(i, _)| u16::try_from(i).ok())
                    .take(MAX_PAYLOAD / 2)
                    .collect();
                p.last_nack = Some(Instant::now());
                out.push(ack_datagram(id, &missing, seq));
            }
        }
        out
    }
}

/// `Option<Instant>::is_none_or` with an elapsed test, MSRV-friendly.
trait ElapsedOr {
    fn is_none_or_elapsed(&self, d: Duration) -> bool;
}

impl ElapsedOr for Option<Instant> {
    fn is_none_or_elapsed(&self, d: Duration) -> bool {
        self.map_or(true, |t| t.elapsed() >= d)
    }
}

/// Both directions for one session, with no I/O: each call returns the
/// datagrams to put on the wire. Hosts and viewers drive it the same way.
#[derive(Debug, Default)]
pub struct ClipSync {
    tx: ClipSender,
    rx: ClipReceiver,
}

impl ClipSync {
    /// The user copied `item` here. Pieces go out on the next ticks.
    /// Returns false if it is too large to share.
    pub fn copied(&mut self, item: &ClipItem) -> bool {
        self.tx.start(item)
    }

    /// Call every ~100 ms: paced sending, repeats, and requests for
    /// missing pieces.
    pub fn tick(&mut self, seq: &SeqCounter) -> Vec<Bytes> {
        let mut out = self.tx.poll(seq);
        out.extend(self.rx.poll(seq));
        out
    }

    /// A `Clipboard` or `ClipboardAck` datagram arrived. Returns an item to
    /// paste here (if one just completed) and a datagram to send back.
    pub fn received(&mut self, packet: &Packet, seq: &SeqCounter) -> (Option<ClipItem>, Option<Bytes>) {
        match packet.header.kind {
            Kind::ClipboardAck => {
                let p = &packet.payload;
                if p.is_empty() {
                    self.tx.ack(packet.header.frame_id);
                } else {
                    let missing: Vec<u16> = p.chunks_exact(2).map(|c| u16::from_be_bytes([c[0], c[1]])).collect();
                    self.tx.missing(packet.header.frame_id, &missing);
                }
                (None, None)
            }
            Kind::Clipboard => match self.rx.push(packet, seq) {
                ClipEvent::Pending => (None, None),
                ClipEvent::AckOnly(a) => (None, Some(a)),
                ClipEvent::Item(i, a) => (Some(i), Some(a)),
            },
            _ => (None, None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(d: Bytes) -> Packet {
        Packet::parse(d).unwrap()
    }

    /// Run two ends against each other through a lossy "link" until done.
    /// `drop` decides per datagram (by running count) whether it is lost.
    fn run(item: &ClipItem, mut drop: impl FnMut(u64) -> bool, max_ticks: usize) -> (Option<ClipItem>, usize) {
        let (sa, sb) = (SeqCounter::default(), SeqCounter::default());
        let (mut a, mut b) = (ClipSync::default(), ClipSync::default());
        assert!(a.copied(item));
        let mut n = 0u64;
        let mut got = None;
        for tick in 0..max_ticks {
            for d in a.tick(&sa) {
                n += 1;
                if drop(n) {
                    continue;
                }
                let (item, reply) = b.received(&parse(d), &sb);
                got = got.or(item);
                if let Some(r) = reply {
                    a.received(&parse(r), &sa);
                }
            }
            // The receiver's own timer: let the "quiet" interval pass.
            for p in b.rx.partial.values_mut() {
                p.last_piece = p.last_piece.checked_sub(Duration::from_millis(200)).unwrap_or(p.last_piece);
                p.last_nack = p.last_nack.and_then(|t| t.checked_sub(Duration::from_millis(200)));
            }
            for r in b.tick(&sb) {
                a.received(&parse(r), &sa);
            }
            if !a.tx.busy() {
                return (got, tick + 1);
            }
        }
        (got, max_ticks)
    }

    #[test]
    fn items_round_trip() {
        for item in [ClipItem::Text("héllo".into()), ClipItem::Png(Bytes::from_static(b"\x89PNG..."))] {
            assert_eq!(ClipItem::decode(&item.encode()), Some(item));
        }
        assert_eq!(ClipItem::decode(&Bytes::from_static(b"\x09x")), None);
    }

    #[test]
    fn small_text_arrives_on_a_clean_link() {
        let item = ClipItem::Text("hello".into());
        let (got, ticks) = run(&item, |_| false, 10);
        assert_eq!(got, Some(item));
        assert_eq!(ticks, 1);
    }

    #[test]
    fn a_4mb_image_arrives_whole_at_one_percent_loss() {
        let item = ClipItem::Png(Bytes::from((0..4_000_000u32).map(|i| (i % 251) as u8).collect::<Vec<u8>>()));
        let mut rng = 12345u64;
        let (got, ticks) = run(
            &item,
            |_| {
                rng = rng.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
                (rng >> 33) % 100 == 0
            },
            60,
        );
        assert_eq!(got, Some(item), "after {ticks} ticks");
        // ~3,400 pieces at 350 per tick is 10 ticks; repairs add a couple.
        assert!(ticks <= 16, "took {ticks} ticks");
    }

    #[test]
    fn total_silence_triggers_a_full_repeat_then_gives_up() {
        let seq = SeqCounter::default();
        let mut tx = ClipSender::default();
        tx.start(&ClipItem::Text("a".into()));
        assert_eq!(tx.poll(&seq).len(), 1);
        for _ in 0..4 {
            tx.in_flight.as_mut().unwrap().last_heard_or_sent -= Duration::from_secs(5);
            assert_eq!(tx.poll(&seq).len(), 1, "full repeat");
        }
        tx.in_flight.as_mut().unwrap().last_heard_or_sent -= Duration::from_secs(5);
        tx.poll(&seq);
        assert!(!tx.busy());
    }

    #[test]
    fn a_repeat_after_a_lost_ack_is_not_pasted_twice() {
        let seq = SeqCounter::default();
        let mut rx = ClipReceiver::default();
        let mut tx = ClipSender::default();
        tx.start(&ClipItem::Text("once".into()));
        let first = tx.poll(&seq);
        assert!(matches!(rx.push(&parse(first[0].clone()), &seq), ClipEvent::Item(..)));
        assert!(matches!(rx.push(&parse(first[0].clone()), &seq), ClipEvent::AckOnly(_)));
    }

    #[test]
    fn oversized_items_are_refused() {
        let mut tx = ClipSender::default();
        assert!(!tx.start(&ClipItem::Png(Bytes::from(vec![0u8; MAX_ITEM_BYTES + 1]))));
    }
}
