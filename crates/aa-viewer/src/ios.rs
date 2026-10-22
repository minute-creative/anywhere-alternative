//! iPhone/iPad entry point. The app's tiny Objective-C `main` (in
//! `packaging/ios`) calls [`aa_viewer_ios_main`], which never returns: winit
//! takes over the main thread with `UIApplicationMain`, like on every OS.
//!
//! Settings come from the iOS Settings app (Settings > Anywhere), so the
//! first version needs no start screen of its own:
//! - `host`: the Mac or PC to watch. Its Tailscale address (100.x.y.z), or
//!   its home-network address. iOS doesn't let ordinary apps find computers
//!   by broadcast, so the address is typed once.
//! - `pair_code`: the six-digit code from the other computer's Share page.
//!   Used once, then cleared: after pairing, the phone is remembered.
//! - `resolution`: picture height (720 or 1080).
//! - `max_mbps`: optional cap for mobile data.
//!
//! Pairing keys live in `Documents/AnywhereData`, the log in
//! `Documents/viewer.log` (visible in the Files app, for debugging).
//!
//! In plain words: this is what runs when you tap the Anywhere icon on the
//! phone. It reads what you typed in Settings, then starts the same viewer
//! the Mac uses.

use std::path::PathBuf;

use objc2_foundation::{ns_string, NSString, NSUserDefaults};

fn documents() -> PathBuf {
    std::env::var_os("HOME").map_or_else(|| PathBuf::from("."), |h| PathBuf::from(h).join("Documents"))
}

fn setting(defaults: &NSUserDefaults, key: &NSString) -> Option<String> {
    let v = defaults.stringForKey(key)?.to_string();
    let v = v.trim().to_owned();
    (!v.is_empty()).then_some(v)
}

fn start_logging() {
    let path = documents().join("viewer.log");
    let file = std::fs::OpenOptions::new().create(true).append(true).open(&path);
    // winit logs a line for every frame on iOS ("processing non
    // RedrawRequested event..."); 1.7 MB in ten minutes on the first test.
    let filter = tracing_subscriber::EnvFilter::new("info,winit=error");
    match file {
        Ok(f) => tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_ansi(false)
            .with_writer(std::sync::Mutex::new(f))
            .init(),
        Err(_) => tracing_subscriber::fmt().with_env_filter(filter).with_ansi(false).init(),
    }
}

/// Called once by the app's `main`. Never returns.
#[no_mangle]
pub extern "C" fn aa_viewer_ios_main() {
    let data = documents().join("AnywhereData");
    let _ = std::fs::create_dir_all(&data);
    // `aa_platform::trust` keeps pairing keys wherever this points.
    std::env::set_var("AA_DATA_DIR", &data);
    start_logging();
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "Anywhere for iPhone starting");

    let defaults = NSUserDefaults::standardUserDefaults();
    let Some(host) = setting(&defaults, ns_string!("host")) else {
        tracing::error!("no host set: open Settings > Anywhere and type the computer's address");
        std::process::exit(2);
    };
    let mut words: Vec<String> = vec!["aa-viewer".into(), host.clone(), "--fullscreen".into()];
    let resolution = setting(&defaults, ns_string!("resolution")).unwrap_or_else(|| "1080".into());
    words.extend(["--resolution".into(), resolution]);
    if let Some(mbps) = setting(&defaults, ns_string!("max_mbps")) {
        words.extend(["--max-mbps".into(), mbps]);
    }
    let code = setting(&defaults, ns_string!("pair_code"));
    if code.is_some() {
        // One use only: the computer makes a new code after each pairing.
        // Saved to disk now, not "soon": iOS writes settings a moment later,
        // and an app that quit quickly kept the used code and kept retrying
        // it ("That code isn't right" on every start; Simulator, 2026-10-20).
        defaults.removeObjectForKey(ns_string!("pair_code"));
        #[allow(deprecated)]
        defaults.synchronize();
    }
    tracing::info!(host, "connecting");
    let mut with_code = words.clone();
    if let Some(c) = &code {
        with_code.extend(["--pair".into(), c.clone()]);
    }
    // `run_with_args` only comes back if something failed before the window
    // opened (on success the window keeps the app until it closes).
    if let Err(e) = crate::run_with_args(with_code) {
        if code.is_none() {
            tracing::error!("viewer stopped: {e:#}");
            std::process::exit(1);
        }
        // A code that didn't work may simply be an old one, while this phone
        // is paired already: try once more without it.
        tracing::warn!("pairing failed ({e:#}); trying as an already-paired phone");
        if let Err(e) = crate::run_with_args(words) {
            tracing::error!("viewer stopped: {e:#}");
            std::process::exit(1);
        }
    }
    std::process::exit(0);
}
