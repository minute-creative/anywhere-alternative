//! The one-time "update the other computer" note in the stream window.
//!
//! In plain words: when the two computers run different versions, the
//! stream still works (both speak the same protocol since 0.4.0), but the
//! older one misses fixes. So once per new version, the stream window shows
//! a small note in the bottom-right corner, "The PC has Anywhere 0.5.7 …
//! update it for:" and what changed, with an X to close it. It never comes
//! back for that version.
//!
//! The host's version arrives in `Welcome` (0.5.9 on; older hosts send
//! none, which counts as "older than 0.5.9").

/// What changed, newest first, in words for the person using it. Add the
/// new version's lines here with each release.
const CHANGES: &[(&str, &[&str])] = &[
    (
        "0.6.0",
        &[
            "iPhone app: watch and use the Mac from the phone with a clipped-on controller",
            "Launcher: the Mac's apps and games as a console-style menu (needs 0.6 on the Mac)",
        ],
    ),
    ("0.5.13", &["Sharper picture on slow Wi-Fi: a smaller, crisp picture instead of a big blurry one"]),
    (
        "0.5.12",
        &[
            "Smoother games: the picture gets smaller for a moment when the PC can't keep up",
            "PC updates no longer get stuck on \"Installing the update\"",
        ],
    ),
    ("0.5.11", &["Updates on a PC can no longer leave Anywhere closed"]),
    (
        "0.5.10",
        &[
            "Fixed the stream window using more and more memory while hidden",
            "Game mouse turns on by itself when a game takes the pointer",
            "Smoother streaming while a game uses the whole graphics chip",
            "Usage numbers switch moved to the bottom of Settings",
        ],
    ),
    (
        "0.5.9",
        &[
            "Black out the PC's screen while you work on it",
            "Holding a key repeats (arrows, Delete)",
            "Picture recovers quickly after a network hiccup",
            "Warns when Valorant's anti-cheat blocks clicks",
            "Can connect from other networks when the router allows it",
        ],
    ),
    (
        "0.5.8",
        &[
            "Sharper picture when the window is bigger than the screen",
            "Windows key and Alt+Tab go to the stream in full screen",
            "No more stuck keys or controller buttons",
            "Reconnects faster after a drop",
        ],
    ),
];

/// At most this many lines, so the note stays small.
const MAX_LINES: usize = 5;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateNote {
    pub title: String,
    pub text: String,
    pub changes: Vec<String>,
    /// The newer version: the note is shown once per this.
    pub newer: String,
}

/// The note for `host_version` (empty: older than 0.5.9) seen from a viewer
/// at `mine`, or None when both run the same version.
pub fn make(host_version: &str, mine: &str, host_is_mac: bool) -> Option<UpdateNote> {
    use aa_platform::update::is_newer;
    let (host, here) = if host_is_mac { ("Mac", "PC") } else { ("PC", "Mac") };
    let here = if cfg!(target_os = "windows") || cfg!(target_os = "macos") { here } else { "computer" };
    let host_shown =
        if host_version.is_empty() { "an older Anywhere".to_owned() } else { format!("Anywhere {host_version}") };
    let (older, newer, title, text) = if host_version.is_empty() || is_newer(mine, host_version) {
        (
            host_version,
            mine,
            format!("Update the {host}"),
            format!("The {host} has {host_shown} and this {here} has {mine}. It works, but update the {host} for:"),
        )
    } else if is_newer(host_version, mine) {
        (
            mine,
            host_version,
            format!("Update this {here}"),
            format!("The {host} has Anywhere {host_version} and this {here} has {mine}. It works, but update this {here} for:"),
        )
    } else {
        return None;
    };
    let changes: Vec<String> = CHANGES
        .iter()
        .filter(|(v, _)| !is_newer(v, newer) && (older.is_empty() || is_newer(v, older)))
        .flat_map(|(_, lines)| lines.iter().map(|l| (*l).to_owned()))
        .take(MAX_LINES)
        .collect();
    Some(UpdateNote { title, text, changes, newer: newer.to_owned() })
}

/// [`make`], but only the first time for this newer version (remembered in
/// a small file next to the settings).
pub fn once(host_version: &str, host_is_mac: bool) -> Option<UpdateNote> {
    let note = make(host_version, aa_platform::update::current(), host_is_mac)?;
    let path = aa_platform::trust::user_dir().join("update-note-seen");
    if std::fs::read_to_string(&path).is_ok_and(|v| v.trim() == note.newer) {
        return None;
    }
    let _ = std::fs::write(&path, &note.newer);
    tracing::info!(host_version, "showing the update note once");
    Some(note)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_version_says_nothing() {
        assert_eq!(make("0.5.9", "0.5.9", false), None);
    }

    #[test]
    fn older_host_gets_the_newer_changes_only() {
        let n = make("0.5.8", "0.5.9", false).unwrap();
        assert!(n.title.starts_with("Update the PC"), "{}", n.title);
        assert!(n.text.contains("0.5.8"));
        assert_eq!(n.changes.len(), 5);
        assert!(n.changes.iter().any(|c| c.contains("Black out")));
        assert!(!n.changes.iter().any(|c| c.contains("Reconnects faster")));
        assert_eq!(n.newer, "0.5.9");
    }

    #[test]
    fn older_viewer_is_told_to_update_itself() {
        let n = make("0.6.0", "0.5.9", true).unwrap();
        assert!(n.title.starts_with("Update this"), "{}", n.title);
        assert_eq!(n.newer, "0.6.0");
    }

    #[test]
    fn host_without_a_version_is_older() {
        let n = make("", "0.5.9", false).unwrap();
        assert!(n.text.contains("an older Anywhere"));
        assert!(n.changes.len() <= MAX_LINES);
    }
}
