//! The apps and windows that could be shared, for the Share page's lists
//! and for a viewer with "Full access".

use aa_core::share::ShareTarget;

/// Apps first (sorted by name), then their windows. Only ordinary apps and
/// windows a person would recognise: no menu bar items, no invisible
/// helper windows, not Anywhere itself.
pub fn list() -> Vec<ShareTarget> {
    let mut items = list_here();
    items.sort_by_key(|t| match t {
        ShareTarget::Screen => (0, String::new()),
        ShareTarget::App { name, .. } => (1, name.to_lowercase()),
        ShareTarget::Window { app, title, .. } => (2, format!("{}\u{0}{}", app.to_lowercase(), title.to_lowercase())),
    });
    items.dedup();
    items
}

#[cfg(target_os = "macos")]
fn list_here() -> Vec<ShareTarget> {
    crate::macos::capture::shareable().unwrap_or_else(|e| {
        tracing::warn!("could not list apps and windows: {e}");
        Vec::new()
    })
}

#[cfg(target_os = "windows")]
fn list_here() -> Vec<ShareTarget> {
    crate::windows::share::shareable()
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn list_here() -> Vec<ShareTarget> {
    crate::mock::shareable()
}

/// Is `target` still there (the app still open, the window not closed)?
pub fn still_open(target: &ShareTarget) -> bool {
    match target {
        ShareTarget::Screen => true,
        ShareTarget::App { id, .. } => {
            list().iter().any(|t| matches!(t, ShareTarget::App { id: other, .. } if other == id))
        }
        ShareTarget::Window { id, .. } => {
            list().iter().any(|t| matches!(t, ShareTarget::Window { id: other, .. } if other == id))
        }
    }
}
