//! Automatic updates, from the app's side.
//!
//! Every six hours (and shortly after opening) the app asks GitHub for the
//! newest version. When there is one and nobody is connected, it installs
//! it and reopens itself, minimised, sharing again if it was:
//!
//! - **Mac:** the new app replaces the old one in Applications (no password
//!   when you own it, which you do after dragging it there).
//! - **PC with "share at all times":** the sharing service (which runs as
//!   the system) downloads and installs it: no prompt at all.
//! - **PC without it:** the installer runs silently but Windows asks once
//!   to "allow changes", because installing into Program Files needs it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aa_platform::update::{self, Release};
use serde::{Deserialize, Serialize};

#[derive(Debug, Default)]
pub struct State {
    pub latest: Option<Release>,
    pub checked: bool,
    pub error: Option<String>,
}

#[derive(Debug, Default)]
pub struct Updater {
    pub state: Mutex<State>,
    pub check_now: AtomicBool,
}

impl Updater {
    pub fn available(&self) -> Option<Release> {
        let s = self.state.lock().ok()?;
        s.latest.clone().filter(|r| update::is_newer(&r.version, update::current()) && r.url_here().is_some())
    }
}

/// Check now and then every six hours, or when asked.
pub fn spawn_checker(u: Arc<Updater>, ctx: eframe::egui::Context) {
    let _ = std::thread::Builder::new().name("updates".into()).spawn(move || {
        std::thread::sleep(Duration::from_secs(15));
        loop {
            let r = update::latest();
            if let Ok(mut s) = u.state.lock() {
                s.checked = true;
                match r {
                    Ok(rel) => {
                        s.latest = Some(rel);
                        s.error = None;
                    }
                    Err(e) => s.error = Some(e.to_string()),
                }
            }
            ctx.request_repaint();
            // Six hours, or sooner when "Check now" is pressed.
            for _ in 0..(6 * 3600 / 5) {
                std::thread::sleep(Duration::from_secs(5));
                if u.check_now.swap(false, Ordering::Relaxed) {
                    break;
                }
            }
        }
    });
}

/// What to restore after the update (sharing on/off).
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Resume {
    pub sharing: bool,
}

fn resume_file() -> std::path::PathBuf {
    aa_platform::trust::user_dir().join("resume.json")
}

pub fn save_resume(r: &Resume) {
    if let Ok(b) = serde_json::to_vec(r) {
        let _ = std::fs::write(resume_file(), b);
    }
}

/// The saved state, once (the file is removed).
pub fn take_resume() -> Option<Resume> {
    let b = std::fs::read(resume_file()).ok()?;
    let _ = std::fs::remove_file(resume_file());
    serde_json::from_slice(&b).ok()
}

/// What happened when we tried.
#[derive(Debug)]
#[cfg_attr(not(any(target_os = "windows", target_os = "macos")), allow(dead_code))]
pub enum Applied {
    /// The new version is in place / installing: the app should quit now.
    Quit,
    /// The Windows service will do it; quit when it says so.
    Requested,
}

/// Install `rel`. Blocking (downloads); run it off the window's thread.
#[allow(clippy::needless_pass_by_value)]
pub fn apply(rel: Release, service_on: bool) -> anyhow::Result<Applied> {
    let url = rel.url_here().ok_or_else(|| anyhow::anyhow!("no installer for this system"))?.to_owned();
    let dir = aa_platform::trust::user_dir();
    #[cfg(target_os = "windows")]
    {
        if service_on && aa_platform::trust::system_dir().is_dir() {
            std::fs::write(update::marker::request(), rel.version.as_bytes())?;
            return Ok(Applied::Requested);
        }
        let setup = dir.join("Anywhere-update-setup.exe");
        update::download(&url, &setup)?;
        aa_platform::windows::session::run_elevated_with(
            &setup.display().to_string(),
            "/VERYSILENT /SUPPRESSMSGBOXES /NORESTART /SP- /RELAUNCH=1",
            false,
        )?;
        Ok(Applied::Quit)
    }
    #[cfg(target_os = "macos")]
    {
        let _ = service_on;
        let dmg = dir.join("Anywhere-update.dmg");
        update::download(&url, &dmg)?;
        crate::service::replace_app_bundle(&dmg)?;
        let _ = std::fs::remove_file(&dmg);
        Ok(Applied::Quit)
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        let _ = (service_on, url, dir);
        anyhow::bail!("updates are for the Mac and Windows apps")
    }
}
