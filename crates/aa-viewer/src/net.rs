//! The viewer's connection: a handshake proves both computers are the ones
//! that paired, then every packet is sealed (see `aa_core::secure`).

use std::net::SocketAddr;
use std::time::Duration;

use aa_core::secure::{self, Answer, Initiator, PublicKeyBytes, Session};
use tokio::net::UdpSocket;

/// The host doesn't know this computer, or this computer doesn't know the
/// host: pairing is needed first. Final (reconnecting won't help).
#[derive(Debug)]
pub struct NotPaired {
    pub host_name: String,
}

impl std::fmt::Display for NotPaired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let n = if self.host_name.is_empty() { "that computer" } else { &self.host_name };
        write!(f, "not paired with {n} yet: pick it in Anywhere and type the code from its Share page")
    }
}

impl std::error::Error for NotPaired {}

pub struct Link {
    sock: UdpSocket,
    session: Session,
}

impl std::fmt::Debug for Link {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Link").finish_non_exhaustive()
    }
}

impl Link {
    /// Prove who we are to `host` and agree on this connection's keys.
    /// Returns the host's key.
    pub async fn connect(bind: SocketAddr, host: SocketAddr) -> anyhow::Result<(Self, PublicKeyBytes)> {
        let sock = crate::udp::bind(bind)?;
        sock.connect(host).await?;
        let id = aa_platform::trust::viewer_identity();
        let (init, hello) = Initiator::start(&id);
        let mut buf = vec![0u8; 2048];
        let answer = tokio::time::timeout(Duration::from_secs(6), async {
            let mut resend = tokio::time::interval(Duration::from_millis(300));
            loop {
                tokio::select! {
                    _ = resend.tick() => { sock.send(&hello).await?; }
                    r = sock.recv(&mut buf) => {
                        let n = r?;
                        if let Some(a) = init.finish(&buf[..n]) {
                            return anyhow::Ok(a);
                        }
                    }
                }
            }
        })
        .await
        .map_err(|_| anyhow::anyhow!("no answer from {host}; is Anywhere sharing there and reachable?"))??;
        match answer {
            Answer::Connected(key, session) => {
                let Some(known) = aa_platform::trust::paired_host(&key) else {
                    return Err(NotPaired { host_name: String::new() }.into());
                };
                tracing::info!(name = known.name, key = secure::fingerprint(&key), "secure connection");
                Ok((Self { sock, session }, key))
            }
            Answer::NotPaired { host_name, .. } => Err(NotPaired { host_name }.into()),
        }
    }

    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.sock.local_addr()
    }

    pub async fn send(&self, buf: &[u8]) -> std::io::Result<usize> {
        self.sock.send(&self.session.seal(buf)).await
    }

    /// The next genuine packet from the host, opened.
    pub async fn recv(&self, buf: &mut [u8]) -> std::io::Result<usize> {
        let mut raw = vec![0u8; buf.len() + secure::OVERHEAD];
        loop {
            let n = self.sock.recv(&mut raw).await?;
            match raw.first() {
                Some(&secure::RESET) => {
                    // The host restarted and lost our keys: reconnect now.
                    return Err(std::io::Error::new(std::io::ErrorKind::ConnectionReset, "the host restarted"));
                }
                Some(&secure::SEALED) => {
                    if let Some(plain) = self.session.open(&raw[..n]).filter(|p| p.len() <= buf.len()) {
                        buf[..plain.len()].copy_from_slice(&plain);
                        return Ok(plain.len());
                    }
                }
                _ => {}
            }
        }
    }
}
