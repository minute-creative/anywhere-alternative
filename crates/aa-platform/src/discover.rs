//! Finding the host without typing its address.
//!
//! Home routers hand out a fresh address whenever a machine reconnects, so
//! "192.168.1.3" is true for a day and wrong the next. The viewer finds the
//! host three ways at once and takes whichever answers:
//!
//! 1. **Listen for the host's beacon** (`aa_platform::lan`): the host
//!    announces itself every second by broadcast *and* multicast on every
//!    adapter. Listening needs nothing to get through the PC's firewall.
//! 2. **Ask**: broadcast `Discover` on every adapter's own subnet (from its
//!    real netmask) plus the all-ones broadcast; hosts answer `Here`.
//! 3. **Ask the last host directly** at the address that worked last time.
//! 4. **Ask every paired computer** at every address it was ever seen at,
//!    and every computer Tailscale says is online: that is how a computer
//!    on another network (office, phone hotspot) is found.
//!
//! The first version only did (2) on one guessed subnet, and on the owner's
//! network nothing answered.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::time::Duration;

use aa_core::control::ControlMessage;
use aa_core::wire::{self, Header, Kind, Packet, SeqCounter};
use bytes::{Bytes, BytesMut};
use tokio::net::UdpSocket;

/// The host's default port; discovery uses the same one.
pub const DEFAULT_PORT: u16 = 7700;
/// Longest we listen. Beacons come every second, so 2.5 s hears at least two.
const LISTEN: Duration = Duration::from_millis(2500);
/// Once something answered, wait this much longer for other hosts.
const AFTER_FIRST: Duration = Duration::from_millis(300);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub name: String,
    /// The best address to use (home network before Tailscale).
    pub addr: SocketAddr,
    /// The host's public key, if it said (every host since 0.4 does).
    pub key: Option<aa_core::secure::PublicKeyBytes>,
    /// Every address it answered at.
    pub addrs: Vec<SocketAddr>,
}

impl Found {
    /// Paired with this computer already (connects without a code)?
    pub fn paired(&self) -> bool {
        self.key.is_some_and(|k| crate::trust::paired_host(&k).is_some())
    }

    /// Reached through Tailscale (another network) rather than directly.
    pub fn via_tailscale(&self) -> bool {
        crate::tailscale::is_tailscale_ip(self.addr.ip())
    }
}

/// Where the last good host address is kept.
fn memory_file() -> Option<PathBuf> {
    #[cfg(target_os = "windows")]
    let base = std::env::var_os("APPDATA").map(PathBuf::from);
    #[cfg(target_os = "macos")]
    let base = std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library/Application Support"));
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let base = std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config"));
    base.map(|b| b.join("AnywhereAlternative").join("last-host"))
}

/// Remember a host that accepted us, to ask it first next time.
pub fn remember(addr: SocketAddr) {
    if let Some(path) = memory_file() {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(path, addr.to_string());
    }
}

/// The host that accepted us last time, if any.
pub fn remembered() -> Option<SocketAddr> {
    std::fs::read_to_string(memory_file()?).ok()?.trim().parse().ok()
}

fn control_datagram(msg: &ControlMessage) -> Bytes {
    let payload = msg.encode();
    let mut out = BytesMut::with_capacity(wire::HEADER_LEN + payload.len());
    Header {
        kind: Kind::Control,
        flags: 0,
        seq: SeqCounter::default().take(),
        frame_id: 0,
        slice_index: 0,
        slice_count: 1,
    }
    .write(&mut out);
    out.extend_from_slice(&payload);
    out.freeze()
}

/// macOS refuses local-network traffic from apps the user hasn't allowed
/// (System Settings → Privacy & Security → Local Network) with "no route to
/// host" or "permission denied".
fn looks_like_local_network_block(e: &std::io::Error) -> bool {
    matches!(e.raw_os_error(), Some(65 | 13 | 1))
}

/// Ask and listen; return every host heard, in the order heard.
pub async fn find_hosts(port: u16) -> anyhow::Result<Vec<Found>> {
    let socket = UdpSocket::bind("0.0.0.0:0").await?;
    socket.set_broadcast(true)?;
    let beacons = match crate::lan::beacon_listener().and_then(UdpSocket::from_std) {
        Ok(s) => Some(s),
        Err(e) => {
            tracing::debug!("not listening for beacons ({e}); asking only");
            None
        }
    };

    let ask = control_datagram(&ControlMessage::Discover);
    let mut targets: Vec<SocketAddr> =
        crate::lan::ipv4_interfaces().iter().map(|i| SocketAddr::new(IpAddr::V4(i.broadcast), port)).collect();
    targets.push(SocketAddr::new(IpAddr::V4(Ipv4Addr::BROADCAST), port));
    if let Some(last) = remembered() {
        targets.push(last);
    }
    targets.extend(far_targets(port));
    targets.sort();
    targets.dedup();

    let mut found: Vec<Found> = Vec::new();
    let mut blocked = false;
    let mut buf = vec![0u8; wire::MAX_DATAGRAM * 2];
    let mut beacon_buf = vec![0u8; wire::MAX_DATAGRAM * 2];
    let mut deadline = tokio::time::Instant::now() + LISTEN;
    let mut resend = tokio::time::interval(Duration::from_millis(400));

    loop {
        tokio::select! {
            _ = resend.tick() => {
                for t in &targets {
                    if let Err(e) = socket.send_to(&ask, t).await {
                        blocked |= looks_like_local_network_block(&e);
                        tracing::debug!("discover send to {t}: {e}");
                    }
                }
            }
            recv = socket.recv_from(&mut buf) => {
                let (n, from) = match recv {
                    Ok(x) => x,
                    // A paired computer that is switched off: Windows reports
                    // the bounce as an error on the next receive. Not fatal.
                    Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => continue,
                    Err(e) => return Err(e.into()),
                };
                if let Some(ControlMessage::Here { name, key, addrs }) = parse(&buf[..n]) {
                    let key = aa_core::secure::from_hex(&key);
                    if let Some(k) = key {
                        // Learn its other addresses (its Tailscale one, say)
                        // while we can see it, for when we can't.
                        if crate::trust::paired_host(&k).is_some() {
                            let mut all = vec![from.to_string()];
                            all.extend(addrs.iter().filter(|a| a.parse::<SocketAddr>().is_ok_and(|a| !a.ip().is_loopback())).cloned());
                            crate::trust::note_host_addrs(&k, &all);
                        }
                    }
                    add(&mut found, &mut deadline, name, from, key, "answered");
                }
            }
            recv = async { beacons.as_ref().expect("guarded").recv_from(&mut beacon_buf).await }, if beacons.is_some() => {
                if let Ok((n, from)) = recv {
                    if let Some(ControlMessage::Beacon { name, port, key }) = parse(&beacon_buf[..n]) {
                        let key = aa_core::secure::from_hex(&key);
                        add(&mut found, &mut deadline, name, SocketAddr::new(from.ip(), port), key, "announced");
                    }
                }
            }
            () = tokio::time::sleep_until(deadline) => break,
        }
    }
    if found.is_empty() && blocked {
        anyhow::bail!(
            "macOS is blocking local network access for this app. Open System Settings → Privacy & Security → \
             Local Network, switch on Terminal (or the app you run aa-viewer from), then try again"
        );
    }
    Ok(found)
}

fn parse(datagram: &[u8]) -> Option<ControlMessage> {
    let packet = Packet::parse(Bytes::copy_from_slice(datagram)).ok()?;
    (packet.header.kind == Kind::Control).then(|| ControlMessage::decode(&packet.payload).ok()).flatten()
}

/// Paired computers' remembered addresses and online Tailscale computers.
fn far_targets(port: u16) -> Vec<SocketAddr> {
    let mut t: Vec<SocketAddr> = crate::trust::paired_hosts()
        .iter()
        .flat_map(|h| h.addrs.iter().filter_map(|a| a.parse::<SocketAddr>().ok()))
        .filter(SocketAddr::is_ipv4)
        .collect();
    for p in crate::tailscale::peers().into_iter().filter(|p| p.online) {
        t.extend(p.ips.into_iter().filter(IpAddr::is_ipv4).map(|ip| SocketAddr::new(ip, port)));
    }
    t
}

/// Prefer a direct home-network address over a Tailscale one (one less hop).
fn better(new: SocketAddr, old: SocketAddr) -> bool {
    crate::tailscale::is_tailscale_ip(old.ip()) && !crate::tailscale::is_tailscale_ip(new.ip())
}

fn add(
    found: &mut Vec<Found>,
    deadline: &mut tokio::time::Instant,
    name: String,
    addr: SocketAddr,
    key: Option<aa_core::secure::PublicKeyBytes>,
    how: &str,
) {
    // The same computer at another address (Wi-Fi and Tailscale): one entry.
    if let Some(f) = found.iter_mut().find(|f| f.addr == addr || (key.is_some() && f.key == key)) {
        if !f.addrs.contains(&addr) {
            f.addrs.push(addr);
        }
        if better(addr, f.addr) {
            f.addr = addr;
        }
        if f.key.is_none() {
            f.key = key;
        }
        return;
    }
    tracing::info!(%addr, name, how, "found host");
    if found.is_empty() {
        *deadline = (*deadline).min(tokio::time::Instant::now() + AFTER_FIRST);
    }
    found.push(Found { name, addr, key, addrs: vec![addr] });
}

/// Find one particular computer (by its key) wherever it is now: home
/// network, new address, Tailscale. Used to reconnect.
pub async fn locate(key: &aa_core::secure::PublicKeyBytes) -> Option<SocketAddr> {
    let found = find_hosts(DEFAULT_PORT).await.ok()?;
    found.into_iter().find(|f| f.key.as_ref() == Some(key)).map(|f| f.addr)
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
    // Keep looking for a while: which computer gets started first should
    // not matter. Two minutes, then give up with advice.
    let started = std::time::Instant::now();
    let mut said = false;
    let hosts = loop {
        let found = find_hosts(DEFAULT_PORT).await?;
        if !found.is_empty() || input.is_some() {
            break found;
        }
        // Nothing answered or announced. If a host accepted us before, its
        // address is still the best bet (most routers keep it).
        if let Some(last) = remembered() {
            tracing::info!(addr = %last, "nothing answered; trying the host that worked last time");
            return Ok(last);
        }
        if started.elapsed() > std::time::Duration::from_secs(120) {
            break found;
        }
        if !said {
            said = true;
            tracing::info!(
                "no host yet; still looking (start aa-host on the other computer; it prints the address to \
                 type here if discovery is blocked)"
            );
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    };
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
