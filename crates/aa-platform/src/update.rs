//! Automatic updates from the project's GitHub Releases page.
//!
//! Asks GitHub which version is newest (a small public web request, no
//! account) and downloads the installer for this system. Uses the `curl`
//! that ships with macOS and with Windows 10/11, so no web library is added
//! to the app. Who installs it depends on the system (see the app's
//! `update.rs` and, on Windows, the sharing service in `windows/session.rs`).

use std::path::Path;
use std::process::Command;

pub const REPO: &str = "minute-creative/anywhere-alternative";

/// This build's version ("0.4.2").
pub fn current() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    pub version: String,
    pub mac_url: Option<String>,
    pub windows_url: Option<String>,
}

impl Release {
    /// The installer for the system we're running on.
    pub fn url_here(&self) -> Option<&str> {
        if cfg!(target_os = "windows") {
            self.windows_url.as_deref()
        } else if cfg!(target_os = "macos") {
            self.mac_url.as_deref()
        } else {
            None
        }
    }
}

fn parts(v: &str) -> Vec<u64> {
    v.trim().trim_start_matches('v').split('.').map(|p| p.parse().unwrap_or(0)).collect()
}

/// Is `candidate` a later version than `than`? ("0.10.0" > "0.9.3")
pub fn is_newer(candidate: &str, than: &str) -> bool {
    let (a, b) = (parts(candidate), parts(than));
    for i in 0..a.len().max(b.len()) {
        let (x, y) = (a.get(i).copied().unwrap_or(0), b.get(i).copied().unwrap_or(0));
        if x != y {
            return x > y;
        }
    }
    false
}

/// Read GitHub's "latest release" answer.
pub fn parse_release(json: &[u8]) -> Option<Release> {
    let v: serde_json::Value = serde_json::from_slice(json).ok()?;
    let version = v.get("tag_name")?.as_str()?.trim_start_matches('v').to_owned();
    let mut r = Release { version, mac_url: None, windows_url: None };
    for a in v.get("assets")?.as_array()? {
        let (Some(name), Some(url)) =
            (a.get("name").and_then(|n| n.as_str()), a.get("browser_download_url").and_then(|u| u.as_str()))
        else {
            continue;
        };
        if name.ends_with("-mac.dmg") {
            r.mac_url = Some(url.to_owned());
        } else if name.ends_with("-windows-setup.exe") {
            r.windows_url = Some(url.to_owned());
        }
    }
    Some(r)
}

/// `curl`, quiet, failing on HTTP errors, never flashing a console window.
pub fn curl() -> Command {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        let sys = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
        let mut c = Command::new(format!(r"{sys}\System32\curl.exe"));
        c.creation_flags(0x0800_0000);
        c.args(["-fsSL", "--retry", "2"]);
        c
    }
    #[cfg(not(target_os = "windows"))]
    {
        let mut c = Command::new("/usr/bin/curl");
        c.args(["-fsSL", "--retry", "2"]);
        c
    }
}

/// The newest release on GitHub.
pub fn latest() -> anyhow::Result<Release> {
    let out = curl()
        .args(["-H", "Accept: application/vnd.github+json", "-H", "User-Agent: Anywhere-updater"])
        .arg(format!("https://api.github.com/repos/{REPO}/releases/latest"))
        .output()?;
    anyhow::ensure!(out.status.success(), "could not reach GitHub ({})", String::from_utf8_lossy(&out.stderr).trim());
    parse_release(&out.stdout).ok_or_else(|| anyhow::anyhow!("unexpected answer from GitHub"))
}

/// Download `url` to `dest` (replacing it).
pub fn download(url: &str, dest: &Path) -> anyhow::Result<()> {
    let _ = std::fs::remove_file(dest);
    let status = curl().args(["-H", "User-Agent: Anywhere-updater", "-o"]).arg(dest).arg(url).status()?;
    anyhow::ensure!(status.success() && dest.exists(), "download failed");
    Ok(())
}

/// Files that coordinate an update between the app and the Windows
/// sharing service, in the shared folder.
pub mod marker {
    use std::path::PathBuf;

    /// App → service: "a newer version exists and nobody is connected; please
    /// install it" (the service fetches it from GitHub itself; the file's
    /// content is ignored).
    pub fn request() -> PathBuf {
        crate::trust::system_dir().join("update-request")
    }
    /// Service → app: "installing now; please close".
    pub fn running() -> PathBuf {
        crate::trust::system_dir().join("update-running")
    }
    /// App → service: "I was open; open me again afterwards".
    pub fn relaunch() -> PathBuf {
        crate::trust::system_dir().join("relaunch-app")
    }
    /// Host → anyone: a viewer is connected right now (don't update).
    pub fn connected() -> PathBuf {
        crate::trust::host_dir().join("viewer-connected")
    }
    /// App → service: this computer is watching another one right now.
    pub fn viewing() -> PathBuf {
        crate::trust::system_dir().join("app-viewing")
    }
}

/// Is someone connected to this computer, or is it watching another one? (The host refreshes
/// its note every half minute; an older one was left by a crash.)
pub fn someone_connected() -> bool {
    fresh(&marker::connected()) || fresh(&marker::viewing())
}

fn fresh(p: &Path) -> bool {
    std::fs::metadata(p)
        .and_then(|m| m.modified())
        .is_ok_and(|t| t.elapsed().is_ok_and(|e| e < std::time::Duration::from_secs(120)))
}

/// Leave (or refresh, or remove) a note.
pub fn note(p: &Path, on: bool) {
    if on {
        let _ = std::fs::write(p, b"");
    } else {
        let _ = std::fs::remove_file(p);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_like_numbers() {
        assert!(is_newer("0.4.2", "0.4.1"));
        assert!(is_newer("v0.10.0", "0.9.9"));
        assert!(is_newer("1.0", "0.99.99"));
        assert!(!is_newer("0.4.1", "0.4.1"));
        assert!(!is_newer("0.4.0", "0.4.1"));
    }

    #[test]
    fn reads_githubs_answer() {
        let json = br#"{"tag_name":"v0.4.2","assets":[
            {"name":"Anywhere-0.4.2-mac.dmg","browser_download_url":"https://x/m.dmg"},
            {"name":"Anywhere-0.4.2-windows-setup.exe","browser_download_url":"https://x/w.exe"}]}"#;
        let r = parse_release(json).unwrap();
        assert_eq!(r.version, "0.4.2");
        assert_eq!(r.mac_url.as_deref(), Some("https://x/m.dmg"));
        assert_eq!(r.windows_url.as_deref(), Some("https://x/w.exe"));
        assert!(parse_release(b"{}").is_none());
    }
}
