//! Pairing from the connecting side: type the code once, and the two
//! computers remember each other (see `aa_core::secure` for how).

use std::net::SocketAddr;
use std::time::Duration;

use aa_core::secure::{self, PairInitiator, PublicKeyBytes};
use tokio::net::UdpSocket;

/// The other computer, now paired.
#[derive(Debug, Clone)]
pub struct PairedWith {
    pub name: String,
    pub key: PublicKeyBytes,
}

/// Pair with the computer at `addr` using the code shown on its screen.
pub async fn pair(addr: SocketAddr, code: &str) -> anyhow::Result<PairedWith> {
    let code = secure::normalize_code(code);
    anyhow::ensure!(code.len() == 6, "the code has six digits");
    let bind: SocketAddr = if addr.is_ipv6() { "[::]:0" } else { "0.0.0.0:0" }.parse()?;
    let socket = UdpSocket::bind(bind).await?;
    socket.connect(addr).await?;
    let id = crate::trust::viewer_identity();
    let (exchange, request) = PairInitiator::start(&code, &id, &crate::trust::computer_name());
    let mut buf = vec![0u8; 2048];

    // 1. Our half of the code exchange until the other computer answers.
    let reply = tokio::time::timeout(Duration::from_secs(12), async {
        let mut resend = tokio::time::interval(Duration::from_millis(500));
        loop {
            tokio::select! {
                _ = resend.tick() => { let _ = socket.send(&request).await; }
                r = socket.recv(&mut buf) => {
                    let Ok(n) = r else { continue };
                    match buf.first() {
                        Some(&secure::PAIR2) => return Ok(buf[..n].to_vec()),
                        Some(&secure::PAIR_FAIL) => {
                            let why = String::from_utf8_lossy(&buf[1..n]).into_owned();
                            if why != "wait" {
                                anyhow::bail!("{why}");
                            }
                            // Too many tries just now: it asked us to slow down.
                        }
                        _ => {}
                    }
                }
            }
        }
    })
    .await
    .map_err(|_| anyhow::anyhow!("no answer from {addr}; is Anywhere sharing there?"))??;

    let done = exchange.finish(&reply).map_err(|e| match e {
        secure::PairError::WrongCode => {
            anyhow::anyhow!("That code isn't right. Check the Share page on the other computer.")
        }
        secure::PairError::Malformed => anyhow::anyhow!("unexpected answer from {addr}"),
    })?;

    // 2. Prove we knew the code too, until it says it remembered us.
    tokio::time::timeout(Duration::from_secs(8), async {
        let mut resend = tokio::time::interval(Duration::from_millis(400));
        loop {
            tokio::select! {
                _ = resend.tick() => { let _ = socket.send(&done.confirm).await; }
                r = socket.recv(&mut buf) => {
                    let Ok(n) = r else { continue };
                    match buf.first() {
                        Some(&secure::PAIR_OK) => return Ok(()),
                        Some(&secure::PAIR_FAIL) => {
                            anyhow::bail!("{}", String::from_utf8_lossy(&buf[1..n]));
                        }
                        _ => {}
                    }
                }
            }
        }
    })
    .await
    .map_err(|_| anyhow::anyhow!("the other computer stopped answering while pairing"))??;

    crate::trust::add_paired_host(&done.host_name, &done.host_key, Some(&addr.to_string()));
    tracing::info!(name = done.host_name, key = secure::fingerprint(&done.host_key), "paired");
    Ok(PairedWith { name: done.host_name, key: done.host_key })
}

/// Ask whoever is at `addr` for its name and key (no pairing, no connection).
pub async fn probe(addr: SocketAddr) -> anyhow::Result<crate::discover::Found> {
    use aa_core::control::ControlMessage;
    use aa_core::wire::{self, Header, Kind, Packet};
    let bind: SocketAddr = if addr.is_ipv6() { "[::]:0" } else { "0.0.0.0:0" }.parse()?;
    let socket = UdpSocket::bind(bind).await?;
    socket.connect(addr).await?;
    let payload = ControlMessage::Discover.encode();
    let mut ask = bytes::BytesMut::new();
    Header { kind: Kind::Control, flags: 0, seq: 0, frame_id: 0, slice_index: 0, slice_count: 1 }.write(&mut ask);
    ask.extend_from_slice(&payload);
    let mut buf = vec![0u8; wire::MAX_DATAGRAM * 2];
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut resend = tokio::time::interval(Duration::from_millis(400));
        loop {
            tokio::select! {
                _ = resend.tick() => { let _ = socket.send(&ask).await; }
                r = socket.recv(&mut buf) => {
                    let Ok(n) = r else { continue };
                    let Ok(p) = Packet::parse(bytes::Bytes::copy_from_slice(&buf[..n])) else { continue };
                    if p.header.kind != Kind::Control { continue }
                    if let Ok(ControlMessage::Here { name, key, .. }) = ControlMessage::decode(&p.payload) {
                        return crate::discover::Found {
                            name,
                            addr,
                            key: secure::from_hex(&key),
                            addrs: vec![addr],
                        };
                    }
                }
            }
        }
    })
    .await
    .map_err(|_| anyhow::anyhow!("nothing answered at {addr}; is Anywhere sharing there?"))
}
