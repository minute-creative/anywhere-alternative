//! The launcher list: apps and games on this computer, and opening, hiding
//! or quitting them on request (`aa_core::apps`).
//!
//! On a Mac the list comes from the application folders, Steam's library
//! and CrossOver's launchers; "running" from the process list; icons from
//! each app's `.icns` through `sips` (Apple's own image tool, no extra
//! permissions). Everything goes through small command-line tools rather
//! than private frameworks, so it works for the always-on host too.
//!
//! On a PC the list is the Start menu's shortcuts and Steam's library
//! (no icons yet: the viewer shows a lettered tile). Other systems (Linux
//! CI) return a pretend list so the flow can be tested.
//!
//! In plain words: this file answers "what can I open on that computer?"
//! and "open/quit this one" for the iPhone app's console-style menu.

#![allow(clippy::doc_markdown)] // app and tool names read better plain

use std::path::Path;

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
use aa_core::apps::AppKind;
use aa_core::apps::{AppAction, AppEntry, IconPack};

/// Everything the launcher can show, sorted: running first, then by name.
/// Never includes Anywhere itself.
pub fn list() -> Vec<AppEntry> {
    let mut items = list_here();
    items.retain(|a| !a.name.eq_ignore_ascii_case("anywhere") && !a.id.starts_with("com.minutecreative."));
    let recent = recent_ids();
    for a in &mut items {
        a.last_used = recent.iter().find(|(id, _)| *id == a.id).map_or(0, |(_, t)| *t);
    }
    items.sort_by(|a, b| b.running.cmp(&a.running).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
    items.dedup_by(|a, b| a.id == b.id);
    items
}

/// Do `action` to the app with `id`. Errors are logged, never fatal: a
/// launcher button that does nothing is annoying, a crashed host is worse.
pub fn act(id: &str, action: AppAction) {
    if action == AppAction::Open {
        remember_recent(id);
    }
    if let Err(e) = act_here(id, action) {
        tracing::warn!(id, ?action, "launcher action failed: {e}");
    }
}

/// Icons (PNG, 128 px) for these apps, for the one-off transfer to the
/// viewer. Slow (a tool run per icon the first time); call off the network
/// thread. Converted icons are cached in the data folder.
pub fn icons(ids: &[String]) -> IconPack {
    let mut pack = IconPack::default();
    for id in ids {
        if let Some(png) = icon_here(id) {
            pack.icons.push((id.clone(), png.into()));
        }
    }
    pack
}

// ---------------------------------------------------------------------------
// "Recent" row: the ids we opened from the launcher, newest first.
// ---------------------------------------------------------------------------

fn recent_ids() -> Vec<(String, u64)> {
    recent_ids_in(&crate::trust::user_dir())
}

fn remember_recent(id: &str) {
    remember_recent_in(&crate::trust::user_dir(), id);
}

fn recent_ids_in(dir: &Path) -> Vec<(String, u64)> {
    std::fs::read(dir.join("launcher-recent.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn remember_recent_in(dir: &Path, id: &str) {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let mut list = recent_ids_in(dir);
    list.retain(|(other, _)| other != id);
    list.insert(0, (id.to_owned(), now));
    list.truncate(24);
    let _ = std::fs::create_dir_all(dir);
    if let Ok(json) = serde_json::to_vec(&list) {
        let _ = std::fs::write(dir.join("launcher-recent.json"), json);
    }
}

// ---------------------------------------------------------------------------
// Steam's library, the same files on every system: `steamapps/
// appmanifest_<id>.acf` holds `"appid"` and `"name"`; `libraryfolders.vdf`
// lists extra library folders.
// ---------------------------------------------------------------------------

#[cfg(any(target_os = "macos", target_os = "windows"))]
mod steam {
    use std::path::{Path, PathBuf};

    use aa_core::apps::{AppEntry, AppKind};

    /// `"key"    "value"` → value, for Valve's key-value files.
    pub fn vdf_value(line: &str, key: &str) -> Option<String> {
        let line = line.trim();
        let rest = line.strip_prefix('"')?.strip_prefix(key)?.strip_prefix('"')?.trim_start();
        let v = rest.strip_prefix('"')?;
        let end = v.find('"')?;
        Some(v[..end].replace("\\\\", "\\"))
    }

    /// Installed games under `steam` (the Steam folder).
    pub fn games(steam: &Path) -> Vec<AppEntry> {
        let mut folders = vec![steam.join("steamapps")];
        if let Ok(vdf) = std::fs::read_to_string(steam.join("steamapps/libraryfolders.vdf")) {
            for line in vdf.lines() {
                if let Some(p) = vdf_value(line, "path") {
                    folders.push(PathBuf::from(p).join("steamapps"));
                }
            }
        }
        let mut out = Vec::new();
        for f in folders {
            let Ok(entries) = std::fs::read_dir(&f) else { continue };
            for e in entries.flatten() {
                let path = e.path();
                let is_manifest = path.file_name().is_some_and(|n| n.to_string_lossy().starts_with("appmanifest_"))
                    && path.extension().is_some_and(|x| x.eq_ignore_ascii_case("acf"));
                if !is_manifest {
                    continue;
                }
                let Ok(acf) = std::fs::read_to_string(&path) else { continue };
                let appid = acf.lines().find_map(|l| vdf_value(l, "appid"));
                let title = acf.lines().find_map(|l| vdf_value(l, "name"));
                if let (Some(appid), Some(title)) = (appid, title) {
                    // Steam's own tools (redistributables, Proton) are not games.
                    if title.contains("Redistributable")
                        || title.starts_with("Proton")
                        || title.starts_with("Steam Linux")
                    {
                        continue;
                    }
                    out.push(AppEntry {
                        id: format!("steam:{appid}"),
                        name: title,
                        kind: AppKind::Steam,
                        running: false,
                        last_used: 0,
                    });
                }
            }
        }
        out
    }

    /// The largest picture Steam cached for a game, if any (its layouts
    /// have changed over the years; try each). Mac only so far: the PC
    /// sends no icons yet.
    #[cfg(target_os = "macos")]
    pub fn picture(steam: &Path, appid: &str) -> Option<PathBuf> {
        let cache = steam.join("appcache/librarycache");
        let candidates = [
            cache.join(format!("{appid}_icon.jpg")),
            cache.join(format!("{appid}_library_600x900.jpg")),
            cache.join(appid).join("library_600x900.jpg"),
        ];
        if let Some(p) = candidates.iter().find(|p| p.exists()) {
            return Some(p.clone());
        }
        let mut best: Option<(u64, PathBuf)> = None;
        for e in std::fs::read_dir(cache.join(appid)).ok()?.flatten() {
            let p = e.path();
            let picture = p.extension().is_some_and(|x| x.eq_ignore_ascii_case("jpg") || x.eq_ignore_ascii_case("png"));
            if picture {
                let len = e.metadata().map_or(0, |m| m.len());
                if best.as_ref().map_or(true, |(l, _)| len > *l) {
                    best = Some((len, p));
                }
            }
        }
        best.map(|(_, p)| p)
    }

    #[cfg(test)]
    mod tests {
        #[test]
        fn vdf_lines_parse() {
            assert_eq!(super::vdf_value("\t\"appid\"\t\t\"1245620\"", "appid").as_deref(), Some("1245620"));
            assert_eq!(super::vdf_value("\"name\" \"Elden Ring\"", "name").as_deref(), Some("Elden Ring"));
            assert_eq!(super::vdf_value("\"name\" \"x\"", "appid"), None);
        }
    }
}

// ---------------------------------------------------------------------------
// macOS
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
mod mac {
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use aa_core::apps::{AppAction, AppEntry, AppKind};

    fn home() -> PathBuf {
        std::env::var_os("HOME").map_or_else(|| PathBuf::from("/"), PathBuf::from)
    }

    fn steam_dir() -> PathBuf {
        home().join("Library/Application Support/Steam")
    }

    /// Where apps live. `~/Applications/CrossOver` holds CrossOver's
    /// launchers for Windows programs, which `open` runs like any app.
    fn app_folders() -> Vec<(PathBuf, AppKind)> {
        let home = home();
        vec![
            (PathBuf::from("/Applications"), AppKind::App),
            (PathBuf::from("/Applications/Utilities"), AppKind::System),
            (PathBuf::from("/System/Applications"), AppKind::System),
            (PathBuf::from("/System/Applications/Utilities"), AppKind::System),
            (home.join("Applications"), AppKind::App),
            (home.join("Applications/CrossOver"), AppKind::CrossOver),
        ]
    }

    #[derive(Clone)]
    struct Bundle {
        path: PathBuf,
        id: String,
        name: String,
        icon_file: Option<String>,
    }

    /// Apps people would want to open from a menu: not background-only
    /// helpers, not menu-bar-only tools.
    fn read_bundle(path: &Path) -> Option<Bundle> {
        let plist = path.join("Contents/Info.plist");
        let out = Command::new("/usr/bin/plutil").args(["-convert", "json", "-o", "-"]).arg(&plist).output().ok()?;
        if !out.status.success() {
            return None;
        }
        let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
        let s = |k: &str| v.get(k).and_then(|x| x.as_str()).map(str::to_owned);
        let flag = |k: &str| v.get(k).is_some_and(|x| x.as_bool() == Some(true) || x.as_str() == Some("1"));
        if flag("LSUIElement") || flag("LSBackgroundOnly") {
            return None;
        }
        let id = s("CFBundleIdentifier")?;
        let stem = path.file_stem().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let name = s("CFBundleDisplayName").or_else(|| s("CFBundleName")).filter(|n| !n.is_empty()).unwrap_or(stem);
        let icon_file = s("CFBundleIconFile").or_else(|| s("CFBundleIconName"));
        Some(Bundle { path: path.to_path_buf(), id, name, icon_file })
    }

    /// Reading every app's Info.plist takes a second or two; the result is
    /// kept for a minute (apps are not installed that often), so the icon
    /// lookup and "open" don't scan the folders again for every app.
    fn bundles() -> Vec<(Bundle, AppKind)> {
        use std::sync::{Mutex, OnceLock};
        use std::time::{Duration, Instant};
        static CACHE: OnceLock<Mutex<Option<(Instant, Vec<(Bundle, AppKind)>)>>> = OnceLock::new();
        let cache = CACHE.get_or_init(|| Mutex::new(None));
        if let Ok(c) = cache.lock() {
            if let Some((at, list)) = c.as_ref() {
                if at.elapsed() < Duration::from_secs(60) {
                    return list.clone();
                }
            }
        }
        let fresh = scan_bundles();
        if let Ok(mut c) = cache.lock() {
            *c = Some((Instant::now(), fresh.clone()));
        }
        fresh
    }

    fn scan_bundles() -> Vec<(Bundle, AppKind)> {
        let mut out = Vec::new();
        for (folder, kind) in app_folders() {
            let Ok(entries) = std::fs::read_dir(&folder) else { continue };
            for e in entries.flatten() {
                let p = e.path();
                if p.extension().is_some_and(|x| x == "app") {
                    if let Some(b) = read_bundle(&p) {
                        out.push((b, kind));
                    }
                }
                if out.len() >= 400 {
                    return out;
                }
            }
        }
        out
    }

    /// Executable paths of everything running, for the "open" dot.
    fn running_paths() -> Vec<String> {
        Command::new("/bin/ps")
            .args(["-axo", "comm="])
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).lines().map(|l| l.trim().to_owned()).collect())
            .unwrap_or_default()
    }

    pub fn list_here() -> Vec<AppEntry> {
        let running = running_paths();
        let mut out: Vec<AppEntry> = bundles()
            .into_iter()
            .map(|(b, kind)| {
                let prefix = format!("{}/Contents/MacOS/", b.path.display());
                let is_running = running.iter().any(|p| p.starts_with(&prefix));
                AppEntry { id: b.id, name: b.name, kind, running: is_running, last_used: 0 }
            })
            .collect();
        out.extend(super::steam::games(&steam_dir()));
        out
    }

    fn bundle_path(id: &str) -> Option<Bundle> {
        bundles().into_iter().map(|(b, _)| b).find(|b| b.id == id)
    }

    fn run(cmd: &mut Command) -> anyhow::Result<()> {
        let out = cmd.output()?;
        if out.status.success() {
            Ok(())
        } else {
            anyhow::bail!("{}", String::from_utf8_lossy(&out.stderr).trim())
        }
    }

    pub fn act_here(id: &str, action: AppAction) -> anyhow::Result<()> {
        if let Some(appid) = id.strip_prefix("steam:") {
            let appid: u64 = appid.parse()?;
            // `open` hands the URL to Steam, which starts the game (and
            // Steam itself first if needed). Steam games have no bundle id
            // we know, so quitting goes through Steam too.
            let url = match action {
                AppAction::Open => format!("steam://rungameid/{appid}"),
                AppAction::Quit | AppAction::Hide => format!("steam://close/{appid}"),
            };
            return run(Command::new("/usr/bin/open").arg(url));
        }
        let b = bundle_path(id).ok_or_else(|| anyhow::anyhow!("no app with id {id}"))?;
        match action {
            // `open -a` starts the app or brings it to the front.
            AppAction::Open => run(Command::new("/usr/bin/open").arg("-a").arg(&b.path)),
            // AppleScript's "quit" is what Cmd+Q does: the app may ask to
            // save. Falls back to a polite TERM signal if scripting is
            // refused (macOS asks for Automation permission the first time;
            // the always-on host has nobody to answer it).
            AppAction::Quit => {
                let script = format!("tell application id \"{}\" to quit", b.id);
                if run(Command::new("/usr/bin/osascript").args(["-e", &script])).is_err() {
                    let prefix = format!("{}/Contents/MacOS/", b.path.display());
                    let pids = Command::new("/bin/ps").args(["-axo", "pid=,comm="]).output()?;
                    for line in String::from_utf8_lossy(&pids.stdout).lines() {
                        if let Some((pid, comm)) = line.trim().split_once(' ') {
                            if comm.trim().starts_with(&prefix) {
                                let _ = Command::new("/bin/kill").args(["-TERM", pid]).status();
                            }
                        }
                    }
                }
                Ok(())
            }
            AppAction::Hide => {
                let script = format!(
                    "tell application \"System Events\" to set visible of (first process whose bundle identifier is \"{}\") to false",
                    b.id
                );
                run(Command::new("/usr/bin/osascript").args(["-e", &script]))
            }
        }
    }

    /// The app's icon as a 128 px PNG, converted once and cached.
    pub fn icon_here(id: &str) -> Option<Vec<u8>> {
        let cache_dir = crate::trust::user_dir().join("icon-cache");
        let _ = std::fs::create_dir_all(&cache_dir);
        let safe: String = id.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect();
        let cached = cache_dir.join(format!("{safe}.png"));
        let source = icon_source(id)?;
        let fresh = match (std::fs::metadata(&cached), std::fs::metadata(&source)) {
            (Ok(c), Ok(s)) => c.modified().ok() >= s.modified().ok(),
            _ => false,
        };
        if !fresh {
            // sips: Apple's image tool. -Z 128 scales the biggest
            // representation to 128 px; the output is a plain PNG.
            let ok = Command::new("/usr/bin/sips")
                .args(["-s", "format", "png", "-Z", "128"])
                .arg(&source)
                .arg("--out")
                .arg(&cached)
                .output()
                .is_ok_and(|o| o.status.success());
            if !ok {
                return None;
            }
        }
        std::fs::read(&cached).ok().filter(|b| b.len() > 8 && b.len() < 512 * 1024)
    }

    /// The image file an app's icon comes from.
    fn icon_source(id: &str) -> Option<PathBuf> {
        if let Some(appid) = id.strip_prefix("steam:") {
            return super::steam::picture(&steam_dir(), appid);
        }
        let b = bundle_path(id)?;
        let res = b.path.join("Contents/Resources");
        let mut names = Vec::new();
        if let Some(f) = &b.icon_file {
            names.push(f.clone());
            if Path::new(f).extension().is_none() {
                names.push(format!("{f}.icns"));
            }
        }
        names.push("AppIcon.icns".into());
        names.into_iter().map(|n| res.join(n)).find(|p| p.exists())
    }
}

#[cfg(target_os = "macos")]
use mac::{act_here, icon_here, list_here};

// ---------------------------------------------------------------------------
// Windows: the Start menu's shortcuts and Steam. Opening goes through
// `cmd /c start`, which runs shortcuts and `steam://` links alike.
// ---------------------------------------------------------------------------

#[cfg(target_os = "windows")]
mod win {
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use aa_core::apps::{AppAction, AppEntry, AppKind};

    fn start_menus() -> Vec<PathBuf> {
        let mut out = Vec::new();
        if let Some(p) = std::env::var_os("ProgramData") {
            out.push(PathBuf::from(p).join(r"Microsoft\Windows\Start Menu\Programs"));
        }
        if let Some(p) = std::env::var_os("APPDATA") {
            out.push(PathBuf::from(p).join(r"Microsoft\Windows\Start Menu\Programs"));
        }
        out
    }

    fn steam_dir() -> PathBuf {
        let pf = std::env::var_os("ProgramFiles(x86)").or_else(|| std::env::var_os("ProgramFiles"));
        pf.map_or_else(|| PathBuf::from(r"C:\Program Files (x86)\Steam"), |p| PathBuf::from(p).join("Steam"))
    }

    /// Shortcuts two folders deep (the Start menu is "Programs/<Vendor>/App.lnk").
    fn shortcuts(dir: &Path, depth: u8, out: &mut Vec<AppEntry>) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                if depth > 0 {
                    shortcuts(&p, depth - 1, out);
                }
                continue;
            }
            if !p.extension().is_some_and(|x| x.eq_ignore_ascii_case("lnk")) {
                continue;
            }
            let name = p.file_stem().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let lower = name.to_lowercase();
            // Noise nobody opens from a game menu.
            if lower.contains("uninstall")
                || lower.contains("readme")
                || lower.contains("help")
                || lower.contains("website")
            {
                continue;
            }
            let folder = dir.to_string_lossy();
            let kind = if folder.contains("Accessories") || folder.contains("System Tools") {
                AppKind::System
            } else {
                AppKind::App
            };
            out.push(AppEntry { id: format!("lnk:{}", p.display()), name, kind, running: false, last_used: 0 });
            if out.len() >= 400 {
                return;
            }
        }
    }

    pub fn list_here() -> Vec<AppEntry> {
        let mut out = Vec::new();
        for m in start_menus() {
            shortcuts(&m, 1, &mut out);
        }
        out.extend(super::steam::games(&steam_dir()));
        out
    }

    pub fn act_here(id: &str, action: AppAction) -> anyhow::Result<()> {
        let target = if let Some(appid) = id.strip_prefix("steam:") {
            let appid: u64 = appid.parse()?;
            match action {
                AppAction::Open => format!("steam://rungameid/{appid}"),
                AppAction::Quit | AppAction::Hide => format!("steam://close/{appid}"),
            }
        } else if let Some(path) = id.strip_prefix("lnk:") {
            anyhow::ensure!(action == AppAction::Open, "only opening is supported for shortcuts on a PC yet");
            path.to_owned()
        } else {
            anyhow::bail!("unknown launcher id {id}");
        };
        // `start ""`: the first quoted argument is the window title.
        let status = Command::new("cmd").args(["/c", "start", "", &target]).status()?;
        anyhow::ensure!(status.success(), "start failed with {status}");
        Ok(())
    }

    pub fn icon_here(_id: &str) -> Option<Vec<u8>> {
        None
    }
}

#[cfg(target_os = "windows")]
use win::{act_here, icon_here, list_here};

// ---------------------------------------------------------------------------
// Anywhere else (Linux CI, the mock host): a pretend list, so the viewer
// side and the protocol can be tested end to end.
// ---------------------------------------------------------------------------

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn list_here() -> Vec<AppEntry> {
    let e = |id: &str, name: &str, kind, running| AppEntry {
        id: id.into(),
        name: name.into(),
        kind,
        running,
        last_used: 0,
    };
    vec![
        e("test.editor", "Test Editor", AppKind::App, true),
        e("test.browser", "Test Browser", AppKind::App, false),
        e("steam:1245620", "Elden Ring", AppKind::Steam, false),
        e("test.finder", "Finder", AppKind::System, true),
        e("test.terminal", "Terminal", AppKind::System, false),
        e("test.crossover", "Valorant (CrossOver)", AppKind::CrossOver, false),
    ]
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn act_here(id: &str, action: AppAction) -> anyhow::Result<()> {
    anyhow::ensure!(list_here().iter().any(|a| a.id == id), "no app with id {id}");
    tracing::info!(id, ?action, "mock launcher action");
    Ok(())
}

/// A 1x1 PNG, so icon transfer can be tested without real icons.
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn icon_here(id: &str) -> Option<Vec<u8>> {
    (id != "test.terminal").then(|| crate::clipboard::rgba_to_png(1, 1, &[60, 120, 240, 255])).flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recent_is_remembered_newest_first() {
        let dir = std::env::temp_dir().join(format!("aa-apps-test-{}", std::process::id()));
        remember_recent_in(&dir, "a");
        remember_recent_in(&dir, "b");
        remember_recent_in(&dir, "a");
        let ids: Vec<String> = recent_ids_in(&dir).into_iter().map(|(id, _)| id).collect();
        assert_eq!(ids, ["a", "b"]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    #[test]
    fn mock_list_is_sorted_running_first() {
        let l = list();
        assert!(l[0].running && !l[l.len() - 1].running);
        assert_eq!(icons(&["test.editor".into(), "test.terminal".into()]).icons.len(), 1);
    }
}
