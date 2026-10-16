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
            why: "Sharper picture for less data when watching another computer (otherwise H.264, ~40% more data)",
            ok: has_hevc_extension(),
            // The free "from Device Manufacturer" edition.
            url: "ms-windows-store://pdp/?ProductId=9N4WGH0Z6VHQ",
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

/// Can this PC decode HEVC? Asked of Windows' video decoders directly (the
/// Store add-on, or one built into the graphics driver, both count).
fn has_hevc_extension() -> bool {
    #[cfg(target_os = "windows")]
    {
        aa_platform::windows::decoder::available_codecs().contains(&aa_core::video::Codec::Hevc)
    }
    #[cfg(not(target_os = "windows"))]
    {
        true
    }
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
