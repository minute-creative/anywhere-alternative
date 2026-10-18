//! UDP socket creation with buffers sized for video, not for DNS.
//!
//! Default kernel UDP buffers are ~200 KB. One 4K frame can be 300 KB of
//! datagrams arriving in a burst; without a bigger buffer the kernel drops
//! them before we ever see them and it looks like network loss.

// Plain summary: every UDP socket (the "mailbox" video travels through) is
// created here so it always gets big buffers. If you suspect packets are
// lost on this computer rather than on the network, look here first.
use std::net::SocketAddr;

use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::UdpSocket;

/// 8 MB: comfortably more than one frame at the bitrates we care about.
const BUFFER_BYTES: usize = 8 * 1024 * 1024;

// Make a UDP socket for `addr` with 8 MB send and receive buffers, ready
// for tokio (non-blocking).
pub fn bind(addr: SocketAddr) -> anyhow::Result<UdpSocket> {
    let domain = if addr.is_ipv6() { Domain::IPV6 } else { Domain::IPV4 };
    let sock = Socket::new(domain, Type::DGRAM, Some(Protocol::UDP))?;
    sock.set_nonblocking(true)?;
    // Best effort: the OS may clamp these to its own maximum, which is fine.
    if let Err(e) = sock.set_recv_buffer_size(BUFFER_BYTES) {
        tracing::debug!("could not enlarge receive buffer: {e}");
    }
    if let Err(e) = sock.set_send_buffer_size(BUFFER_BYTES) {
        tracing::debug!("could not enlarge send buffer: {e}");
    }
    // IPv6 sockets take IPv6 only: IPv4 keeps its own socket, which is what
    // home-network discovery (broadcasts) has always used.
    if addr.is_ipv6() {
        let _ = sock.set_only_v6(true);
    }
    // Bind = claim this address and port so packets sent there reach us.
    sock.bind(&addr.into())?;
    tracing::debug!(recv = ?sock.recv_buffer_size(), send = ?sock.send_buffer_size(), "udp buffers");
    Ok(UdpSocket::from_std(sock.into())?)
}
