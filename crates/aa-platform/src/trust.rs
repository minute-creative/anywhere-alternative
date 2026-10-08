//! Who this computer trusts, kept in small files next to the settings.
//!
//! - `viewer.key` / `host.key`: this computer's own secret keys (one for
//!   connecting out, one for sharing). Never leave the computer.
//! - `paired-hosts.json`: computers we can connect to without a code, with
//!   every address they were seen at (home network, Tailscale…).
//! - `paired-viewers.json`: computers allowed to connect here.
//! - `pair-code`: the six-digit code shown on the Share page.
//!
//! On a Mac that shares at the login screen, the sharing side lives in
//! `/Library/Application Support/AnywhereAlternative` instead, readable by
//! administrators and the system, so the login-screen copy of the host and
//! the logged-in copy are the same computer to the viewers. The app creates
//! that folder when "share after a restart" is switched on.
//!
//! `AA_DATA_DIR` moves everything (tests, two copies on one machine).

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use aa_core::secure::{self, Identity, PublicKeyBytes};
use serde::{Deserialize, Serialize};

/// The user's own folder (settings, logs, the viewer side).
pub fn user_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("AA_DATA_DIR") {
        let d = PathBuf::from(d);
        let _ = std::fs::create_dir_all(&d);
        return d;
    }
    #[cfg(target_os = "windows")]
    let base = std::env::var_os("APPDATA").map(PathBuf::from);
    #[cfg(target_os = "macos")]
    let base = std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library/Application Support"));
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let base = std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config"));
    let dir = base.unwrap_or_else(std::env::temp_dir).join("AnywhereAlternative");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// The shared folder used on a Mac for sharing at the login screen.
pub const MAC_SYSTEM_DIR: &str = "/Library/Application Support/AnywhereAlternative";

/// Where the sharing side keeps its key, pairings and code.
pub fn host_dir() -> PathBuf {
    if std::env::var_os("AA_DATA_DIR").is_none() && cfg!(target_os = "macos") {
        let sys = Path::new(MAC_SYSTEM_DIR);
        if sys.is_dir() && writable(sys) {
            return sys.to_path_buf();
        }
    }
    user_dir()
}

fn writable(dir: &Path) -> bool {
    let probe = dir.join(".write-test");
    let ok = std::fs::write(&probe, b"").is_ok();
    let _ = std::fs::remove_file(&probe);
    ok
}

/// Write a file only this user (and, in the shared Mac folder, the
/// administrators group) can read.
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let shared = path.starts_with(MAC_SYSTEM_DIR);
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(if shared { 0o660 } else { 0o600 }));
    }
    // Rename is all-or-nothing: a crash never leaves half a file.
    std::fs::rename(&tmp, path)
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn load_identity(path: &Path) -> Identity {
    if let Ok(b) = std::fs::read(path) {
        if let Ok(secret) = <[u8; 32]>::try_from(b.as_slice()) {
            return Identity::from_secret(secret);
        }
    }
    let id = Identity::generate();
    if let Err(e) = write_private(path, &id.secret_bytes()) {
        tracing::warn!("could not save this computer's key to {}: {e}", path.display());
    }
    id
}

/// This computer's key for connecting to others.
pub fn viewer_identity() -> Identity {
    load_identity(&user_dir().join("viewer.key"))
}

/// This computer's key for being shared.
pub fn host_identity() -> Identity {
    load_identity(&host_dir().join("host.key"))
}

/// A short name for this computer ("Maitrik's Mac mini").
pub fn computer_name() -> String {
    let n = gethostname::gethostname().to_string_lossy().into_owned();
    n.trim_end_matches(".local").to_owned()
}

// ---------------------------------------------------------------------------
// Computers we connect to
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PairedHost {
    pub name: String,
    /// Public key, hex.
    pub key: String,
    /// Addresses it answered at, most recent first.
    #[serde(default)]
    pub addrs: Vec<String>,
    #[serde(default)]
    pub last_used: u64,
}

fn hosts_file() -> PathBuf {
    user_dir().join("paired-hosts.json")
}

pub fn paired_hosts() -> Vec<PairedHost> {
    std::fs::read(hosts_file()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

fn save_hosts(list: &[PairedHost]) {
    if let Ok(b) = serde_json::to_vec_pretty(list) {
        let _ = write_private(&hosts_file(), &b);
    }
}

pub fn paired_host(key: &PublicKeyBytes) -> Option<PairedHost> {
    let hex = secure::to_hex(key);
    paired_hosts().into_iter().find(|h| h.key == hex)
}

/// Remember a computer we just paired with (or update its name/address).
pub fn add_paired_host(name: &str, key: &PublicKeyBytes, addr: Option<&str>) {
    let hex = secure::to_hex(key);
    let mut list = paired_hosts();
    if let Some(h) = list.iter_mut().find(|h| h.key == hex) {
        name.clone_into(&mut h.name);
    } else {
        list.push(PairedHost { name: name.to_owned(), key: hex.clone(), addrs: Vec::new(), last_used: now() });
    }
    save_hosts(&list);
    if let Some(a) = addr {
        note_host_addrs(key, &[a.to_owned()]);
    }
}

/// A paired computer answered at these addresses: keep them (newest first,
/// at most eight) so it can be found again from anywhere.
pub fn note_host_addrs(key: &PublicKeyBytes, addrs: &[String]) {
    let hex = secure::to_hex(key);
    let mut list = paired_hosts();
    let Some(h) = list.iter_mut().find(|h| h.key == hex) else { return };
    let before = h.addrs.clone();
    for a in addrs.iter().rev() {
        h.addrs.retain(|x| x != a);
        h.addrs.insert(0, a.clone());
    }
    h.addrs.truncate(8);
    if h.addrs != before {
        save_hosts(&list);
    }
}

pub fn mark_host_used(key: &PublicKeyBytes) {
    let hex = secure::to_hex(key);
    let mut list = paired_hosts();
    if let Some(h) = list.iter_mut().find(|h| h.key == hex) {
        h.last_used = now();
        save_hosts(&list);
    }
}

pub fn forget_host(key_hex: &str) {
    let mut list = paired_hosts();
    list.retain(|h| h.key != key_hex);
    save_hosts(&list);
}

// ---------------------------------------------------------------------------
// Computers allowed to connect here
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PairedViewer {
    pub name: String,
    pub key: String,
    #[serde(default)]
    pub added: u64,
}

fn viewers_file() -> PathBuf {
    host_dir().join("paired-viewers.json")
}

pub fn paired_viewers() -> Vec<PairedViewer> {
    std::fs::read(viewers_file()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

pub fn is_paired_viewer(key: &PublicKeyBytes) -> bool {
    let hex = secure::to_hex(key);
    paired_viewers().iter().any(|v| v.key == hex)
}

pub fn paired_viewer_name(key: &PublicKeyBytes) -> Option<String> {
    let hex = secure::to_hex(key);
    paired_viewers().into_iter().find(|v| v.key == hex).map(|v| v.name)
}

pub fn add_paired_viewer(name: &str, key: &PublicKeyBytes) {
    let hex = secure::to_hex(key);
    let mut list = paired_viewers();
    list.retain(|v| v.key != hex);
    list.push(PairedViewer { name: name.to_owned(), key: hex, added: now() });
    if let Ok(b) = serde_json::to_vec_pretty(&list) {
        if let Err(e) = write_private(&viewers_file(), &b) {
            tracing::warn!("could not save the paired computer: {e}");
        }
    }
}

pub fn forget_viewer(key_hex: &str) {
    let mut list = paired_viewers();
    list.retain(|v| v.key != key_hex);
    if let Ok(b) = serde_json::to_vec_pretty(&list) {
        let _ = write_private(&viewers_file(), &b);
    }
}

// ---------------------------------------------------------------------------
// The pairing code
// ---------------------------------------------------------------------------

fn code_file() -> PathBuf {
    host_dir().join("pair-code")
}

/// The code shown on the Share page (made on first use). `AA_PAIR_CODE`
/// fixes it (tests).
pub fn pair_code() -> String {
    if let Ok(c) = std::env::var("AA_PAIR_CODE") {
        return secure::normalize_code(&c);
    }
    if let Ok(c) = std::fs::read_to_string(code_file()) {
        let c = secure::normalize_code(&c);
        if c.len() == 6 {
            return c;
        }
    }
    new_pair_code()
}

/// Replace the code (after a pairing, or too many wrong tries).
pub fn new_pair_code() -> String {
    if let Ok(c) = std::env::var("AA_PAIR_CODE") {
        return secure::normalize_code(&c);
    }
    let c = secure::new_code();
    let _ = write_private(&code_file(), c.as_bytes());
    c
}

/// "482913" → "482 913", easier to read and type.
pub fn spaced(code: &str) -> String {
    if code.len() == 6 {
        format!("{} {}", &code[..3], &code[3..])
    } else {
        code.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pairings_are_remembered_and_forgotten() {
        let dir = std::env::temp_dir().join(format!("aa-trust-{}", std::process::id()));
        std::env::set_var("AA_DATA_DIR", &dir);
        let a = Identity::generate().public();
        add_paired_host("Mac mini", &a, Some("192.168.1.20:7700"));
        note_host_addrs(&a, &["100.101.102.103:7700".into()]);
        note_host_addrs(&a, &["192.168.1.20:7700".into()]);
        let h = paired_host(&a).unwrap();
        assert_eq!(h.addrs, vec!["192.168.1.20:7700", "100.101.102.103:7700"]);
        add_paired_viewer("PC", &a);
        assert!(is_paired_viewer(&a));
        forget_viewer(&secure::to_hex(&a));
        assert!(!is_paired_viewer(&a));
        forget_host(&secure::to_hex(&a));
        assert!(paired_host(&a).is_none());
        // Keys survive a restart.
        assert_eq!(viewer_identity().public(), viewer_identity().public());
        assert_eq!(spaced("482913"), "482 913");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
