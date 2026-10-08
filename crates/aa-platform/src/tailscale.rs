//! Tailscale: reaching your computers from anywhere.
//!
//! Tailscale (free for personal use) links your computers into one private,
//! encrypted network that works through any router, so the PC at the office
//! can reach the Mac at home as if both were on the same Wi-Fi. Anywhere
//! doesn't need to know how; it just asks the Tailscale app which of your
//! computers are online and checks which of them are sharing.

use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Peer {
    pub name: String,
    pub ips: Vec<IpAddr>,
    pub online: bool,
}

/// Where the Tailscale command line is, if Tailscale is installed.
pub fn cli() -> Option<PathBuf> {
    let candidates: &[&str] = if cfg!(target_os = "windows") {
        &[r"C:\Program Files\Tailscale\tailscale.exe", r"C:\Program Files (x86)\Tailscale\tailscale.exe"]
    } else if cfg!(target_os = "macos") {
        &[
            "/Applications/Tailscale.app/Contents/MacOS/Tailscale",
            "/opt/homebrew/bin/tailscale",
            "/usr/local/bin/tailscale",
        ]
    } else {
        &["/usr/bin/tailscale", "/usr/local/bin/tailscale"]
    };
    candidates.iter().map(PathBuf::from).find(|p| p.exists())
}

pub fn installed() -> bool {
    cli().is_some()
}

/// Is this address inside Tailscale's private range (`100.64.0.0/10` or
/// `fd7a:115c:a1e0::/48`)?
pub fn is_tailscale_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            o[0] == 100 && (64..128).contains(&o[1])
        }
        IpAddr::V6(v6) => v6.segments()[..3] == [0xfd7a, 0x115c, 0xa1e0],
    }
}

/// Your other computers on Tailscale (cached for 20 s; asking takes ~0.1 s).
pub fn peers() -> Vec<Peer> {
    static CACHE: Mutex<Option<(Instant, Vec<Peer>)>> = Mutex::new(None);
    if let Ok(c) = CACHE.lock() {
        if let Some((t, p)) = c.as_ref().filter(|(t, _)| t.elapsed() < Duration::from_secs(20)) {
            let _ = t;
            return p.clone();
        }
    }
    let list = query().unwrap_or_default();
    if let Ok(mut c) = CACHE.lock() {
        *c = Some((Instant::now(), list.clone()));
    }
    list
}

fn query() -> Option<Vec<Peer>> {
    let mut cmd = std::process::Command::new(cli()?);
    cmd.args(["status", "--json"]).stdin(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // no console window flashing up
    }
    let out = cmd.output().ok()?;
    parse_status(&out.stdout)
}

/// Read `tailscale status --json`.
pub fn parse_status(json: &[u8]) -> Option<Vec<Peer>> {
    let v: serde_json::Value = serde_json::from_slice(json).ok()?;
    let peers = v.get("Peer")?.as_object()?;
    Some(
        peers
            .values()
            .map(|p| Peer {
                name: p.get("HostName").and_then(|n| n.as_str()).unwrap_or("").to_owned(),
                ips: p
                    .get("TailscaleIPs")
                    .and_then(|a| a.as_array())
                    .map(|a| a.iter().filter_map(|ip| ip.as_str()?.parse().ok()).collect())
                    .unwrap_or_default(),
                online: p.get("Online").and_then(serde_json::Value::as_bool).unwrap_or(false),
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_tailscale_status() {
        let json = br#"{"Self":{"HostName":"pc"},"Peer":{
            "nodekey:a":{"HostName":"Mac-mini","TailscaleIPs":["100.101.1.2","fd7a:115c:a1e0::1"],"Online":true},
            "nodekey:b":{"HostName":"old-laptop","TailscaleIPs":["100.90.0.9"],"Online":false}}}"#;
        let mut p = parse_status(json).unwrap();
        p.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(p[0].name, "Mac-mini");
        assert!(p[0].online);
        assert_eq!(p[0].ips.len(), 2);
        assert!(!p[1].online);
        assert!(is_tailscale_ip("100.101.1.2".parse().unwrap()));
        assert!(!is_tailscale_ip("192.168.1.2".parse().unwrap()));
        assert!(is_tailscale_ip("fd7a:115c:a1e0::1".parse().unwrap()));
    }
}
