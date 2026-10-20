//! The launcher: which apps and games the sharing computer has, so a viewer
//! can open, switch to and quit them from a console-style menu without a
//! mouse (the iPhone app with a clipped-on controller).
//!
//! The host answers [`crate::control::ControlMessage::AppList`] with
//! `Apps` pages of [`AppEntry`] (small JSON, one datagram each) and then
//! sends the icons as one [`IconPack`] through the reliable clipboard
//! transfer (`ClipItem::Blob`), because a PNG icon is far bigger than a
//! datagram and must arrive whole.
//!
//! In plain words: the Mac tells the phone "here are your apps and games";
//! the phone shows them as big tiles; pressing a tile tells the Mac to open
//! that one.

#![allow(clippy::doc_markdown)] // app names read better plain

use bytes::{Buf, BufMut, Bytes, BytesMut};
use serde::{Deserialize, Serialize};

/// Where an entry comes from; decides how the host opens it and which row
/// of the launcher it sits in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AppKind {
    /// An ordinary application (`/Applications/Safari.app`).
    #[default]
    App,
    /// A Steam game, opened through `steam://rungameid/<id>`.
    Steam,
    /// A Windows program run through CrossOver (a launcher in
    /// `~/Applications/CrossOver`).
    CrossOver,
    /// Something the system provides (Finder, System Settings, a Terminal).
    System,
}

/// One tile in the launcher.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AppEntry {
    /// Stable id: the bundle id for apps (`com.apple.Safari`), `steam:<appid>`
    /// for Steam games. What the viewer sends back to open it.
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub kind: AppKind,
    /// Open right now on the host (shown with a dot; "switch to" rather
    /// than "open").
    #[serde(default)]
    pub running: bool,
    /// Seconds since 1970 of the last time it was opened here (0 = unknown),
    /// so the launcher can show a "Recent" row.
    #[serde(default)]
    pub last_used: u64,
}

/// What the viewer may ask the host to do with one app.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AppAction {
    /// Open it, or bring it to the front if it is already open.
    Open,
    /// Ask it to quit (like Cmd+Q).
    Quit,
    /// Hide it (like Cmd+H).
    Hide,
}

/// How many entries go in one `Apps` page: keeps each page comfortably
/// inside one datagram (an entry is ~100 bytes of JSON).
pub const PAGE_SIZE: usize = 8;

/// Split a list into `Apps` pages.
pub fn pages(apps: &[AppEntry]) -> Vec<Vec<AppEntry>> {
    if apps.is_empty() {
        return vec![Vec::new()];
    }
    apps.chunks(PAGE_SIZE).map(<[AppEntry]>::to_vec).collect()
}

/// Icons for a list of apps, packed into one blob for the clipboard
/// transfer. Layout, repeated per icon: `u16` id length, the id, `u32`
/// PNG length, the PNG bytes. Apps without an icon are simply absent.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct IconPack {
    pub icons: Vec<(String, Bytes)>,
}

impl IconPack {
    /// The blob name the clipboard transfer carries, so the receiver knows
    /// this is not something to paste.
    pub const BLOB_NAME: &'static str = "app-icons";

    pub fn encode(&self) -> Bytes {
        let mut out = BytesMut::new();
        for (id, png) in &self.icons {
            let Ok(id_len) = u16::try_from(id.len()) else { continue };
            let Ok(png_len) = u32::try_from(png.len()) else { continue };
            out.put_u16(id_len);
            out.extend_from_slice(id.as_bytes());
            out.put_u32(png_len);
            out.extend_from_slice(png);
        }
        out.freeze()
    }

    pub fn decode(mut bytes: Bytes) -> Self {
        let mut icons = Vec::new();
        while bytes.remaining() >= 2 {
            let id_len = usize::from(bytes.get_u16());
            if bytes.remaining() < id_len + 4 {
                break;
            }
            let id = String::from_utf8_lossy(&bytes.split_to(id_len)).into_owned();
            let png_len = bytes.get_u32() as usize;
            if bytes.remaining() < png_len {
                break;
            }
            icons.push((id, bytes.split_to(png_len)));
        }
        Self { icons }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icon_pack_round_trips() {
        let pack = IconPack {
            icons: vec![
                ("com.apple.Safari".into(), Bytes::from_static(b"\x89PNG..")),
                ("steam:1245620".into(), Bytes::new()),
            ],
        };
        assert_eq!(IconPack::decode(pack.encode()), pack);
        assert_eq!(IconPack::decode(Bytes::from_static(b"\x00\x05ab")).icons.len(), 0, "a cut-off pack is ignored");
    }

    #[test]
    fn pages_split_long_lists() {
        let apps: Vec<AppEntry> = (0..20).map(|i| AppEntry { id: format!("a{i}"), ..AppEntry::default() }).collect();
        let p = pages(&apps);
        assert_eq!(p.len(), 3);
        assert_eq!(p[2].len(), 4);
        assert_eq!(pages(&[]).len(), 1, "an empty list is still one (empty) page");
    }
}
