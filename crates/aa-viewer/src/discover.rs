//! Finding the host without typing its address.
//!
//! Home routers hand out a fresh address whenever a machine reconnects, so
//! "192.168.1.3" is true for a day and wrong the next. Instead the viewer
//! broadcasts a tiny `Discover` message to the whole local network on the
//! host's normal port, and every host answers with its computer name. No
//! extra port, no extra firewall rule, no service to install: the host is
//! already listening there.
//!
//! Why not mDNS/Bonjour: it is the "proper" answer but needs a resolver on
//! both sides and a multicast group; for a LAN with a handful of machines a
//! broadcast on a port we already own does the same job in 60 lines.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

use aa_core::control::ControlMessage;
use aa_core::wire::{self, Header, Kind, Packet, SeqCounter};
use bytes::{Bytes, BytesMut};
use tokio::net::UdpSocket;

/// The host's default port; discovery uses the same one.
pub const DEFAULT_PORT: u16 = 7700;
/// How long to listen for answers. Hosts reply within a millisecond; the
/// wait is for Wi-Fi, which can hold a broadcast for a while.
const LISTEN: Duration = Duration::from_millis(1500);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub name: String,
    pub addr: SocketAddr,
}

/// Broadcast and collect every host that answers, nearest first.
pub async fn find_hosts(port: u16) -> anyhow::Result<Vec<Found>> {
    let socket = UdpSocket::bind("0.0.0.0:0").await?;
    socket.set_broadcast(true)?;
    let seq = SeqCounter::default();

    let payload = ControlMessage::Discover.encode();
    let mut out = BytesMut::with_capacity(wire::HEADER_LEN + payload.len());
    Header { kind: Kind::Control, flags: 0, seq: seq.take(), frame_id: 0, slice_index: 0, slice_count: 1 }
        .write(&mut out);
    out.extend_from_slice(&payload);
    let out = out.freeze();

    // The all-ones broadcast reaches every interface; the subnet broadcast
    // is a fallback for routers that drop the former.
    let mut targets = vec![SocketAddr::new(IpAddr::V4(Ipv4Addr::BROADCAST), port)];
    if let Some(local) = local_ipv4().await {
        let [a, b, c, _] = local.octets();
        targets.push(SocketAddr::new(IpAddr::V4(Ipv4Addr::new(a, b, c, 255)), port));
    }

    let mut found: Vec<Found> = Vec::new();
    let mut buf = vec![0u8; wire::MAX_DATAGRAM * 2];
    let deadline = tokio::time::Instant::now() + LISTEN;
    let mut resend = tokio::time::interval(Duration::from_millis(400));

    loop {
        tokio::select! {
            _ = resend.tick() => {
                for t in &targets {
                    if let Err(e) = socket.send_to(&out, t).await {
                        tracing::debug!("discover send to {t}: {e}");
                    }
                }
            }
            recv = socket.recv_from(&mut buf) => {
                let (n, from) = recv?;
                let Ok(packet) = Packet::parse(Bytes::copy_from_slice(&buf[..n])) else { continue };
                if packet.header.kind != Kind::Control {
                    continue;
                }
                if let Ok(ControlMessage::Here { name }) = ControlMessage::decode(&packet.payload) {
                    if !found.iter().any(|f| f.addr == from) {
                        tracing::info!(%from, name, "found host");
                        found.push(Found { name, addr: from });
                    }
                }
            }
            () = tokio::time::sleep_until(deadline) => break,
        }
    }
    Ok(found)
}

/// Our own LAN address, learned by asking the OS which interface it would
/// use to reach the internet. Nothing is actually sent.
async fn local_ipv4() -> Option<Ipv4Addr> {
    let probe = UdpSocket::bind("0.0.0.0:0").await.ok()?;
    probe.connect("8.8.8.8:53").await.ok()?;
    match probe.local_addr().ok()?.ip() {
        IpAddr::V4(v4) if !v4.is_loopback() && !v4.is_unspecified() => Some(v4),
        _ => None,
    }
}

/// Turn whatever the user typed into an address: a full `ip:port`, a bare
/// IP (default port), or a computer name / nothing (discover).
pub async fn resolve(input: Option<&str>) -> anyhow::Result<SocketAddr> {
    if let Some(s) = input {
        if let Ok(addr) = s.parse::<SocketAddr>() {
            return Ok(addr);
        }
        if let Ok(ip) = s.parse::<IpAddr>() {
            return Ok(SocketAddr::new(ip, DEFAULT_PORT));
        }
    }
    tracing::info!("looking for hosts on the local network…");
    let hosts = find_hosts(DEFAULT_PORT).await?;
    let wanted = input.map(str::to_ascii_lowercase);
    let pick = match &wanted {
        Some(w) => hosts.iter().find(|h| h.name.to_ascii_lowercase().contains(w)),
        None => hosts.first(),
    };
    match pick {
        Some(h) => {
            if hosts.len() > 1 && wanted.is_none() {
                let names: Vec<_> = hosts.iter().map(|h| format!("{} ({})", h.name, h.addr)).collect();
                tracing::info!("several hosts answered: {}; using the first (pass a name to choose)", names.join(", "));
            }
            tracing::info!(name = h.name, addr = %h.addr, "using host");
            Ok(h.addr)
        }
        None if hosts.is_empty() => anyhow::bail!(
            "no host found on the local network; is aa-host running on the same Wi-Fi/LAN? \
             (you can still pass its address, e.g. 192.168.1.5:7700)"
        ),
        None => {
            let names: Vec<_> = hosts.iter().map(|h| h.name.clone()).collect();
            anyhow::bail!("no host named like {:?}; found: {}", input.unwrap_or(""), names.join(", "))
        }
    }
}
