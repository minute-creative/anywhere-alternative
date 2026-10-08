//! The host's network socket, with pairing and encryption built in.
//!
//! The rest of the host reads and writes plain packets exactly as before;
//! this layer seals everything to a connected computer, opens what comes
//! back, answers handshakes and pairing requests itself, and lets only one
//! kind of unencrypted packet through: "who's there?" (`Discover`), so
//! computers can still find this one by name.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use aa_core::control::ControlMessage;
use aa_core::secure::{self, Identity, PendingPair, PublicKeyBytes, Response, Session};
use aa_core::wire::{Kind, Packet};
use bytes::Bytes;
use tokio::net::UdpSocket;

/// A connected computer's keys.
struct Peer {
    session: Arc<Session>,
    viewer: PublicKeyBytes,
    viewer_eph: PublicKeyBytes,
    reply: Vec<u8>,
    last: Instant,
}

/// Wrong codes in a row before the code changes (someone guessing).
const MAX_WRONG: u32 = 5;
/// One new pairing attempt per this long, from anyone: guessing a six-digit
/// code one try at a time would take weeks.
const PAIR_SPACING: Duration = Duration::from_secs(2);

struct Pairing {
    code: String,
    /// Pairing requests answered, waiting for proof; `true` once proven.
    pending: HashMap<SocketAddr, (PendingPair, Instant, bool)>,
    last_attempt: Option<Instant>,
    wrong: u32,
}

pub struct Net {
    sock: UdpSocket,
    id: Identity,
    name: String,
    peers: Mutex<HashMap<SocketAddr, Peer>>,
    pairing: Mutex<Pairing>,
    last_reset: Mutex<HashMap<SocketAddr, Instant>>,
}

impl std::fmt::Debug for Net {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Net").field("name", &self.name).finish_non_exhaustive()
    }
}

impl Net {
    pub fn bind(addr: SocketAddr) -> anyhow::Result<Self> {
        let sock = crate::udp::bind(addr)?;
        let id = aa_platform::trust::host_identity();
        let code = aa_platform::trust::pair_code();
        tracing::info!(
            key = secure::fingerprint(&id.public()),
            "pairing code for new computers: {}",
            aa_platform::trust::spaced(&code)
        );
        Ok(Self {
            sock,
            name: aa_platform::trust::computer_name(),
            id,
            peers: Mutex::default(),
            pairing: Mutex::new(Pairing { code, pending: HashMap::new(), last_attempt: None, wrong: 0 }),
            last_reset: Mutex::default(),
        })
    }

    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.sock.local_addr()
    }

    pub fn public_key_hex(&self) -> String {
        secure::to_hex(&self.id.public())
    }

    /// The paired name of whoever is at `addr` (for the log and the app).
    pub fn peer_name(&self, addr: SocketAddr) -> Option<String> {
        let key = self.peers.lock().ok()?.get(&addr)?.viewer;
        aa_platform::trust::paired_viewer_name(&key)
    }

    /// Send one packet: sealed if `to` is a connected computer.
    pub async fn send_to(&self, buf: &[u8], to: SocketAddr) -> std::io::Result<usize> {
        let session = self.peers.lock().ok().and_then(|p| p.get(&to).map(|p| Arc::clone(&p.session)));
        match session {
            Some(s) => self.sock.send_to(&s.seal(buf), to).await,
            None => self.sock.send_to(buf, to).await,
        }
    }

    /// The next plain packet for the host, from a paired computer (or a
    /// "who's there?" from anyone). Handshakes are answered on the way.
    pub async fn recv_from(&self, buf: &mut [u8]) -> std::io::Result<(usize, SocketAddr)> {
        let mut raw = vec![0u8; buf.len() + secure::OVERHEAD];
        loop {
            let (n, from) = self.sock.recv_from(&mut raw).await?;
            let d = &raw[..n];
            let Some(&first) = d.first() else { continue };
            match first {
                secure::SEALED => {
                    let session = self.peers.lock().ok().and_then(|mut p| {
                        p.get_mut(&from).map(|p| {
                            p.last = Instant::now();
                            Arc::clone(&p.session)
                        })
                    });
                    match session.and_then(|s| s.open(d)) {
                        Some(plain) if plain.len() <= buf.len() => {
                            buf[..plain.len()].copy_from_slice(&plain);
                            return Ok((plain.len(), from));
                        }
                        Some(_) => {}
                        None => self.maybe_reset(from).await,
                    }
                }
                secure::INIT => self.handshake(d, from).await,
                secure::PAIR1 => self.pair_request(d, from).await,
                secure::PAIR3 => self.pair_confirm(d, from).await,
                _ if is_discover(d) && d.len() <= buf.len() => {
                    buf[..n].copy_from_slice(d);
                    return Ok((n, from));
                }
                _ => tracing::trace!(%from, "dropped an unencrypted packet"),
            }
        }
    }

    /// A sealed packet we can't open, from an address we have no session
    /// with: probably a viewer still talking to the host from before it
    /// restarted. Tell it (at most once a second) so it reconnects at once
    /// instead of waiting out a timeout.
    async fn maybe_reset(&self, from: SocketAddr) {
        if self.peers.lock().is_ok_and(|p| p.contains_key(&from)) {
            return; // a damaged or replayed packet in a live session: just drop it
        }
        let due = self.last_reset.lock().is_ok_and(|mut r| {
            r.retain(|_, t| t.elapsed() < Duration::from_secs(30));
            let due = r.get(&from).map_or(true, |t| t.elapsed() >= Duration::from_secs(1));
            if due {
                r.insert(from, Instant::now());
            }
            due
        });
        if due {
            let _ = self.sock.send_to(&[secure::RESET], from).await;
        }
    }

    async fn handshake(&self, d: &[u8], from: SocketAddr) {
        // A repeat of the INIT we already answered (our answer was lost):
        // the same answer again, or the two sides would hold different keys.
        let repeat =
            self.peers.lock().ok().and_then(|p| {
                p.get(&from).filter(|p| d.get(33..65) == Some(&p.viewer_eph[..])).map(|p| p.reply.clone())
            });
        if let Some(r) = repeat {
            let _ = self.sock.send_to(&r, from).await;
            return;
        }
        let Some(resp) = secure::respond(&self.id, &self.name, d, aa_platform::trust::is_paired_viewer) else {
            return;
        };
        match resp {
            Response::Accept { viewer, viewer_eph, session, reply } => {
                let name = aa_platform::trust::paired_viewer_name(&viewer).unwrap_or_default();
                tracing::info!(%from, name, "paired computer says hello");
                if let Ok(mut p) = self.peers.lock() {
                    // Forget sessions nobody has used for two minutes.
                    p.retain(|_, x| x.last.elapsed() < Duration::from_secs(120));
                    p.insert(
                        from,
                        Peer {
                            session: Arc::new(session),
                            viewer,
                            viewer_eph,
                            reply: reply.clone(),
                            last: Instant::now(),
                        },
                    );
                }
                let _ = self.sock.send_to(&reply, from).await;
            }
            Response::Unknown { reply } => {
                tracing::info!(%from, "a computer that isn't paired tried to connect; it needs the pairing code");
                let _ = self.sock.send_to(&reply, from).await;
            }
        }
    }

    async fn pair_request(&self, d: &[u8], from: SocketAddr) {
        let answer: Vec<u8> = {
            let Ok(mut p) = self.pairing.lock() else { return };
            p.pending.retain(|_, (_, t, _)| t.elapsed() < Duration::from_secs(60));
            if let Some((pp, _, _)) = p.pending.get(&from).filter(|(pp, _, _)| pp.request == d) {
                pp.reply.clone() // the same request again: same answer
            } else if p.last_attempt.is_some_and(|t| t.elapsed() < PAIR_SPACING) {
                fail("wait")
            } else {
                p.last_attempt = Some(Instant::now());
                // The app may have made a new code (its "new code" button).
                p.code = aa_platform::trust::pair_code();
                let code = p.code.clone();
                match secure::pair_respond(&code, &self.id, &self.name, d) {
                    Some(pp) => {
                        // Counted as wrong until the viewer proves otherwise.
                        p.wrong += 1;
                        if p.wrong > MAX_WRONG {
                            p.wrong = 0;
                            p.code = aa_platform::trust::new_pair_code();
                            tracing::warn!(
                                "several wrong pairing codes in a row; the code changed to {}",
                                aa_platform::trust::spaced(&p.code)
                            );
                        }
                        let r = pp.reply.clone();
                        p.pending.insert(from, (pp, Instant::now(), false));
                        r
                    }
                    None => return,
                }
            }
        };
        let _ = self.sock.send_to(&answer, from).await;
    }

    async fn pair_confirm(&self, d: &[u8], from: SocketAddr) {
        let ok = {
            let Ok(mut p) = self.pairing.lock() else { return };
            let Some((pp, _, done)) = p.pending.get(&from).cloned() else { return };
            let right = secure::pair_confirmed(&pp, &self.id.public(), d);
            if right && !done {
                aa_platform::trust::add_paired_viewer(&pp.viewer_name, &pp.viewer_key);
                if let Some(e) = p.pending.get_mut(&from) {
                    e.2 = true; // a repeat of this proof (our OK was lost) gets OK again
                }
                p.wrong = 0;
                // A used code is spent: the next computer gets a new one.
                p.code = aa_platform::trust::new_pair_code();
                tracing::info!(
                    name = pp.viewer_name,
                    "paired with a new computer; it can now connect without a code. New pairing code: {}",
                    aa_platform::trust::spaced(&p.code)
                );
            }
            right
        };
        let msg = if ok { vec![secure::PAIR_OK] } else { fail("That code isn't right.") };
        let _ = self.sock.send_to(&msg, from).await;
    }
}

fn fail(why: &str) -> Vec<u8> {
    let mut v = vec![secure::PAIR_FAIL];
    v.extend_from_slice(why.as_bytes());
    v
}

/// Only "who's there?" may arrive unencrypted.
fn is_discover(d: &[u8]) -> bool {
    Packet::parse(Bytes::copy_from_slice(d)).is_ok_and(|p| {
        p.header.kind == Kind::Control && matches!(ControlMessage::decode(&p.payload), Ok(ControlMessage::Discover))
    })
}
