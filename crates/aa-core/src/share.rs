//! What the sharing computer shows: the whole screen, one app, or one window.
//!
//! The choice is made by the person sitting at the sharing computer (in the
//! app's Share page), or by a viewer that was given "Full access". Without
//! full access, a viewer watching one app can only click and type inside
//! that app, and only hears that app's sound.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ShareTarget {
    /// Everything on the main screen (the default, and always the choice
    /// when nobody is sitting at the sharing computer).
    #[default]
    Screen,
    /// Every window of one app, with its menus and pop-ups. `id` is the
    /// Mac bundle id (`com.apple.Safari`) or the Windows program file name
    /// (`chrome.exe`), so an app that is restarted is still the same app.
    App { id: String, name: String },
    /// One window. `id` is the system's window number; `app` and `title`
    /// are for showing in lists.
    Window { id: u64, app: String, title: String },
}

impl ShareTarget {
    /// For lists and status lines: "Whole screen", "Safari", "Safari: Inbox".
    pub fn label(&self) -> String {
        match self {
            Self::Screen => "Whole screen".into(),
            Self::App { name, .. } => name.clone(),
            Self::Window { app, title, .. } if title.is_empty() => app.clone(),
            Self::Window { app, title, .. } => format!("{app}: {title}"),
        }
    }

    pub fn is_screen(&self) -> bool {
        matches!(self, Self::Screen)
    }
}

/// The share choice saved by the app for the sharing side to follow
/// (`share.json` next to the pairing files). The app rewrites it every
/// little while; an old file (the app was closed or crashed) means "whole
/// screen", so a computer left alone always shares everything, as asked.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ShareChoice {
    pub target: ShareTarget,
    /// Seconds since 1970 when the app last wrote this.
    #[serde(default)]
    pub written: u64,
}

impl ShareChoice {
    /// How long a choice stays valid without the app refreshing it.
    pub const FRESH_FOR_SECS: u64 = 120;

    // Fresh (written by the app in the last 2 minutes): use it. Stale: whole screen.
    pub fn target_at(&self, now: u64) -> ShareTarget {
        if now.saturating_sub(self.written) <= Self::FRESH_FOR_SECS {
            self.target.clone()
        } else {
            ShareTarget::Screen
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_and_round_trip() {
        let w = ShareTarget::Window { id: 7, app: "Safari".into(), title: "Inbox".into() };
        assert_eq!(w.label(), "Safari: Inbox");
        let json = serde_json::to_string(&w).unwrap();
        assert_eq!(serde_json::from_str::<ShareTarget>(&json).unwrap(), w);
        assert_eq!(ShareTarget::default().label(), "Whole screen");
    }

    #[test]
    fn a_stale_choice_means_whole_screen() {
        let c = ShareChoice { target: ShareTarget::App { id: "x".into(), name: "X".into() }, written: 1000 };
        assert!(!c.target_at(1100).is_screen());
        assert!(c.target_at(1000 + ShareChoice::FRESH_FOR_SECS + 1).is_screen());
    }
}
