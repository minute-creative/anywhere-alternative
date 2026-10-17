//! Pairing and encryption.
//!
//! **Why.** Before this, anyone on the same network who ran Anywhere could
//! connect, and every picture and keystroke crossed the network readable.
//! That was tolerable on a home Wi-Fi; it is not over the internet.
//!
//! **Pairing (once per pair of computers).** The sharing computer shows a
//! six-digit code. The other computer types it. Both run SPAKE2, a
//! *password-authenticated key exchange*: they end up with a shared secret
//! only if both used the same code, and someone listening learns nothing
//! that lets them guess the code offline (each guess needs a live attempt,
//! which the host rate-limits). With that secret they swap their permanent
//! public keys, and each computer remembers the other's.
//!
//! **Every connection after that.** No code. A three-way Diffie-Hellman
//! between both computers' permanent keys and fresh one-time keys (the
//! pattern behind Signal and `WireGuard`) gives two session keys only the
//! two real computers can compute. Every datagram is then sealed with
//! ChaCha20-Poly1305: unreadable to others, and any change or replay is
//! detected and dropped.
//!
//! Datagram first bytes (plain packets start with a wire `Kind`, 1-15):
//!
//! ```text
//! 0xA0 INIT      viewer → host   [viewer key 32][one-time key 32]
//! 0xA1 RESP      host → viewer   [host key 32][one-time key 32][proof 16]
//! 0xA2 UNKNOWN   host → viewer   [host key 32][host name]   "pair first"
//! 0xA3 PAIR1     viewer → host   [spake 33][viewer key 32][viewer name]
//! 0xA4 PAIR2     host → viewer   [spake 33][host key 32][proof 32][host name]
//! 0xA5 PAIR3     viewer → host   [proof 32]
//! 0xA6 PAIR_OK   host → viewer
//! 0xA7 PAIR_FAIL host → viewer   [reason]
//! 0xA8 RESET     host → viewer   "I don't have your session (I restarted)"
//! 0xAE SEALED    both            [counter u64][ciphertext + 16-byte tag]
//! ```

// In plain words: the two computers only ever react to packets they can
// read with keys that only they hold. A stranger on the network sees noise
// and cannot make the host do anything, not even with recorded packets.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use rand_core::{OsRng, RngCore};
use sha2::{Digest, Sha256};
use spake2::{Ed25519Group, Identity as SpakeId, Password, Spake2};
use x25519_dalek::{PublicKey, StaticSecret};

// The first byte of each special packet below (0xA0 and up never clash with
// the ordinary packet kinds, which are small numbers).
pub const INIT: u8 = 0xA0;
pub const RESP: u8 = 0xA1;
pub const UNKNOWN: u8 = 0xA2;
pub const PAIR1: u8 = 0xA3;
pub const PAIR2: u8 = 0xA4;
pub const PAIR3: u8 = 0xA5;
pub const PAIR_OK: u8 = 0xA6;
pub const PAIR_FAIL: u8 = 0xA7;
pub const RESET: u8 = 0xA8;
pub const SEALED: u8 = 0xAE;

/// Bytes a sealed datagram adds: tag byte, counter, authentication tag.
/// `wire::MAX_DATAGRAM` (1200) + 25 still fits Tailscale's 1280-byte links.
pub const OVERHEAD: usize = 1 + 8 + 16;

/// A public key (32 bytes).
pub type PublicKeyBytes = [u8; 32];

const SPAKE_LEN: usize = 33;
const NAME_MAX: usize = 64;

// Each computer has a permanent secret key (kept in a file, never sent) and
// the matching public key (shared during pairing, like a phone number).
/// This computer's permanent key pair.
pub struct Identity {
    secret: StaticSecret,
    public: PublicKeyBytes,
}

impl std::fmt::Debug for Identity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Identity").field("public", &fingerprint(&self.public)).finish_non_exhaustive()
    }
}

impl Identity {
    // A brand-new random key pair (first run on this computer).
    pub fn generate() -> Self {
        Self::from_secret(random_bytes())
    }

    // Rebuild the key pair from the saved secret (every later run).
    pub fn from_secret(bytes: [u8; 32]) -> Self {
        let secret = StaticSecret::from(bytes);
        let public = PublicKey::from(&secret).to_bytes();
        Self { secret, public }
    }

    pub fn secret_bytes(&self) -> [u8; 32] {
        self.secret.to_bytes()
    }

    pub fn public(&self) -> PublicKeyBytes {
        self.public
    }
}

// 32 random bytes from the operating system's secure random source.
fn random_bytes() -> [u8; 32] {
    let mut b = [0u8; 32];
    OsRng.fill_bytes(&mut b);
    b
}

/// A fresh six-digit pairing code.
pub fn new_code() -> String {
    // Any number 000000-999999, chosen at random.
    format!("{:06}", OsRng.next_u32() % 1_000_000)
}

/// Digits only, so "482 913" and "482-913" both work.
pub fn normalize_code(code: &str) -> String {
    code.chars().filter(char::is_ascii_digit).collect()
}

/// Lower-case hex, for files and logs.
pub fn to_hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

// Read a public key written as 64 hex characters; anything else -> None.
pub fn from_hex(s: &str) -> Option<PublicKeyBytes> {
    let s = s.trim();
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, o) in out.iter_mut().enumerate() {
        *o = u8::from_str_radix(s.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(out)
}

/// A short, readable form of a key ("3F9A-12C0") for people to compare.
pub fn fingerprint(key: &PublicKeyBytes) -> String {
    let h = Sha256::digest(key);
    format!("{:02X}{:02X}-{:02X}{:02X}", h[0], h[1], h[2], h[3])
}

// A computer name from the network: no control characters, at most 64 bytes
// (a hostile peer cannot inject odd text into the app or logs).
fn clean_name(raw: &[u8]) -> String {
    let s = String::from_utf8_lossy(raw);
    let s: String = s.chars().filter(|c| !c.is_control()).collect();
    let mut s = s.trim().to_owned();
    while s.len() > NAME_MAX {
        s.pop();
    }
    s
}

fn name_bytes(name: &str) -> Vec<u8> {
    clean_name(name.as_bytes()).into_bytes()
}

// The 32 bytes starting at position "at", if the packet is long enough.
fn key_at(d: &[u8], at: usize) -> Option<PublicKeyBytes> {
    d.get(at..at + 32)?.try_into().ok()
}

// ---------------------------------------------------------------------------
// Sealed datagrams
// ---------------------------------------------------------------------------

/// Remembers which counters arrived recently, so a recorded packet played
/// back (say, a key press) is dropped instead of acted on twice.
#[derive(Debug)]
struct ReplayWindow {
    top: u64,
    bits: [u64; Self::WORDS],
}

impl ReplayWindow {
    // 2,048 bits = remember the last 2,048 counters; anything older is refused.
    const WORDS: usize = 32;
    const SIZE: u64 = 64 * Self::WORDS as u64; // 2048 packets: ~1/3 s of 4K video

    fn new() -> Self {
        let mut w = Self { top: 0, bits: [0; Self::WORDS] };
        w.set(0); // counters start at 1
        w
    }

    // Which 64-bit word and which bit inside it stands for counter n.
    fn bit(n: u64) -> (usize, u64) {
        let i = n % Self::SIZE;
        ((i / 64) as usize, 1u64 << (i % 64))
    }

    fn set(&mut self, n: u64) {
        let (w, m) = Self::bit(n);
        self.bits[w] |= m;
    }

    fn is_set(&self, n: u64) -> bool {
        let (w, m) = Self::bit(n);
        self.bits[w] & m != 0
    }

    /// True the first time `n` is seen (and it isn't too old).
    fn accept(&mut self, n: u64) -> bool {
        // A counter newer than anything seen: clear the bits it skips over and
        // move the window forward.
        if n > self.top {
            if n - self.top >= Self::SIZE {
                self.bits = [0; Self::WORDS];
            } else {
                for k in self.top + 1..n {
                    let (w, m) = Self::bit(k);
                    self.bits[w] &= !m;
                }
            }
            self.top = n;
            self.set(n);
            true
        // Older: refuse if outside the window or already seen, else mark it seen.
        } else if self.top - n >= Self::SIZE || self.is_set(n) {
            false
        } else {
            self.set(n);
            true
        }
    }
}

/// The keys of one connection, from one side's point of view.
pub struct Session {
    send: ChaCha20Poly1305,
    recv: ChaCha20Poly1305,
    counter: AtomicU64,
    window: Mutex<ReplayWindow>,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session").finish_non_exhaustive()
    }
}

/// The handshake proof uses the last counter, which data never reaches.
const PROOF_COUNTER: u64 = u64::MAX;

// The "number used once" that ChaCha20 needs: the packet counter. Because
// the counter never repeats within a session, neither does the nonce.
fn nonce(counter: u64) -> Nonce {
    let mut n = [0u8; 12];
    n[4..].copy_from_slice(&counter.to_be_bytes());
    Nonce::from(n)
}

impl Session {
    fn new(send: [u8; 32], recv: [u8; 32]) -> Self {
        Self {
            send: ChaCha20Poly1305::new(Key::from_slice(&send)),
            recv: ChaCha20Poly1305::new(Key::from_slice(&recv)),
            counter: AtomicU64::new(1),
            window: Mutex::new(ReplayWindow::new()),
        }
    }

    /// Encrypt one datagram.
    pub fn seal(&self, plain: &[u8]) -> Vec<u8> {
        // Each packet gets the next counter, written in clear in front so the other
        // side knows which nonce to use. The counter and tag byte are also checked
        // ("aad"), so changing them breaks the seal.
        let n = self.counter.fetch_add(1, Ordering::Relaxed);
        let mut head = [0u8; 9];
        head[0] = SEALED;
        head[1..].copy_from_slice(&n.to_be_bytes());
        let body = self.send.encrypt(&nonce(n), Payload { msg: plain, aad: &head }).expect("encryption cannot fail");
        let mut out = Vec::with_capacity(9 + body.len());
        out.extend_from_slice(&head);
        out.extend_from_slice(&body);
        out
    }

    /// Decrypt one datagram; `None` if forged, damaged or replayed.
    pub fn open(&self, d: &[u8]) -> Option<Vec<u8>> {
        // Too short or not a sealed packet: not ours.
        if d.len() < OVERHEAD || d[0] != SEALED {
            return None;
        }
        let n = u64::from_be_bytes(d[1..9].try_into().ok()?);
        // The proof counter is never used for data; refuse it here.
        if n == PROOF_COUNTER {
            return None;
        }
        let plain = self.recv.decrypt(&nonce(n), Payload { msg: &d[9..], aad: &d[..9] }).ok()?;
        // Only after the packet proved genuine may it move the window.
        self.window.lock().ok()?.accept(n).then_some(plain)
    }

    // A tag over nothing, proving the sender derived the same keys (used once
    // in the handshake).
    fn proof(&self) -> Vec<u8> {
        self.send.encrypt(&nonce(PROOF_COUNTER), Payload { msg: &[], aad: b"aa-proof" }).expect("encrypt")
    }

    fn check_proof(&self, tag: &[u8]) -> bool {
        self.recv.decrypt(&nonce(PROOF_COUNTER), Payload { msg: tag, aad: b"aa-proof" }).is_ok()
    }
}

/// Both directions' keys from the three shared secrets. Every public key
/// is mixed in, so a session belongs to exactly these two computers.
fn derive(dh: [[u8; 32]; 3], keys: [&PublicKeyBytes; 4]) -> ([u8; 32], [u8; 32]) {
    // Mix the three shared secrets and all four public keys through HKDF (a
    // standard key-stretching function) into 64 bytes: the first 32 encrypt
    // viewer -> host, the second 32 host -> viewer.
    let mut ikm = Vec::with_capacity(96);
    for d in &dh {
        ikm.extend_from_slice(d);
    }
    let mut info = Vec::with_capacity(128);
    for k in keys {
        info.extend_from_slice(k);
    }
    let hk = Hkdf::<Sha256>::new(Some(b"anywhere-session-v1"), &ikm);
    let mut okm = [0u8; 64];
    hk.expand(&info, &mut okm).expect("64 bytes is a valid length");
    let (a, b) = okm.split_at(32);
    (a.try_into().expect("32"), b.try_into().expect("32"))
}

// ---------------------------------------------------------------------------
// Connection handshake
// ---------------------------------------------------------------------------

/// The viewer's half-finished handshake.
pub struct Initiator {
    me_secret: StaticSecret,
    me: PublicKeyBytes,
    eph: StaticSecret,
    eph_pub: PublicKeyBytes,
}

impl std::fmt::Debug for Initiator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Initiator").finish_non_exhaustive()
    }
}

/// What came back from the host.
#[derive(Debug)]
#[allow(clippy::large_enum_variant)] // made once per connection
pub enum Answer {
    /// Connected: the host's key (check it is one we paired with) and the session.
    Connected(PublicKeyBytes, Session),
    /// The host doesn't know us: pair first.
    NotPaired { host_key: PublicKeyBytes, host_name: String },
}

impl Initiator {
    /// Start a connection: the datagram to send (repeat it until answered).
    pub fn start(id: &Identity) -> (Self, Vec<u8>) {
        // Fresh one-time key for this connection only ("forward secrecy": stealing
        // the permanent keys later does not unlock recorded sessions).
        let eph = StaticSecret::from(random_bytes());
        let eph_pub = PublicKey::from(&eph).to_bytes();
        let mut d = Vec::with_capacity(65);
        d.push(INIT);
        d.extend_from_slice(&id.public);
        d.extend_from_slice(&eph_pub);
        (Self { me_secret: id.secret.clone(), me: id.public, eph, eph_pub }, d)
    }

    /// Read the host's answer. `None`: not an answer to us (ignore it).
    pub fn finish(&self, d: &[u8]) -> Option<Answer> {
        // The first byte says which answer this is.
        match *d.first()? {
            UNKNOWN => {
                Some(Answer::NotPaired { host_key: key_at(d, 1)?, host_name: clean_name(d.get(33..).unwrap_or(&[])) })
            }
            RESP if d.len() == 1 + 64 + 16 => {
                let host = key_at(d, 1)?;
                let host_eph = key_at(d, 33)?;
                // Three combinations of one-time and permanent keys. Only the real host
                // (who has its permanent secret) can compute the same three.
                let dh = [
                    self.eph.diffie_hellman(&PublicKey::from(host_eph)).to_bytes(),
                    self.eph.diffie_hellman(&PublicKey::from(host)).to_bytes(),
                    self.me_secret.diffie_hellman(&PublicKey::from(host_eph)).to_bytes(),
                ];
                let (v2h, h2v) = derive(dh, [&self.me, &self.eph_pub, &host, &host_eph]);
                let s = Session::new(v2h, h2v);
                // Accept only if the host's proof checks out with these keys.
                s.check_proof(&d[65..]).then_some(Answer::Connected(host, s))
            }
            _ => None,
        }
    }
}

/// The host's reply to an `INIT`.
#[derive(Debug)]
#[allow(clippy::large_enum_variant)] // made once per connection
pub enum Response {
    /// A paired viewer: send `reply`, keep `session` for this address.
    Accept { viewer: PublicKeyBytes, viewer_eph: PublicKeyBytes, session: Session, reply: Vec<u8> },
    /// Not paired: send `reply` (tells it to pair first).
    Unknown { reply: Vec<u8> },
}

/// Host side of the handshake. `None`: malformed, ignore.
pub fn respond(id: &Identity, name: &str, d: &[u8], is_paired: impl Fn(&PublicKeyBytes) -> bool) -> Option<Response> {
    // Not a well-formed INIT: ignore.
    if d.len() != 65 || d[0] != INIT {
        return None;
    }
    let viewer = key_at(d, 1)?;
    let viewer_eph = key_at(d, 33)?;
    // Unknown viewer: tell it who we are so it can ask to pair.
    if !is_paired(&viewer) {
        let mut reply = vec![UNKNOWN];
        reply.extend_from_slice(&id.public);
        reply.extend_from_slice(&name_bytes(name));
        return Some(Response::Unknown { reply });
    }
    let eph = StaticSecret::from(random_bytes());
    let eph_pub = PublicKey::from(&eph).to_bytes();
    // The same three combinations, from the host's side (mirror image).
    let dh = [
        eph.diffie_hellman(&PublicKey::from(viewer_eph)).to_bytes(),
        id.secret.diffie_hellman(&PublicKey::from(viewer_eph)).to_bytes(),
        eph.diffie_hellman(&PublicKey::from(viewer)).to_bytes(),
    ];
    let (v2h, h2v) = derive(dh, [&viewer, &viewer_eph, &id.public, &eph_pub]);
    // The host sends with the viewer's "receive" key and vice versa.
    let session = Session::new(h2v, v2h);
    let mut reply = Vec::with_capacity(81);
    reply.push(RESP);
    reply.extend_from_slice(&id.public);
    reply.extend_from_slice(&eph_pub);
    reply.extend_from_slice(&session.proof());
    Some(Response::Accept { viewer, viewer_eph, session, reply })
}

// ---------------------------------------------------------------------------
// Pairing
// ---------------------------------------------------------------------------

type HmacSha256 = Hmac<Sha256>;

fn pair_proof(secret: &[u8], label: &[u8], viewer: &PublicKeyBytes, host: &PublicKeyBytes) -> [u8; 32] {
    // A keyed hash binding the pairing secret to both public keys, so a
    // meddler in the middle cannot swap in its own key.
    let mut m = <HmacSha256 as Mac>::new_from_slice(secret).expect("any key length");
    m.update(label);
    m.update(viewer);
    m.update(host);
    m.finalize().into_bytes().into()
}

fn spake_ids() -> (SpakeId, SpakeId) {
    (SpakeId::new(b"anywhere-viewer"), SpakeId::new(b"anywhere-host"))
}

/// The viewer's side of pairing.
pub struct PairInitiator {
    spake: Spake2<Ed25519Group>,
    me: PublicKeyBytes,
}

impl std::fmt::Debug for PairInitiator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PairInitiator").finish_non_exhaustive()
    }
}

/// Result of a successful pairing on the viewer.
#[derive(Debug, Clone)]
pub struct Paired {
    pub host_key: PublicKeyBytes,
    pub host_name: String,
    /// Send this (repeat until `PAIR_OK`).
    pub confirm: Vec<u8>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PairError {
    #[error("that code is not right")]
    WrongCode,
    #[error("unexpected answer")]
    Malformed,
}

impl PairInitiator {
    pub fn start(code: &str, id: &Identity, name: &str) -> (Self, Vec<u8>) {
        // Step 1 (viewer): SPAKE2 message made from the typed code, plus our public
        // key and name.
        let (v, h) = spake_ids();
        let (spake, msg) = Spake2::<Ed25519Group>::start_a(&Password::new(normalize_code(code).as_bytes()), &v, &h);
        let mut d = Vec::with_capacity(1 + SPAKE_LEN + 32 + NAME_MAX);
        d.push(PAIR1);
        d.extend_from_slice(&msg);
        d.extend_from_slice(&id.public);
        d.extend_from_slice(&name_bytes(name));
        (Self { spake, me: id.public }, d)
    }

    /// Read `PAIR2`. Consumes the exchange (it can only be finished once).
    pub fn finish(self, d: &[u8]) -> Result<Paired, PairError> {
        // Step 3 (viewer): read the host's answer and check its proof. A wrong
        // code gives a different secret, so the proof does not match.
        if d.len() < 1 + SPAKE_LEN + 64 || d[0] != PAIR2 {
            return Err(PairError::Malformed);
        }
        let host_key = key_at(d, 1 + SPAKE_LEN).ok_or(PairError::Malformed)?;
        let proof = &d[1 + SPAKE_LEN + 32..1 + SPAKE_LEN + 64];
        let secret = self.spake.finish(&d[1..=SPAKE_LEN]).map_err(|_| PairError::Malformed)?;
        let expect = pair_proof(&secret, b"host", &self.me, &host_key);
        if !subtle_eq(&expect, proof) {
            return Err(PairError::WrongCode);
        }
        // Our own proof back, so the host knows we had the code too.
        let mut confirm = vec![PAIR3];
        confirm.extend_from_slice(&pair_proof(&secret, b"viewer", &self.me, &host_key));
        Ok(Paired { host_key, host_name: clean_name(&d[1 + SPAKE_LEN + 64..]), confirm })
    }
}

/// Host side: what it keeps between `PAIR2` and the viewer's `PAIR3`.
#[derive(Clone)]
pub struct PendingPair {
    secret: Vec<u8>,
    pub viewer_key: PublicKeyBytes,
    pub viewer_name: String,
    /// The `PAIR1` this answers (a repeat gets the same reply).
    pub request: Vec<u8>,
    pub reply: Vec<u8>,
}

impl std::fmt::Debug for PendingPair {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PendingPair").field("viewer_name", &self.viewer_name).finish_non_exhaustive()
    }
}

/// Host: answer a `PAIR1` with the current code. `None`: malformed.
pub fn pair_respond(code: &str, id: &Identity, name: &str, d: &[u8]) -> Option<PendingPair> {
    // Step 2 (host): answer with its own SPAKE2 message, public key, proof
    // and name, using the code it is showing right now.
    if d.len() < 1 + SPAKE_LEN + 32 || d[0] != PAIR1 {
        return None;
    }
    let viewer_key = key_at(d, 1 + SPAKE_LEN)?;
    let (v, h) = spake_ids();
    let (spake, msg) = Spake2::<Ed25519Group>::start_b(&Password::new(normalize_code(code).as_bytes()), &v, &h);
    let secret = spake.finish(&d[1..=SPAKE_LEN]).ok()?;
    let mut reply = Vec::with_capacity(1 + SPAKE_LEN + 64 + NAME_MAX);
    reply.push(PAIR2);
    reply.extend_from_slice(&msg);
    reply.extend_from_slice(&id.public);
    reply.extend_from_slice(&pair_proof(&secret, b"host", &viewer_key, &id.public));
    reply.extend_from_slice(&name_bytes(name));
    Some(PendingPair {
        secret,
        viewer_key,
        viewer_name: clean_name(&d[1 + SPAKE_LEN + 32..]),
        request: d.to_vec(),
        reply,
    })
}

/// Host: is this `PAIR3` the right proof (so the viewer typed our code)?
pub fn pair_confirmed(p: &PendingPair, host: &PublicKeyBytes, d: &[u8]) -> bool {
    // Step 4 (host): the viewer's proof matches only if it typed our code.
    d.len() == 33 && d[0] == PAIR3 && subtle_eq(&pair_proof(&p.secret, b"viewer", &p.viewer_key, host), &d[1..])
}

/// Constant-time comparison (no timing hints about how much matched).
fn subtle_eq(a: &[u8], b: &[u8]) -> bool {
    // Compares every byte whatever happens, so the time taken reveals nothing.
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
#[allow(clippy::many_single_char_names)]
mod tests {
    use super::*;

    fn connect(viewer: &Identity, host: &Identity, paired: bool) -> Option<(Session, Session)> {
        let (init, d) = Initiator::start(viewer);
        match respond(host, "Mac", &d, |_| paired)? {
            Response::Accept { viewer: v, session, reply, .. } => {
                assert_eq!(v, viewer.public());
                match init.finish(&reply)? {
                    Answer::Connected(h, s) => {
                        assert_eq!(h, host.public());
                        Some((s, session))
                    }
                    Answer::NotPaired { .. } => None,
                }
            }
            Response::Unknown { reply } => match init.finish(&reply)? {
                Answer::NotPaired { host_key, host_name } => {
                    assert_eq!(host_key, host.public());
                    assert_eq!(host_name, "Mac");
                    None
                }
                Answer::Connected(..) => panic!("unknown viewer connected"),
            },
        }
    }

    #[test]
    fn paired_computers_talk_privately_both_ways() {
        let (v, h) = (Identity::generate(), Identity::generate());
        let (vs, hs) = connect(&v, &h, true).expect("connects");
        let sealed = vs.seal(b"hello host");
        assert!(!sealed.windows(5).any(|w| w == b"hello"), "plaintext visible");
        assert_eq!(hs.open(&sealed).unwrap(), b"hello host");
        assert_eq!(vs.open(&hs.seal(b"hi viewer")).unwrap(), b"hi viewer");
        assert_eq!(sealed.len(), b"hello host".len() + OVERHEAD);
    }

    #[test]
    fn strangers_are_told_to_pair() {
        let (v, h) = (Identity::generate(), Identity::generate());
        assert!(connect(&v, &h, false).is_none());
    }

    #[test]
    fn replays_tampering_and_other_sessions_are_dropped() {
        let (v, h) = (Identity::generate(), Identity::generate());
        let (vs, hs) = connect(&v, &h, true).unwrap();
        let a = vs.seal(b"press A");
        assert!(hs.open(&a).is_some());
        assert!(hs.open(&a).is_none(), "replay accepted");
        let mut b = vs.seal(b"press B");
        let last = b.len() - 1;
        b[last] ^= 1;
        assert!(hs.open(&b).is_none(), "tampered packet accepted");
        // A second connection between the same two computers has new keys.
        let (vs2, _) = connect(&v, &h, true).unwrap();
        assert!(hs.open(&vs2.seal(b"x")).is_none());
        // Someone else's keys can't fake the host's answer.
        let (init, d) = Initiator::start(&v);
        let Some(Response::Accept { mut reply, .. }) = respond(&h, "Mac", &d, |_| true) else { panic!() };
        reply[70] ^= 1;
        assert!(init.finish(&reply).is_none());
    }

    #[test]
    fn late_and_reordered_packets_still_open() {
        let (v, h) = (Identity::generate(), Identity::generate());
        let (vs, hs) = connect(&v, &h, true).unwrap();
        let pkts: Vec<_> = (0..3000).map(|i: u32| vs.seal(&i.to_be_bytes())).collect();
        assert!(hs.open(&pkts[10]).is_some());
        assert!(hs.open(&pkts[5]).is_some(), "slightly late packet dropped");
        assert!(hs.open(&pkts[2999]).is_some());
        assert!(hs.open(&pkts[20]).is_none(), "ancient packet accepted");
        assert!(hs.open(&pkts[2000]).is_some());
    }

    #[test]
    fn pairing_with_the_right_code() {
        let (v, h) = (Identity::generate(), Identity::generate());
        let (pi, p1) = PairInitiator::start("482 913", &v, "Maitrik's PC");
        let pending = pair_respond("482913", &h, "Mac mini", &p1).unwrap();
        assert_eq!(pending.viewer_name, "Maitrik's PC");
        let done = pi.finish(&pending.reply).unwrap();
        assert_eq!(done.host_key, h.public());
        assert_eq!(done.host_name, "Mac mini");
        assert!(pair_confirmed(&pending, &h.public(), &done.confirm));
    }

    #[test]
    fn pairing_with_a_wrong_code_fails_on_both_sides() {
        let (v, h) = (Identity::generate(), Identity::generate());
        let (pi, p1) = PairInitiator::start("111111", &v, "PC");
        let pending = pair_respond("222222", &h, "Mac", &p1).unwrap();
        assert_eq!(pi.finish(&pending.reply).unwrap_err(), PairError::WrongCode);
        // A viewer that ignores the failure and sends a made-up proof.
        let mut fake = vec![PAIR3];
        fake.extend_from_slice(&[0u8; 32]);
        assert!(!pair_confirmed(&pending, &h.public(), &fake));
    }

    #[test]
    fn hex_and_codes() {
        let k = Identity::generate().public();
        assert_eq!(from_hex(&to_hex(&k)), Some(k));
        assert_eq!(from_hex("zz"), None);
        let c = new_code();
        assert_eq!(c.len(), 6);
        assert!(c.chars().all(|ch| ch.is_ascii_digit()));
        assert_eq!(fingerprint(&k).len(), 9);
    }

    #[test]
    fn garbage_never_panics() {
        let h = Identity::generate();
        let (init, _) = Initiator::start(&Identity::generate());
        for len in 0..120 {
            for first in [INIT, RESP, UNKNOWN, PAIR1, PAIR2, PAIR3, SEALED, 0] {
                let mut d = vec![7u8; len];
                if let Some(f) = d.first_mut() {
                    *f = first;
                }
                let _ = respond(&h, "x", &d, |_| true);
                let _ = init.finish(&d);
                let _ = pair_respond("1", &h, "x", &d);
                let (pi, _) = PairInitiator::start("1", &h, "x");
                let _ = pi.finish(&d);
            }
        }
    }
}
