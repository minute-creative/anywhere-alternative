//! UDP socket creation with buffers sized for video, not for DNS.
//!
//! Default kernel UDP buffers are ~200 KB. One 4K frame can be 300 KB of
//! datagrams arriving in a burst; without a bigger buffer the kernel drops
//! them before we ever see them and it looks like network loss.

use std::net::SocketAddr;

use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::UdpSocket;

/// 8 MB: comfortably more than one frame at the bitrates we care about.
const BUFFER_BYTES: usize = 8 * 1024 * 1024;

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
    sock.bind(&addr.into())?;
    tracing::debug!(recv = ?sock.recv_buffer_size(), send = ?sock.send_buffer_size(), "udp buffers");
    Ok(UdpSocket::from_std(sock.into())?)
}
