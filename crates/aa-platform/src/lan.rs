//! Finding each other on the local network.
//!
//! Two machines can fail to see each other's broadcasts for boring reasons:
//! a PC firewall that drops incoming broadcasts, a router that filters
//! broadcast but passes multicast (or the reverse), a Mac with Wi-Fi and
//! Ethernet both up where the "everyone" broadcast leaves on the wrong one.
//! So discovery uses every route at once:
//!
//! * The host sends a *beacon* every second on every adapter, as both a
//!   subnet broadcast and a multicast to [`BEACON_GROUP`]. Outgoing traffic
//!   is never blocked by the PC's firewall, and the viewer only listens.
//! * The viewer still asks (broadcast `Discover` on every adapter) and
//!   remembers the last host it reached, asking that address directly.

use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};

use socket2::{Domain, Protocol, Socket, Type};

/// Viewers listen for beacons here.
pub const BEACON_PORT: u16 = 7701;
/// Administratively scoped multicast group (never leaves the LAN).
pub const BEACON_GROUP: Ipv4Addr = Ipv4Addr::new(239, 255, 77, 1);

/// One IPv4 network adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Iface {
    pub ip: Ipv4Addr,
    /// Directed broadcast for this adapter's subnet (from its real netmask,
    /// not an assumed /24).
    pub broadcast: Ipv4Addr,
}

/// Every up, non-loopback IPv4 adapter.
pub fn ipv4_interfaces() -> Vec<Iface> {
    let Ok(list) = if_addrs::get_if_addrs() else { return Vec::new() };
    list.into_iter()
        .filter(|i| !i.is_loopback())
        .filter_map(|i| match i.addr {
            if_addrs::IfAddr::V4(v4) => {
                let broadcast =
                    v4.broadcast.unwrap_or_else(|| Ipv4Addr::from(u32::from(v4.ip) | !u32::from(v4.netmask)));
                // Link-local (169.254.x) adapters are dead ends (no DHCP).
                (!v4.ip.is_link_local()).then_some(Iface { ip: v4.ip, broadcast })
            }
            if_addrs::IfAddr::V6(_) => None,
        })
        .collect()
}

/// Send `payload` out of every adapter: its subnet broadcast, the
/// all-ones broadcast and the beacon multicast group, all to `port`.
/// Errors are returned as text for logging (the caller decides how loud).
pub fn send_everywhere(payload: &[u8], port: u16, multicast: bool) -> Vec<String> {
    let mut errors = Vec::new();
    for iface in ipv4_interfaces() {
        let sock = match UdpSocket::bind(SocketAddrV4::new(iface.ip, 0)) {
            Ok(s) => s,
            Err(e) => {
                errors.push(format!("{}: {e}", iface.ip));
                continue;
            }
        };
        let _ = sock.set_broadcast(true);
        let mut targets = vec![iface.broadcast, Ipv4Addr::BROADCAST];
        if multicast {
            let _ = sock.set_multicast_ttl_v4(1);
            targets.push(BEACON_GROUP);
        }
        for t in targets {
            if let Err(e) = sock.send_to(payload, SocketAddrV4::new(t, port)) {
                errors.push(format!("{} -> {t}: {e}", iface.ip));
            }
        }
    }
    errors
}

/// A socket that hears beacons: bound to [`BEACON_PORT`] (shared, so two
/// viewers can run) and joined to [`BEACON_GROUP`] on every adapter.
pub fn beacon_listener() -> std::io::Result<std::net::UdpSocket> {
    let sock = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    sock.set_reuse_address(true)?;
    #[cfg(all(unix, not(target_os = "solaris")))]
    sock.set_reuse_port(true)?;
    sock.set_nonblocking(true)?;
    sock.bind(&SocketAddr::from((Ipv4Addr::UNSPECIFIED, BEACON_PORT)).into())?;
    let mut joined = 0;
    for iface in ipv4_interfaces() {
        if sock.join_multicast_v4(&BEACON_GROUP, &iface.ip).is_ok() {
            joined += 1;
        }
    }
    if joined == 0 {
        let _ = sock.join_multicast_v4(&BEACON_GROUP, &Ipv4Addr::UNSPECIFIED);
    }
    Ok(sock.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interfaces_have_sane_broadcasts() {
        for i in ipv4_interfaces() {
            assert!(!i.ip.is_loopback());
            // The broadcast address is in the same subnet: it shares the
            // adapter's leading bits, and is at least as large.
            assert!(u32::from(i.broadcast) >= u32::from(i.ip), "{i:?}");
        }
    }

    #[test]
    fn a_beacon_sent_everywhere_is_heard() {
        let Ok(listener) = beacon_listener() else { return }; // port busy on this box
        listener.set_nonblocking(false).unwrap();
        listener.set_read_timeout(Some(std::time::Duration::from_secs(1))).unwrap();
        if ipv4_interfaces().is_empty() {
            return;
        }
        send_everywhere(b"hello", BEACON_PORT, true);
        let mut buf = [0u8; 16];
        let (n, _) = listener.recv_from(&mut buf).expect("beacon heard");
        assert_eq!(&buf[..n], b"hello");
    }
}
