//! "Anywhere Camera" and "Anywhere Microphone": the camera and microphone
//! of the computer you sit at, offered to calls (Google Meet, Zoom, Teams)
//! on the computer you are watching.
//!
//! On the watched computer the pictures reach the call one of two ways:
//! - a browser add-on (Chrome, Edge, Safari) that adds both devices to
//!   web pages, fed over a WebSocket on this computer only (127.0.0.1);
//! - on Windows 11 also a real virtual camera every app sees.
//!
//! This file holds what both ends of the browser link agree on: the port,
//! which add-ons may connect, and the message layout. No OS code.

// In plain words: a call app on the watched computer (Meet in a browser, or
// any Windows 11 app) sees "Anywhere Camera". Its pictures come from the
// camera of the computer you sit at. This file holds the shared numbers and
// names both ends must agree on.
use serde::{Deserialize, Serialize};

/// The WebSocket port on 127.0.0.1 the add-on connects to.
pub const BRIDGE_PORT: u16 = 7721;

/// The add-on's ID when loaded by hand (developer mode). Fixed by the
/// public key in its manifest, so it is the same on every computer.
pub const DEV_EXTENSION_ID: &str = "eboaohfmengdlgbmnjailhbjjkdcfelp";

/// The IDs the Chrome Web Store and Edge Add-ons give the add-on once it
/// is published there (each store picks its own). Filled in after the owner
/// publishes; until then the developer-mode ID is the only Chrome/Edge one.
pub const CHROME_STORE_ID: Option<&str> = None;
pub const EDGE_STORE_ID: Option<&str> = None;

/// May a WebSocket from this `Origin` use the camera?
///
/// Web pages can't fake the Origin header, so only our add-on gets in: a
/// web page (or any other site's script) connecting to 127.0.0.1 is turned
/// away. Safari gives each installed add-on a random origin, so any Safari
/// add-on origin is accepted; Safari only runs add-ons the user enabled.
pub fn origin_allowed(origin: &str) -> bool {
    // Ignore a trailing slash some browsers add.
    let origin = origin.trim_end_matches('/');
    // Chrome and Edge: only our known add-on ids.
    if let Some(id) = origin.strip_prefix("chrome-extension://") {
        return id == DEV_EXTENSION_ID || Some(id) == CHROME_STORE_ID || Some(id) == EDGE_STORE_ID;
    }
    // Safari: any add-on id made of letters, digits and dashes.
    if let Some(rest) = origin.strip_prefix("safari-web-extension://") {
        return !rest.is_empty() && rest.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
    }
    false
}

/// First byte of a binary message from the host to the add-on.
pub const MSG_VIDEO: u8 = 1;
pub const MSG_AUDIO: u8 = 2;
/// Bytes before the payload: kind, flags, 8-byte timestamp (µs, big-endian).
pub const MSG_HEADER: usize = 10;
/// Flag bit: this video message is a keyframe.
pub const FLAG_KEY: u8 = 1;

/// A binary message: H.264 (Annex B) for video, or 10 ms of 48 kHz stereo
/// 16-bit little-endian samples for audio.
pub fn encode_media(kind: u8, keyframe: bool, ts_us: u64, payload: &[u8]) -> Vec<u8> {
    // Layout: [kind][flags][8-byte timestamp][payload].
    let mut out = Vec::with_capacity(MSG_HEADER + payload.len());
    out.push(kind);
    out.push(if keyframe { FLAG_KEY } else { 0 });
    out.extend_from_slice(&ts_us.to_be_bytes());
    out.extend_from_slice(payload);
    out
}

/// Text messages, both directions, as JSON.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BridgeMsg {
    /// Add-on → host: what this browser tab needs right now.
    Want { video: bool, audio: bool },
    /// Add-on → host: a picture was lost, send a fresh one.
    Keyframe,
    /// Host → add-on: who is connected and what they send.
    Status {
        /// The computer watching this one, if any ("MAITRIK").
        viewer: Option<String>,
        /// Its camera, while it sends.
        camera: Option<String>,
        /// Why there is no picture, in plain words.
        problem: Option<String>,
    },
}

/// The Windows virtual camera: a small DLL that Windows' camera service
/// loads (`aa-vcam`), fed by the Anywhere app through shared memory.
// Shared-memory layout between the Anywhere app and the camera DLL. Both
// sides must use exactly these offsets.
pub mod vcam {
    /// The DLL's class id (registered by the installer).
    pub const CLSID: u128 = 0x4aba_54b9_4410_46e2_99d7_5445_cd96_a471;
    pub const CLSID_STR: &str = "{4ABA54B9-4410-46E2-99D7-5445CD96A471}";
    /// What apps show in their camera list.
    pub const NAME: &str = "Anywhere Camera";
    /// Shared memory between the camera service (which creates it while an
    /// app uses the camera) and the Anywhere app (which fills it).
    pub const SECTION: &str = "Global\\AnywhereCamera";
    pub const MAGIC: u32 = 0x4d43_4141; // "AACM"
    pub const WIDTH: usize = 1280;
    pub const HEIGHT: usize = 720;
    // Byte offsets of little-endian u32 fields.
    pub const OFF_MAGIC: usize = 0;
    /// 1 while an app is using the camera (written by the DLL).
    pub const OFF_WANTED: usize = 4;
    /// Picture counter: odd while a picture is being written (by the app).
    pub const OFF_SEQ: usize = 8;
    pub const OFF_DATA: usize = 64;
    /// NV12, 1280x720.
    pub const FRAME_LEN: usize = WIDTH * HEIGHT * 3 / 2;
    pub const SIZE: usize = OFF_DATA + FRAME_LEN;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_our_add_on_may_connect() {
        assert!(origin_allowed(&format!("chrome-extension://{DEV_EXTENSION_ID}")));
        assert!(origin_allowed("safari-web-extension://3F1B2C4D-0000-4AAA-8BBB-1234567890AB"));
        assert!(!origin_allowed("https://meet.google.com"));
        assert!(!origin_allowed("chrome-extension://aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"));
        assert!(!origin_allowed("null"));
        assert!(!origin_allowed(""));
        assert!(!origin_allowed("safari-web-extension://"));
    }

    #[test]
    fn messages_round_trip() {
        let m = encode_media(MSG_VIDEO, true, 42, b"abc");
        assert_eq!(m[0], MSG_VIDEO);
        assert_eq!(m[1], FLAG_KEY);
        assert_eq!(u64::from_be_bytes(m[2..10].try_into().unwrap()), 42);
        assert_eq!(&m[10..], b"abc");
        let w: BridgeMsg = serde_json::from_str(r#"{"type":"want","video":true,"audio":false}"#).unwrap();
        assert_eq!(w, BridgeMsg::Want { video: true, audio: false });
        let s = serde_json::to_string(&BridgeMsg::Status { viewer: None, camera: None, problem: None }).unwrap();
        assert!(s.contains(r#""type":"status""#));
    }
}
