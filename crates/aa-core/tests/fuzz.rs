//! Hostile-input tests: random and adversarial bytes into every parser and
//! every stateful receiver. A pass means: no panic, no endless loop, bounded
//! memory, and never a corrupted frame or clipboard item handed out.
//!
//! Deterministic (seeded) so a failure reproduces. Raise `ROUNDS` with the
//! env var `AA_FUZZ_ROUNDS` for a longer soak.

use aa_core::audio::{AudioHeader, AudioSequence};
use aa_core::clipboard::{ClipItem, ClipSync};
use aa_core::control::ControlMessage;
use aa_core::control_flow::{BitrateController, ReceiverReport};
use aa_core::input::InputEvent;
use aa_core::stats::LossTracker;
use aa_core::wire::{self, Header, Kind, Packet, Reassembler, Reassembly, SeqCounter, HEADER_LEN, MAX_PAYLOAD};
use bytes::{Bytes, BytesMut};

/// Small, fast, seeded RNG (xorshift64*), no dependency needed.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
    fn chance(&mut self, pct: u64) -> bool {
        self.below(100) < pct
    }
    fn bytes(&mut self, len: usize) -> Vec<u8> {
        (0..len).map(|_| self.next() as u8).collect()
    }
}

fn rounds(default: usize) -> usize {
    std::env::var("AA_FUZZ_ROUNDS").ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

fn header(kind: Kind, frame_id: u32, ix: u16, n: u16) -> BytesMut {
    let mut b = BytesMut::new();
    Header { kind, flags: 0, seq: 0, frame_id, slice_index: ix, slice_count: n }.write(&mut b);
    b
}

#[test]
fn every_parser_survives_random_bytes() {
    let mut r = Rng(0x00C0_FFEE);
    for _ in 0..rounds(200_000) {
        let len = r.below(1600) as usize;
        let raw = Bytes::from(r.bytes(len));
        let _ = Packet::parse(raw.clone());
        let _ = ControlMessage::decode(&raw);
        let _ = ClipItem::decode(&raw);
        let _ = ReceiverReport::decode(&mut raw.clone());
        let _ = AudioHeader::read(&mut raw.clone());
        // Input decoding is a loop over a datagram: it must always consume
        // or stop, never spin.
        let mut buf = raw.clone();
        let mut guard = 0;
        while !buf.is_empty() && InputEvent::decode(&mut buf).is_ok() {
            guard += 1;
            assert!(guard <= len, "input decoder did not consume bytes");
        }
    }
}

#[test]
fn control_parser_survives_json_shaped_garbage() {
    let mut r = Rng(7);
    let pieces = [
        "{",
        "}",
        "\"Hello\"",
        ":",
        ",",
        "\"protocol\"",
        "65535",
        "-1",
        "1e999",
        "null",
        "[",
        "]",
        "\"codecs\"",
        "\"Hevc\"",
        "\"Clipboard\"",
        "\"Beacon\"",
        "\"name\"",
        "\"\\u0000\"",
        "true",
        "\"HostStatus\"",
        "\"message\"",
        "18446744073709551616",
    ];
    for _ in 0..rounds(100_000) {
        let n = r.below(40) as usize;
        let s: String = (0..n).map(|_| pieces[r.below(pieces.len() as u64) as usize]).collect();
        let _ = ControlMessage::decode(s.as_bytes());
    }
    // A very long but valid string must not be a problem either.
    let long = format!("{{\"type\":\"here\",\"name\":\"{}\"}}", "x".repeat(100_000));
    assert!(ControlMessage::decode(long.as_bytes()).is_ok());
}

/// Frames through a link that loses, duplicates and reorders. Every frame
/// the reassembler hands out must be byte-identical to what was sent.
#[test]
fn reassembler_never_delivers_a_wrong_frame() {
    let mut r = Rng(42);
    let seq = SeqCounter::default();
    let mut originals = std::collections::HashMap::new();
    let mut ra = Reassembler::default();
    let mut delivered = 0u32;
    let mut in_flight: Vec<Bytes> = Vec::new();
    for frame_id in 0..rounds(6_000) as u32 {
        let len = 1 + r.below(MAX_PAYLOAD as u64 * 40) as usize;
        let frame = Bytes::from(r.bytes(len));
        originals.insert(frame_id, frame.clone());
        let mut pkts = wire::slice_frame_with_fec(&frame, frame_id, r.chance(5), &seq).unwrap();
        // Lose some, duplicate some.
        pkts.retain(|_| !r.chance(3));
        let dups: Vec<Bytes> = pkts.iter().filter(|_| r.chance(2)).cloned().collect();
        pkts.extend(dups);
        in_flight.extend(pkts);
        // Mild reordering: shuffle a window.
        let n = in_flight.len();
        for i in 0..n {
            if r.chance(5) {
                let j = (i + 1 + r.below(4) as usize).min(n - 1);
                in_flight.swap(i, j);
            }
        }
        // Deliver all but a random tail (it arrives with the next frame).
        let keep = r.below(3) as usize;
        let cut = in_flight.len().saturating_sub(keep);
        for d in in_flight.drain(..cut) {
            if let Reassembly::Complete(f) = ra.push(Packet::parse(d).unwrap()) {
                assert_eq!(f.data, originals[&f.frame_id], "frame {} corrupted", f.frame_id);
                delivered += 1;
            }
        }
        let _ = ra.take_abandoned();
        originals.retain(|&id, _| id + 50 > frame_id);
    }
    // 3% loss with 10% FEC: most frames should survive.
    assert!(delivered as usize > rounds(6_000) * 80 / 100, "only {delivered} delivered");
}

/// Malicious or broken headers: wrong counts, parity that disagrees,
/// length fields that lie, huge counts. Nothing may panic or grow without
/// bound.
#[test]
fn reassembler_survives_hostile_headers() {
    let mut r = Rng(99);
    let mut ra = Reassembler::default();
    for _ in 0..rounds(300_000) {
        let kind = if r.chance(50) { Kind::Video } else { Kind::VideoFec };
        let cap = if r.chance(10) { 65_535 } else { 20 };
        let n = 1 + r.below(cap) as u16;
        let ix = r.below(u64::from(n)) as u16;
        let jump = if r.chance(1) { r.next() as u32 } else { 0 };
        let frame_id = (r.below(10) as u32).wrapping_add(jump);
        let mut b = header(kind, frame_id, ix, n);
        let plen = r.below(MAX_PAYLOAD as u64 + 1) as usize;
        b.extend_from_slice(&r.bytes(plen));
        let _ = ra.push(Packet::parse(b.freeze()).unwrap());
        let _ = ra.take_abandoned();
    }
}

/// Clipboard: random pieces, acks with random "missing" lists, transfers
/// that change size mid-way, ids that count down to force many partials.
#[test]
fn clipboard_survives_hostile_traffic() {
    let mut r = Rng(5);
    let seq = SeqCounter::default();
    let mut sync = ClipSync::default();
    assert!(sync.copied(&ClipItem::Text("x".repeat(50_000))));
    for i in 0..rounds(200_000) {
        let kind = if r.chance(70) { Kind::Clipboard } else { Kind::ClipboardAck };
        let cap = if r.chance(5) { 65_535 } else { 30 };
        let n = 1 + r.below(cap) as u16;
        let ix = r.below(u64::from(n)) as u16;
        // Ids that go *down* are the nasty case for "keep newer partials".
        let id = 1_000_000u32.saturating_sub(i as u32);
        let mut b = header(kind, id, ix, n);
        let plen = r.below(MAX_PAYLOAD as u64 + 1) as usize;
        b.extend_from_slice(&r.bytes(plen));
        let (item, _reply) = sync.received(&Packet::parse(b.freeze()).unwrap(), &seq);
        let _ = item;
        if i % 100 == 0 {
            let _ = sync.tick(&seq);
        }
    }
    assert!(sync.partial_transfers() <= 2, "{} partial transfers kept", sync.partial_transfers());
}

#[test]
fn sequence_trackers_never_underflow_or_go_negative() {
    let mut r = Rng(3);
    let mut loss = LossTracker::default();
    let mut audio = AudioSequence::default();
    let mut s: u16 = 0;
    for _ in 0..rounds(1_000_000) {
        // Mostly forward, sometimes back, sometimes far jumps, across wrap.
        s = match r.below(100) {
            0..=84 => s.wrapping_add(1),
            85..=94 => s.wrapping_sub(r.below(5) as u16),
            95..=98 => s.wrapping_add(r.below(200) as u16),
            _ => r.next() as u16,
        };
        loss.observe(s);
        let _ = audio.observe(s);
        assert!(loss.loss_ratio() >= 0.0 && loss.loss_ratio() <= 1.0);
    }
}

#[test]
fn bitrate_controller_stays_in_bounds_whatever_it_is_told() {
    let mut r = Rng(11);
    for _ in 0..200 {
        let min = r.below(5_000) as u32;
        let max = r.below(200_000) as u32;
        let mut c = BitrateController::new(r.next() as u32, min, max);
        for _ in 0..2_000 {
            let rep = ReceiverReport {
                loss_per_10k: r.below(20_000) as u16,
                frames_abandoned: r.below(300) as u16,
                frames_received: r.below(300) as u16,
                rtt_tenths_ms: if r.chance(5) { 0 } else { r.below(65_535) as u16 },
            };
            c.on_report(&rep);
            if r.chance(1) {
                c.set_max_kbps(r.below(200_000) as u32);
            }
            let k = c.current_kbps();
            assert!(k >= min.min(max.max(min)), "{k} under floor {min}");
        }
    }
}

#[test]
fn slicing_handles_empty_tiny_and_huge_frames() {
    let seq = SeqCounter::default();
    for len in [0usize, 1, MAX_PAYLOAD - 1, MAX_PAYLOAD, MAX_PAYLOAD + 1, MAX_PAYLOAD * 3000] {
        let frame = Bytes::from(vec![9u8; len]);
        let pkts = wire::slice_frame_with_fec(&frame, 7, true, &seq).unwrap();
        let mut ra = Reassembler::default();
        let mut got = None;
        for p in pkts {
            assert!(p.len() <= wire::MAX_DATAGRAM + 2, "datagram of {} bytes", p.len());
            if let Reassembly::Complete(f) = ra.push(Packet::parse(p).unwrap()) {
                got = Some(f.data);
            }
        }
        assert_eq!(got.as_deref(), Some(&frame[..]), "len {len}");
    }
    // Past 65535 slices a frame cannot be numbered: refused, not wrapped.
    let too_big = Bytes::from(vec![0u8; MAX_PAYLOAD * 65_536]);
    assert!(wire::slice_frame_with_fec(&too_big, 1, false, &seq).is_err());
    let _ = HEADER_LEN;
}
