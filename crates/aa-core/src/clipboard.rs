//! Shared clipboard: what gets copied on one machine becomes pasteable on
//! the other, both ways.
//!
//! Clipboard items can be large (a screenshot is megabytes) and must arrive
//! whole or not at all, unlike video where a late frame is simply skipped.
//! So a transfer is cut into `Kind::Clipboard` datagrams like a video frame,
//! and the receiver answers `Kind::ClipboardAck` once it has every piece.
//! The sender repeats the whole transfer until that ack arrives (a few
//! tries, spaced out). Clipboard changes are rare and human-paced, so this
//! simple scheme is plenty; no windowing or selective repeat needed.
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

/// Sending side: one transfer in flight at a time; a newer copy replaces an
/// older one still being sent (only the latest clipboard matters).
#[derive(Debug, Default)]
pub struct ClipSender {
    next_id: u32,
    in_flight: Option<InFlight>,
}

#[derive(Debug)]
struct InFlight {
    id: u32,
    datagrams: Vec<Bytes>,
    last_sent: Instant,
    tries: u32,
}

impl ClipSender {
    /// Gaps between repeats, growing: the first repeat covers a single lost
    /// datagram quickly, later ones give a busy link room.
    const RETRY_AFTER: [Duration; 4] =
        [Duration::from_millis(300), Duration::from_millis(600), Duration::from_secs(1), Duration::from_secs(2)];

    /// Start sending `item`. Returns the datagrams to send now, or `None`
    /// if the item is too large.
    pub fn start(&mut self, item: &ClipItem, seq: &SeqCounter) -> Option<Vec<Bytes>> {
        let encoded = item.encode();
        if encoded.len() > MAX_ITEM_BYTES {
            return None;
        }
        self.next_id = self.next_id.wrapping_add(1);
        let id = self.next_id;
        let datagrams = datagrams(&encoded, id)?;
        let now: Vec<Bytes> = datagrams.iter().map(|d| restamp(d, seq)).collect();
        self.in_flight = Some(InFlight { id, datagrams, last_sent: Instant::now(), tries: 0 });
        Some(now)
    }

    /// Call often (every ~100 ms). Returns datagrams to send again, if the
    /// ack is overdue. Gives up silently after the last retry.
    pub fn poll(&mut self, seq: &SeqCounter) -> Vec<Bytes> {
        let Some(f) = self.in_flight.as_mut() else { return Vec::new() };
        let Some(wait) = Self::RETRY_AFTER.get(f.tries as usize) else {
            self.in_flight = None;
            return Vec::new();
        };
        if f.last_sent.elapsed() < *wait {
            return Vec::new();
        }
        f.tries += 1;
        f.last_sent = Instant::now();
        f.datagrams.iter().map(|d| restamp(d, seq)).collect()
    }

    /// The other side confirmed transfer `id`.
    pub fn ack(&mut self, id: u32) {
        if self.in_flight.as_ref().is_some_and(|f| f.id == id) {
            self.in_flight = None;
        }
    }

    pub fn busy(&self) -> bool {
        self.in_flight.is_some()
    }
}

/// Receiving side.
#[derive(Debug, Default)]
pub struct ClipReceiver {
    partial: BTreeMap<u32, Vec<Option<Bytes>>>,
    /// Last transfer applied, so a repeat (our ack got lost) is acked again
    /// but not pasted twice.
    last_done: Option<u32>,
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

impl ClipReceiver {
    pub fn push(&mut self, packet: &Packet, seq: &SeqCounter) -> ClipEvent {
        let h = packet.header;
        let id = h.frame_id;
        let ack = || {
            let mut b = BytesMut::with_capacity(HEADER_LEN);
            Header {
                kind: Kind::ClipboardAck,
                flags: 0,
                seq: seq.take(),
                frame_id: id,
                slice_index: 0,
                slice_count: 1,
            }
            .write(&mut b);
            b.freeze()
        };
        if self.last_done == Some(id) {
            // Only answer once per full repeat (on its last piece).
            return if h.slice_index + 1 == h.slice_count { ClipEvent::AckOnly(ack()) } else { ClipEvent::Pending };
        }
        // A new transfer supersedes any older partial ones.
        self.partial.retain(|&k, _| k >= id);
        let slots = self.partial.entry(id).or_insert_with(|| vec![None; usize::from(h.slice_count)]);
        if slots.len() != usize::from(h.slice_count) {
            return ClipEvent::Pending;
        }
        slots[usize::from(h.slice_index)] = Some(packet.payload.clone());
        if slots.iter().any(Option::is_none) {
            return ClipEvent::Pending;
        }
        let slots = self.partial.remove(&id).expect("present");
        let mut data = BytesMut::new();
        for s in slots.into_iter().flatten() {
            data.extend_from_slice(&s);
        }
        self.last_done = Some(id);
        match ClipItem::decode(&data.freeze()) {
            Some(item) => ClipEvent::Item(item, ack()),
            None => ClipEvent::AckOnly(ack()),
        }
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
    /// The user copied `item` here.
    pub fn copied(&mut self, item: &ClipItem, seq: &SeqCounter) -> Vec<Bytes> {
        self.tx.start(item, seq).unwrap_or_else(|| {
            tracing_free_warn(item);
            Vec::new()
        })
    }

    /// Call every ~100 ms: repeats an unacknowledged transfer.
    pub fn tick(&mut self, seq: &SeqCounter) -> Vec<Bytes> {
        self.tx.poll(seq)
    }

    /// A `Clipboard` or `ClipboardAck` datagram arrived. Returns an item to
    /// paste here (if one just completed) and datagrams to send back.
    pub fn received(&mut self, packet: &Packet, seq: &SeqCounter) -> (Option<ClipItem>, Option<Bytes>) {
        match packet.header.kind {
            Kind::ClipboardAck => {
                self.tx.ack(packet.header.frame_id);
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

/// aa-core has no logging dependency; the caller sees an empty send list.
fn tracing_free_warn(_item: &ClipItem) {}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(ds: Vec<Bytes>) -> Vec<Packet> {
        ds.into_iter().map(|d| Packet::parse(d).unwrap()).collect()
    }

    #[test]
    fn items_round_trip() {
        for item in [ClipItem::Text("héllo".into()), ClipItem::Png(Bytes::from_static(b"\x89PNG..."))] {
            assert_eq!(ClipItem::decode(&item.encode()), Some(item));
        }
        assert_eq!(ClipItem::decode(&Bytes::from_static(b"\x09x")), None);
    }

    #[test]
    fn a_large_item_arrives_whole_and_is_acked() {
        let seq = SeqCounter::default();
        let big = ClipItem::Png(Bytes::from(vec![7u8; MAX_PAYLOAD * 50 + 3]));
        let mut tx = ClipSender::default();
        let mut rx = ClipReceiver::default();
        let mut got = None;
        for p in parse(tx.start(&big, &seq).unwrap()) {
            if let ClipEvent::Item(item, ack) = rx.push(&p, &seq) {
                got = Some(item);
                tx.ack(Packet::parse(ack).unwrap().header.frame_id);
            }
        }
        assert_eq!(got, Some(big));
        assert!(!tx.busy());
    }

    #[test]
    fn a_lost_piece_is_recovered_by_the_repeat_and_applied_once() {
        let seq = SeqCounter::default();
        let item = ClipItem::Text("x".repeat(MAX_PAYLOAD * 3));
        let mut tx = ClipSender::default();
        let mut rx = ClipReceiver::default();
        let first = parse(tx.start(&item, &seq).unwrap());
        for p in first.iter().skip(1) {
            assert_eq!(rx.push(p, &seq), ClipEvent::Pending);
        }
        // Retry fires once the first deadline passes.
        tx.in_flight.as_mut().unwrap().last_sent -= Duration::from_secs(1);
        let again = parse(tx.poll(&seq));
        let applied: Vec<_> = again
            .iter()
            .filter_map(|p| match rx.push(p, &seq) {
                ClipEvent::Item(i, _) => Some(i),
                _ => None,
            })
            .collect();
        assert_eq!(applied, vec![item]);
        // The ack got lost: the next repeat is acked again but not reapplied.
        tx.in_flight.as_mut().unwrap().last_sent -= Duration::from_secs(1);
        let third = parse(tx.poll(&seq));
        let events: Vec<_> = third.iter().map(|p| rx.push(p, &seq)).collect();
        assert!(events.iter().all(|e| !matches!(e, ClipEvent::Item(..))));
        assert!(matches!(events.last(), Some(ClipEvent::AckOnly(_))));
    }

    #[test]
    fn sender_gives_up_eventually() {
        let seq = SeqCounter::default();
        let mut tx = ClipSender::default();
        tx.start(&ClipItem::Text("a".into()), &seq);
        for _ in 0..10 {
            if let Some(f) = tx.in_flight.as_mut() {
                f.last_sent -= Duration::from_secs(5);
            }
            tx.poll(&seq);
        }
        assert!(!tx.busy());
    }

    #[test]
    fn two_ends_sync_both_ways() {
        let (sa, sb) = (SeqCounter::default(), SeqCounter::default());
        let (mut a, mut b) = (ClipSync::default(), ClipSync::default());
        let mut pasted_on_b = None;
        for d in a.copied(&ClipItem::Text("from a".into()), &sa) {
            let (item, ack) = b.received(&Packet::parse(d).unwrap(), &sb);
            pasted_on_b = pasted_on_b.or(item);
            if let Some(ack) = ack {
                a.received(&Packet::parse(ack).unwrap(), &sa);
            }
        }
        assert_eq!(pasted_on_b, Some(ClipItem::Text("from a".into())));
        assert!(!a.tx.busy());
    }

    #[test]
    fn oversized_items_are_refused() {
        let seq = SeqCounter::default();
        let mut tx = ClipSender::default();
        assert!(tx.start(&ClipItem::Png(Bytes::from(vec![0u8; MAX_ITEM_BYTES + 1])), &seq).is_none());
    }
}
