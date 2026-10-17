//! The "Anywhere Camera" browser add-on: where the app keeps it, and
//! getting it into Chrome, Edge and Safari.
//!
//! - Chrome and Edge, until the add-on is published in their stores: the
//!   person loads the folder once ("Load unpacked" in developer mode); the
//!   folder sits inside the installed app, so every update of Anywhere also
//!   updates the add-on.
//! - Chrome and Edge, once published (`aa_core::calls::*_STORE_ID` filled
//!   in): the app tells the browser about it the official way for programs
//!   ("external extensions"); the browser then asks "Enable?" once and keeps
//!   it updated from the store.
//! - Safari: the extension ships inside Anywhere.app; we only make sure
//!   macOS has noticed it. Safari shows it while "Allow Unsigned
//!   Extensions" is ticked (Apple signs extensions only for paid accounts).

use std::path::PathBuf;

/// The unpacked add-on folder inside the installed app.
pub fn folder() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    let candidates = [
        // Mac: Anywhere.app/Contents/MacOS/anywhere → Contents/Resources.
        dir.join("../Resources/browser-addon"),
        // Windows: next to anywhere.exe.
        dir.join("browser-addon"),
        // Running from the source tree.
        dir.join("../../dist/addon/chrome"),
    ];
    candidates.into_iter().find(|p| p.join("manifest.json").exists()).and_then(|p| p.canonicalize().ok())
}

/// The Safari extension inside the Mac app, if this build has one.
#[cfg(target_os = "macos")]
pub fn safari_extension() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let plugins = exe.parent()?.join("../PlugIns");
    std::fs::read_dir(plugins).ok()?.flatten().map(|e| e.path()).find(|p| p.extension().is_some_and(|x| x == "appex"))
}

/// Make sure macOS (and so Safari) knows about our Safari extension.
/// Harmless to repeat; runs once per app start in the background.
pub fn register() {
    #[cfg(target_os = "macos")]
    if let Some(x) = safari_extension() {
        let _ = std::process::Command::new("/usr/bin/pluginkit").arg("-a").arg(&x).status();
    }
    offer_from_stores();
}

/// Once the add-on is in the Chrome Web Store / Edge Add-ons: tell the
/// browsers it is wanted. They show "New extension added" with an Enable
/// button the next time they start.
fn offer_from_stores() {
    const CHROME_UPDATES: &str = "https://clients2.google.com/service/update2/crx";
    const EDGE_UPDATES: &str = "https://edge.microsoft.com/extensionwebstorebase/v1/crx";
    let wanted = [
        (aa_core::calls::CHROME_STORE_ID, CHROME_UPDATES, "Google/Chrome", "Software\\Google\\Chrome\\Extensions"),
        (aa_core::calls::EDGE_STORE_ID, EDGE_UPDATES, "Microsoft Edge", "Software\\Microsoft\\Edge\\Extensions"),
    ];
    for (id, url, mac_dir, win_key) in wanted {
        let Some(id) = id else { continue };
        let _ = (&mac_dir, &win_key); // each is used on one system only
        #[cfg(target_os = "macos")]
        {
            let Some(home) = std::env::var_os("HOME") else { continue };
            let dir = PathBuf::from(home).join("Library/Application Support").join(mac_dir).join("External Extensions");
            let file = dir.join(format!("{id}.json"));
            if !file.exists() {
                let _ = std::fs::create_dir_all(&dir);
                let _ = std::fs::write(&file, format!("{{\"external_update_url\": \"{url}\"}}\n"));
            }
        }
        #[cfg(target_os = "windows")]
        {
            let key = format!("HKCU\\{win_key}\\{id}");
            let mut cmd = std::process::Command::new("reg");
            cmd.args(["add", &key, "/v", "update_url", "/t", "REG_SZ", "/d", url, "/f"]);
            {
                use std::os::windows::process::CommandExt;
                cmd.creation_flags(0x0800_0000); // no console window
            }
            let _ = cmd.status();
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        let _ = (id, url);
    }
}

/// Show the add-on folder in Finder / Explorer (for "Load unpacked").
pub fn reveal_folder() {
    let Some(dir) = folder() else { return };
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(&dir).spawn();
    #[cfg(target_os = "windows")]
    let _ = std::process::Command::new("explorer").arg(&dir).spawn();
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let _ = std::process::Command::new("xdg-open").arg(&dir).spawn();
}
