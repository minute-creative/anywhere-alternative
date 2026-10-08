//! The setup checklist: optional helpers each feature needs, whether they
//! are installed, and where to get them. Checked by looking for the files
//! they install, which needs no special rights.

use std::path::Path;

#[derive(Debug, Clone)]
pub struct Check {
    pub name: &'static str,
    pub why: &'static str,
    pub ok: bool,
    pub url: &'static str,
}

fn exists(p: &str) -> bool {
    Path::new(p).exists()
}

/// What this computer needs, for the platform it is.
pub fn run() -> Vec<Check> {
    let mut v = Vec::new();
    if cfg!(target_os = "windows") {
        let sys = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
        v.push(Check {
            name: "ViGEmBus",
            why: "Controllers when others connect to this PC",
            ok: exists(&format!(r"{sys}\System32\drivers\ViGEmBus.sys")),
            url: "https://github.com/nefarius/ViGEmBus/releases",
        });
        v.push(Check {
            name: "usbip-win2",
            why: "Full DualSense: adaptive triggers, haptics, touchpad, motion",
            ok: exists(r"C:\Program Files\USBip\usbip.exe"),
            url: "https://github.com/vadimgrn/usbip-win2/releases",
        });
        v.push(Check {
            name: "VB-CABLE",
            why: "The viewer's microphone as a microphone on this PC",
            ok: exists(&format!(r"{sys}\System32\drivers\vbaudio_cable64_win10.sys"))
                || exists(&format!(r"{sys}\System32\drivers\vbaudio_cable64_win7.sys"))
                || exists(r"C:\Program Files\VB\CABLE"),
            url: "https://vb-audio.com/Cable/",
        });
        v.push(Check {
            name: "HEVC Video Extensions",
            why: "Sharper video from a Mac (otherwise H.264 is used)",
            ok: has_hevc_extension(),
            url: "ms-windows-store://pdp/?ProductId=9NMZLZ57R3T7",
        });
    }
    v.push(Check {
        name: "Tailscale",
        why: "Reach your computers from anywhere, not only on the same Wi-Fi (free)",
        ok: aa_platform::tailscale::installed(),
        url: "https://tailscale.com/download",
    });
    if cfg!(target_os = "macos") {
        v.push(Check {
            name: "BlackHole 2ch",
            why: "The viewer's microphone as a microphone on this Mac",
            ok: exists("/Library/Audio/Plug-Ins/HAL/BlackHole2ch.driver"),
            url: "https://existential.audio/blackhole/",
        });
    }
    v
}

/// The Store's HEVC add-on lives in `WindowsApps` under a known prefix.
fn has_hevc_extension() -> bool {
    let root = std::env::var("ProgramFiles").unwrap_or_else(|_| r"C:\Program Files".into());
    // The folder is private to Windows; if we can't look, don't nag.
    std::fs::read_dir(Path::new(&root).join("WindowsApps")).map_or(true, |d| {
        d.flatten().any(|e| e.file_name().to_string_lossy().starts_with("Microsoft.HEVCVideoExtension"))
    })
}

/// Open a web page (or Store link) in the default browser.
pub fn open_url(url: &str) {
    #[cfg(target_os = "windows")]
    let r = std::process::Command::new("cmd").args(["/C", "start", "", url]).spawn();
    #[cfg(target_os = "macos")]
    let r = std::process::Command::new("open").arg(url).spawn();
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let r = std::process::Command::new("xdg-open").arg(url).spawn();
    if let Err(e) = r {
        tracing::warn!("could not open {url}: {e}");
    }
}

/// Open the macOS privacy settings page for a permission.
pub fn open_privacy(pane: &str) {
    open_url(&format!("x-apple.systempreferences:com.apple.preference.security?{pane}"));
}
