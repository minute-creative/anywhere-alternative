#![allow(clippy::doc_markdown)] // UPnP, NAT-PMP read better plain
//! Opening a door on the home router, so the other computer can reach this
//! one from anywhere without Tailscale or any server of ours.
//!
//! In plain words: a home router lets everything go out but stops anything
//! from coming in that nothing inside asked for. Most routers let a program
//! ask "please send what arrives on port 7700 to me" (the router feature is
//! called UPnP; Apple-style routers use NAT-PMP instead). The sharing
//! computer asks once an hour, and tells its paired computers the outside
//! address it got. From then on they can connect from any network.
//!
//! When it can't work, the log says why in plain words, starting with
//! `outside:` (the app shows that line on its Share page):
//! - the router has door-opening switched off (UPnP off in its settings);
//! - the internet provider shares one public address between many homes
//!   (the router's own outside address is a private one, CGNAT): no door on
//!   this router reaches the internet, so Tailscale is the way in.
//!
//! Written by hand with only the standard library (about 300 lines): the
//! ready-made Rust UPnP crate needs a newer compiler than this project
//! supports and brings dozens of dependencies.
//!
//! Debugging: `AA_NO_PORTMAP=1` switches it off (tests, mock hosts).

use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream, UdpSocket};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How long the router keeps a door open; renewed at half that.
const LEASE_SECS: u32 = 3600;
/// Give up on a router that doesn't answer within this.
const TIMEOUT: Duration = Duration::from_secs(3);
/// Shown to the person in the router's own list of open doors.
const DESCRIPTION: &str = "Anywhere (remote desktop)";

/// What this computer looks like from the internet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outside {
    /// A door is open: the other computer can reach us at `addr`.
    Open { addr: SocketAddr, how: &'static str },
    /// The router's own outside address is private: the provider shares
    /// one public address between many homes (CGNAT).
    Shared { router_outside: Ipv4Addr },
    /// The router didn't answer door-opening requests (UPnP and NAT-PMP
    /// switched off, or not supported).
    NoHelp,
}

impl Outside {
    /// One line for the log and the app.
    pub fn describe(&self) -> String {
        match self {
            Self::Open { addr, how } => {
                format!("reachable from anywhere at {addr} (the router opened a door, {how})")
            }
            Self::Shared { router_outside } => format!(
                "not reachable from outside directly: the internet provider shares one public address between \
                 many homes (the router's outside address is {router_outside}). Use Tailscale to connect from \
                 other networks, or ask the provider for a public IP or IPv6."
            ),
            Self::NoHelp => "not reachable from outside directly: the router didn't open a door (switch on UPnP \
                             in the router's settings). Tailscale still works from other networks."
                .to_owned(),
        }
    }

    /// The outside address, when there is one.
    pub fn addr(&self) -> Option<SocketAddr> {
        match self {
            Self::Open { addr, .. } => Some(*addr),
            _ => None,
        }
    }
}

/// The latest answer of the running door keeper, for code that can't hold
/// the [`PortMap`] itself (the host's "who's there?" answer).
static CURRENT: Mutex<Option<Outside>> = Mutex::new(None);

/// What this computer looks like from the internet right now (`None`: no
/// door keeper running, or no answer yet).
pub fn current() -> Option<Outside> {
    CURRENT.lock().ok().and_then(|c| c.clone())
}

/// The door keeper: a background thread that opens the door and keeps it
/// open. Cheap to clone; ask it [`PortMap::outside`] any time.
#[derive(Debug, Clone, Default)]
pub struct PortMap {
    state: Arc<Mutex<Option<Outside>>>,
    gateway: Arc<Mutex<Option<Gateway>>>,
}

impl PortMap {
    /// Start keeping a door open for UDP `port`. Returns at once; the first
    /// answer comes within a few seconds.
    pub fn start(port: u16) -> Self {
        let me = Self::default();
        if std::env::var_os("AA_NO_PORTMAP").is_some() {
            return me;
        }
        let keeper = me.clone();
        let _ = std::thread::Builder::new().name("aa-portmap".into()).spawn(move || loop {
            let now = keeper.attempt(port);
            let changed = keeper.state.lock().map(|mut s| {
                let changed = s.as_ref() != Some(&now);
                *s = Some(now.clone());
                changed
            });
            if let Ok(mut c) = CURRENT.lock() {
                *c = Some(now.clone());
            }
            if changed.unwrap_or(false) {
                if now.addr().is_some() {
                    tracing::info!("outside: {}", now.describe());
                } else {
                    tracing::warn!("outside: {}", now.describe());
                }
            }
            // Renew at half the lease; retry sooner after a failure (the
            // router may still be starting up after a power cut).
            let wait = if now.addr().is_some() { u64::from(LEASE_SECS / 2) } else { 300 };
            std::thread::sleep(Duration::from_secs(wait));
        });
        me
    }

    /// What we know so far (`None` until the first attempt finished).
    pub fn outside(&self) -> Option<Outside> {
        self.state.lock().ok().and_then(|s| s.clone())
    }

    /// Close the door again (sharing stopped). Best effort: the router
    /// closes it by itself when the lease runs out anyway.
    pub fn close(&self) {
        if let Some(g) = self.gateway.lock().ok().and_then(|g| g.clone()) {
            if let Some(external) = self.outside().and_then(|o| o.addr()).map(|a| a.port()) {
                g.remove(external);
            }
        }
    }

    fn attempt(&self, port: u16) -> Outside {
        // UPnP first (most home routers, the owner's ZTE among them).
        if let Some(g) = Gateway::find() {
            if let Some(ext) = g.external_ip() {
                if !is_public(ext) {
                    return Outside::Shared { router_outside: ext };
                }
                if let Some(external_port) = g.open(port) {
                    if let Ok(mut slot) = self.gateway.lock() {
                        *slot = Some(g);
                    }
                    return Outside::Open { addr: SocketAddr::new(IpAddr::V4(ext), external_port), how: "UPnP" };
                }
            }
        }
        // Then NAT-PMP (Apple and some other routers).
        if let Some(found) = natpmp::open(port) {
            return found;
        }
        Outside::NoHelp
    }
}

/// An address the whole internet can reach (not private, not shared CGNAT).
pub fn is_public(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    let cgnat = o[0] == 100 && (64..128).contains(&o[1]);
    !(ip.is_private() || ip.is_loopback() || ip.is_link_local() || ip.is_unspecified() || cgnat)
}

// ---------------------------------------------------------------------------
// UPnP Internet Gateway Device
// ---------------------------------------------------------------------------

/// The router's door-opening service, as its description file names it.
#[derive(Debug, Clone)]
struct Gateway {
    /// Router address and port for its control page.
    host: SocketAddr,
    /// Path of the control page.
    control: String,
    /// `urn:schemas-upnp-org:service:WANIPConnection:1` or the PPP variant.
    service: String,
    /// This computer's address on the router's network.
    local: Ipv4Addr,
}

impl Gateway {
    /// Ask the network "is there a router that opens doors?" (SSDP) and read
    /// its description.
    fn find() -> Option<Self> {
        let sock = UdpSocket::bind("0.0.0.0:0").ok()?;
        sock.set_read_timeout(Some(Duration::from_millis(400))).ok()?;
        let _ = sock.set_multicast_ttl_v4(2);
        let targets = [
            "urn:schemas-upnp-org:device:InternetGatewayDevice:1",
            "urn:schemas-upnp-org:device:InternetGatewayDevice:2",
            "urn:schemas-upnp-org:service:WANIPConnection:1",
        ];
        let deadline = Instant::now() + TIMEOUT;
        let mut buf = [0u8; 2048];
        let mut asked = 0;
        while Instant::now() < deadline {
            if asked < 3 {
                for st in targets {
                    let msg = format!(
                        "M-SEARCH * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nMAN: \"ssdp:discover\"\r\nMX: \
                         2\r\nST: {st}\r\n\r\n"
                    );
                    let _ = sock.send_to(msg.as_bytes(), "239.255.255.250:1900");
                }
                asked += 1;
            }
            let Ok((n, _)) = sock.recv_from(&mut buf) else { continue };
            let reply = String::from_utf8_lossy(&buf[..n]);
            let Some(location) = header(&reply, "location") else { continue };
            if let Some(g) = Self::from_location(location) {
                return Some(g);
            }
        }
        None
    }

    fn from_location(location: &str) -> Option<Self> {
        let (host, path) = split_url(location)?;
        let (_, xml) = http(host, &format!("GET {path} HTTP/1.0\r\nHost: {host}\r\n\r\n"))?;
        let (service, control) = control_url(&xml)?;
        // A control URL may be absolute (another port) or a path.
        let (host, control) = if control.starts_with("http://") {
            let (h, p) = split_url(&control)?;
            (h, p.to_owned())
        } else if control.starts_with('/') {
            (host, control)
        } else {
            (host, format!("/{control}"))
        };
        let local = match TcpStream::connect_timeout(&host, TIMEOUT).ok()?.local_addr().ok()?.ip() {
            IpAddr::V4(v4) => v4,
            IpAddr::V6(_) => return None,
        };
        Some(Self { host, control, service, local })
    }

    fn soap(&self, action: &str, args: &[(&str, String)]) -> Option<(u16, String)> {
        let inner = args.iter().fold(String::new(), |mut acc, (k, v)| {
            use std::fmt::Write as _;
            let _ = write!(acc, "<{k}>{v}</{k}>");
            acc
        });
        let body = format!(
            "<?xml version=\"1.0\"?><s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" \
             s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\"><s:Body><u:{action} \
             xmlns:u=\"{svc}\">{inner}</u:{action}></s:Body></s:Envelope>",
            svc = self.service
        );
        let req = format!(
            "POST {path} HTTP/1.0\r\nHost: {host}\r\nContent-Type: text/xml; charset=\"utf-8\"\r\nSOAPAction: \
             \"{svc}#{action}\"\r\nContent-Length: {len}\r\n\r\n{body}",
            path = self.control,
            host = self.host,
            svc = self.service,
            len = body.len()
        );
        http(self.host, &req)
    }

    fn external_ip(&self) -> Option<Ipv4Addr> {
        let (status, body) = self.soap("GetExternalIPAddress", &[])?;
        (status == 200).then(|| tag(&body, "NewExternalIPAddress")?.trim().parse().ok()).flatten()
    }

    /// Open UDP `port` (or the next free one) to this computer. Returns the
    /// outside port.
    fn open(&self, port: u16) -> Option<u16> {
        for external in port..port.saturating_add(8) {
            for lease in [LEASE_SECS, 0] {
                let args = [
                    ("NewRemoteHost", String::new()),
                    ("NewExternalPort", external.to_string()),
                    ("NewProtocol", "UDP".to_owned()),
                    ("NewInternalPort", port.to_string()),
                    ("NewInternalClient", self.local.to_string()),
                    ("NewEnabled", "1".to_owned()),
                    ("NewPortMappingDescription", DESCRIPTION.to_owned()),
                    ("NewLeaseDuration", lease.to_string()),
                ];
                let (status, body) = self.soap("AddPortMapping", &args)?;
                if status == 200 {
                    return Some(external);
                }
                match tag(&body, "errorCode").map(str::trim) {
                    // "Only permanent doors": ask again without an end time
                    // (renewing keeps it ours; closing removes it).
                    Some("725") => {}
                    // Taken by another computer: try the next port.
                    Some("718") => break,
                    other => {
                        tracing::debug!(error = ?other, "router refused to open a door");
                        return None;
                    }
                }
            }
        }
        None
    }

    fn remove(&self, external: u16) {
        let args = [
            ("NewRemoteHost", String::new()),
            ("NewExternalPort", external.to_string()),
            ("NewProtocol", "UDP".to_owned()),
        ];
        let _ = self.soap("DeletePortMapping", &args);
    }
}

/// One header's value from an SSDP or HTTP reply (names ignore case).
fn header<'a>(reply: &'a str, name: &str) -> Option<&'a str> {
    reply.lines().find_map(|l| {
        let (k, v) = l.split_once(':')?;
        k.trim().eq_ignore_ascii_case(name).then(|| v.trim())
    })
}

/// `http://192.168.1.1:5000/desc.xml` → (router address, `/desc.xml`).
fn split_url(url: &str) -> Option<(SocketAddr, &str)> {
    let rest = url.strip_prefix("http://")?;
    let (hostport, path) = rest.find('/').map_or((rest, "/"), |i| (&rest[..i], &rest[i..]));
    let addr = match hostport.parse::<SocketAddr>() {
        Ok(a) => a,
        Err(_) => SocketAddr::new(hostport.parse::<IpAddr>().ok()?, 80),
    };
    Some((addr, path))
}

/// The text between `<name>` and `</name>` (any namespace prefix), first
/// occurrence.
fn tag<'a>(xml: &'a str, name: &str) -> Option<&'a str> {
    let open = xml.find(&format!("{name}>"))?;
    let start = open + name.len() + 1;
    let end = xml[start..].find("</")? + start;
    Some(&xml[start..end])
}

/// The door-opening service in a router's description: (service type,
/// control URL). IP connections are preferred over PPP ones.
fn control_url(xml: &str) -> Option<(String, String)> {
    let mut best: Option<(String, String)> = None;
    for block in xml.split("<service>").skip(1) {
        let Some(st) = tag(block, "serviceType") else { continue };
        let st = st.trim();
        let ip = st.contains(":WANIPConnection:");
        if !(ip || st.contains(":WANPPPConnection:")) {
            continue;
        }
        let Some(url) = tag(block, "controlURL") else { continue };
        if best.is_none() || ip {
            best = Some((st.to_owned(), url.trim().to_owned()));
        }
        if ip {
            break;
        }
    }
    best
}

/// One HTTP/1.0 exchange (so the answer is never "chunked"): status code
/// and body.
fn http(to: SocketAddr, request: &str) -> Option<(u16, String)> {
    let mut s = TcpStream::connect_timeout(&to, TIMEOUT).ok()?;
    s.set_read_timeout(Some(TIMEOUT)).ok()?;
    s.set_write_timeout(Some(TIMEOUT)).ok()?;
    s.write_all(request.as_bytes()).ok()?;
    let mut raw = Vec::new();
    let _ = s.take(256 * 1024).read_to_end(&mut raw);
    let text = String::from_utf8_lossy(&raw).into_owned();
    let status = text.split_whitespace().nth(1)?.parse().ok()?;
    let body = text.split_once("\r\n\r\n").map_or(String::new(), |(_, b)| b.to_owned());
    Some((status, body))
}

// ---------------------------------------------------------------------------
// NAT-PMP (RFC 6886): the simpler Apple-style door opener
// ---------------------------------------------------------------------------

mod natpmp {
    use super::{is_public, Outside, LEASE_SECS};
    use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
    use std::time::Duration;

    /// Routers listen for NAT-PMP here.
    const PORT: u16 = 5351;

    /// The likely router on each network this computer is on: the first
    /// address of the subnet (192.168.1.1 and the like). NAT-PMP has no
    /// discovery; asking the wrong address costs a quarter of a second.
    fn guesses() -> Vec<Ipv4Addr> {
        let mut g: Vec<Ipv4Addr> = crate::lan::ipv4_interfaces()
            .iter()
            .filter(|i| i.ip.is_private())
            .map(|i| {
                let o = i.ip.octets();
                Ipv4Addr::new(o[0], o[1], o[2], 1)
            })
            .collect();
        g.dedup();
        g
    }

    fn ask(sock: &UdpSocket, gw: Ipv4Addr, req: &[u8], want: u8) -> Option<Vec<u8>> {
        for _ in 0..2 {
            sock.send_to(req, SocketAddr::new(IpAddr::V4(gw), PORT)).ok()?;
            let mut buf = [0u8; 16];
            if let Ok((n, from)) = sock.recv_from(&mut buf) {
                if from.ip() == IpAddr::V4(gw) && n >= 8 && buf[1] == want && buf[2..4] == [0, 0] {
                    return Some(buf[..n].to_vec());
                }
            }
        }
        None
    }

    pub(super) fn open(port: u16) -> Option<Outside> {
        let sock = UdpSocket::bind("0.0.0.0:0").ok()?;
        sock.set_read_timeout(Some(Duration::from_millis(250))).ok()?;
        for gw in guesses() {
            // "What is your outside address?" → [0, 128, result, epoch, addr].
            let Some(r) = ask(&sock, gw, &[0, 0], 128) else { continue };
            if r.len() < 12 {
                continue;
            }
            let ext = Ipv4Addr::new(r[8], r[9], r[10], r[11]);
            if !is_public(ext) {
                return Some(Outside::Shared { router_outside: ext });
            }
            // "Open UDP port" → [0, 129, result, epoch, inside, outside, lease].
            let mut req = vec![0, 1, 0, 0];
            req.extend_from_slice(&port.to_be_bytes());
            req.extend_from_slice(&port.to_be_bytes());
            req.extend_from_slice(&LEASE_SECS.to_be_bytes());
            let r = ask(&sock, gw, &req, 129)?;
            if r.len() >= 12 {
                let external = u16::from_be_bytes([r[10], r[11]]);
                return Some(Outside::Open { addr: SocketAddr::new(IpAddr::V4(ext), external), how: "NAT-PMP" });
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_and_shared_addresses() {
        assert!(is_public(Ipv4Addr::new(103, 206, 1, 2)));
        assert!(!is_public(Ipv4Addr::new(10, 113, 0, 1)), "provider-internal");
        assert!(!is_public(Ipv4Addr::new(100, 72, 3, 4)), "CGNAT range");
        assert!(!is_public(Ipv4Addr::new(192, 168, 1, 1)));
        assert!(is_public(Ipv4Addr::new(100, 128, 0, 1)), "just past CGNAT");
    }

    #[test]
    fn reads_an_ssdp_reply_and_urls() {
        let reply =
            "HTTP/1.1 200 OK\r\nCACHE-CONTROL: max-age=120\r\nLocation: http://192.168.1.1:52869/gw.xml\r\n\r\n";
        assert_eq!(header(reply, "location"), Some("http://192.168.1.1:52869/gw.xml"));
        let (a, p) = split_url("http://192.168.1.1:52869/gw.xml").unwrap();
        assert_eq!(a, "192.168.1.1:52869".parse().unwrap());
        assert_eq!(p, "/gw.xml");
        let (a, p) = split_url("http://10.0.0.1").unwrap();
        assert_eq!(a, "10.0.0.1:80".parse().unwrap());
        assert_eq!(p, "/");
    }

    #[test]
    fn finds_the_ip_connection_service() {
        let xml = "<root><device><serviceList>\
            <service><serviceType>urn:schemas-upnp-org:service:Layer3Forwarding:1</serviceType>\
              <controlURL>/l3f</controlURL></service>\
            <service><serviceType>urn:schemas-upnp-org:service:WANPPPConnection:1</serviceType>\
              <controlURL>/ppp</controlURL></service>\
            <service><serviceType>urn:schemas-upnp-org:service:WANIPConnection:1</serviceType>\
              <controlURL>/upnp/control/WANIPConn1</controlURL></service>\
            </serviceList></device></root>";
        let (svc, url) = control_url(xml).unwrap();
        assert_eq!(svc, "urn:schemas-upnp-org:service:WANIPConnection:1");
        assert_eq!(url, "/upnp/control/WANIPConn1");
        let soap = "<s:Body><u:GetExternalIPAddressResponse><NewExternalIPAddress>103.206.1.2\
                    </NewExternalIPAddress></u:GetExternalIPAddressResponse></s:Body>";
        assert_eq!(tag(soap, "NewExternalIPAddress").map(str::trim), Some("103.206.1.2"));
        assert_eq!(tag("<errorCode>725</errorCode>", "errorCode"), Some("725"));
    }

    /// A pretend router: serves its description, then answers SOAP calls the
    /// way a real UPnP router does (one refusal of a timed door first).
    #[test]
    fn talks_to_a_pretend_router() {
        use std::io::{BufRead, BufReader};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let log = Arc::clone(&seen);
        std::thread::spawn(move || {
            for stream in listener.incoming().take(5) {
                let mut s = stream.unwrap();
                let mut reader = BufReader::new(s.try_clone().unwrap());
                let mut head = String::new();
                let mut len = 0usize;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if let Some(v) = header(&line, "content-length") {
                        len = v.parse().unwrap();
                    }
                    head.push_str(&line);
                    if line == "\r\n" || line.is_empty() {
                        break;
                    }
                }
                let mut body = vec![0u8; len];
                reader.read_exact(&mut body).unwrap();
                let body = String::from_utf8(body).unwrap();
                log.lock().unwrap().push(format!("{head}{body}"));
                let (status, reply) = if head.starts_with("GET") {
                    (200, "<root><service><serviceType>urn:schemas-upnp-org:service:WANIPConnection:1</serviceType><controlURL>/ctl</controlURL></service></root>".to_owned())
                } else if head.contains("#GetExternalIPAddress") {
                    (200, "<NewExternalIPAddress>103.206.1.2</NewExternalIPAddress>".to_owned())
                } else if body.contains("<NewLeaseDuration>3600<") {
                    (500, "<errorCode>725</errorCode>".to_owned())
                } else {
                    (200, String::new())
                };
                let _ = write!(s, "HTTP/1.0 {status} OK\r\nContent-Length: {}\r\n\r\n{reply}", reply.len());
            }
        });
        let g = Gateway::from_location(&format!("http://{addr}/desc.xml")).expect("description");
        assert_eq!(g.control, "/ctl");
        assert_eq!(g.local, Ipv4Addr::LOCALHOST);
        assert_eq!(g.external_ip(), Some(Ipv4Addr::new(103, 206, 1, 2)));
        assert_eq!(g.open(7700), Some(7700), "falls back to a permanent door after error 725");
        let calls = seen.lock().unwrap();
        let add = calls.iter().rfind(|c| c.contains("#AddPortMapping")).expect("asked to open");
        assert!(add.contains("<NewProtocol>UDP</NewProtocol>") && add.contains("<NewInternalPort>7700<"));
        assert!(add.contains("<NewLeaseDuration>0<"));
    }

    #[test]
    fn describes_each_case_plainly() {
        let open = Outside::Open { addr: "103.206.1.2:7700".parse().unwrap(), how: "UPnP" };
        assert!(open.describe().starts_with("reachable from anywhere at 103.206.1.2:7700"));
        assert_eq!(open.addr(), Some("103.206.1.2:7700".parse().unwrap()));
        assert!(Outside::Shared { router_outside: Ipv4Addr::new(10, 1, 2, 3) }.describe().contains("Tailscale"));
        assert!(Outside::NoHelp.addr().is_none());
    }
}
