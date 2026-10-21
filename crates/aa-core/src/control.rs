//! Control-plane messages: handshake and in-session settings changes.
//!
//! These are rare and small, so they use JSON for readability. Everything
//! on the hot path (video, input) is hand-packed binary in [`crate::wire`]
//! and [`crate::input`].
// In plain words: these are the "spoken" messages between the two computers:
// hello, welcome, goodbye, "who's there?", settings changes, camera on/off.
// Each is written as a short line of JSON text. Adding a new optional field
// needs #[serde(default)] so older versions, which do not send it, still work.

use serde::{Deserialize, Serialize};

use crate::capability::{Capabilities, Negotiated};
use crate::share::ShareTarget;
use crate::PROTOCOL_VERSION;

// Every message the computers say to each other, outside the picture, sound
// and input streams. "type" in the JSON names which one it is.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ControlMessage {
    /// Viewer → host: I want to connect, here is what I can decode.
    Hello { protocol: u16, capabilities: Capabilities },
    /// Host → viewer: accepted, here is what we agreed on.
    Welcome {
        protocol: u16,
        negotiated: Negotiated,
        /// "macos" or "windows": the viewer maps Ctrl/Cmd so shortcuts mean
        /// the same thing on both (Cmd+C on a Mac keyboard copies on a PC,
        /// Ctrl+C on a PC keyboard copies on a Mac). Missing from older
        /// hosts, which were all Windows.
        #[serde(default)]
        host_os: String,
        /// This viewer has "Full access" here: it may pick what is shared
        /// (see [`ControlMessage::SetShare`]) and always has full control.
        #[serde(default)]
        full_access: bool,
        /// The host program's version ("0.5.9"), for the viewer's one-time
        /// "update the other computer" note. Empty from hosts before 0.5.9.
        #[serde(default)]
        version: String,
    },
    /// Host → viewer: refused, with a human-readable reason.
    Reject { reason: String },
    /// Either direction: ending the session cleanly.
    Bye,
    /// Viewer → host: please change the cap (user moved a slider).
    SetMaxBitrate { kbps: u32 },
    /// Viewer → host: mute (or restore) the host's own speakers so someone
    /// sitting next to the host PC doesn't hear everything twice. The host
    /// always restores on disconnect.
    SetHostMute { muted: bool },
    /// Viewer → host: black out the host's own monitor (the viewer still
    /// sees the desktop), so people near the host can't read it. The host
    /// always takes it off on disconnect. Hosts older than 0.5.9 ignore it.
    SetScreenMask { on: bool },
    /// Viewer → every host on the LAN (broadcast): who is out there?
    Discover,
    /// Host → viewer: I am, and this is my name. Sent to whoever asked.
    Here {
        name: String,
        /// The host's public key (hex): tells the app whether it is a
        /// computer we already paired with, before connecting.
        #[serde(default)]
        key: String,
        /// Every address the host has (home network, Tailscale…), so a
        /// computer paired at home can still be reached from elsewhere.
        #[serde(default)]
        addrs: Vec<String>,
        /// The host program's version ("0.4.8"); empty from anything
        /// older than 0.4.8. Lets the app say *which* old version is
        /// answering rather than just "older".
        #[serde(default)]
        version: String,
    },
    /// Host → whole LAN, every second, unasked: "I'm here, connect on
    /// `port`". The viewer only listens, so this works even when the PC's
    /// firewall drops incoming broadcasts (outgoing ones are allowed).
    Beacon {
        name: String,
        port: u16,
        #[serde(default)]
        key: String,
        #[serde(default)]
        version: String,
    },
    /// Host → viewer: why the picture is paused ("the PC is locked…"), or
    /// `None` when it is back. Repeated every few seconds while set.
    HostStatus { message: Option<String> },
    /// Viewer → host: what could you share? (Full-access viewers only get
    /// an answer.)
    ShareList,
    /// Host → viewer: the apps and windows open there, and what is shown now.
    Shareables { items: Vec<ShareTarget>, current: ShareTarget },
    /// Viewer → host: show this instead. Ignored without full access.
    SetShare { target: ShareTarget },
    /// Host → viewer, once a second: what the stream is doing right now,
    /// for the viewer's corner display.
    StreamInfo {
        /// Frames per second the host is aiming for (the connection decides).
        fps: u16,
        /// Bitrate the host is aiming for.
        kbps: u32,
        /// What is being shared.
        sharing: String,
        /// Something on the host blocking input (Valorant's anti-cheat
        /// swallowing clicks), in plain words; shown in the viewer's corner.
        /// Optional, so older viewers and hosts still understand each other.
        #[serde(default)]
        warning: Option<String>,
        /// A game on the host has taken the mouse pointer (hidden and
        /// locked): the viewer switches to game mouse by itself.
        #[serde(default)]
        cursor_captured: bool,
        /// The app in front on the host ("ELDEN RING", "Safari"), so a
        /// phone can pick that game's controls by itself. Empty if unknown.
        #[serde(default)]
        front_app: String,
    },
    /// Host → viewer: a text field here has the keyboard focus, or none
    /// does any more. `field` is where it is, as fractions of the shared
    /// picture (x, y, width, height), so the viewer can zoom to it.
    /// `secure` marks a password field (the phone hides what is typed).
    /// Sent on change and every second while a field is focused, only to
    /// viewers that asked ([`Capabilities::text_focus`]); older viewers
    /// would ignore it anyway.
    TextFocus {
        field: Option<[f32; 4]>,
        #[serde(default)]
        secure: bool,
    },
    /// Host → viewer: an app here is using "Anywhere Camera" / "Anywhere
    /// Microphone" (a call started), or stopped. The viewer turns its own
    /// camera and microphone on only while asked, and only if its settings
    /// allow calls on the other computer to use them.
    CallDevices { camera: bool, mic: bool },
    /// Host → viewer: a camera frame was lost; send a fresh keyframe.
    CameraKeyframe,
    /// Viewer → host: whether its camera is sending, and which one.
    CameraState {
        /// The camera's name ("Integrated Camera"), `None` if there is none
        /// or the settings switch it off for calls.
        name: Option<String>,
        /// Frames are on their way.
        sending: bool,
        /// Why not, in plain words, if it should and isn't.
        #[serde(default)]
        problem: Option<String>,
    },
    /// Viewer → host: which apps and games are there? (The launcher on the
    /// iPhone app.) Answered with `Apps` pages, then the icons as a blob
    /// through the clipboard transfer. Hosts before 0.6 ignore it.
    AppList,
    /// Host → viewer: one page of the launcher list (`page` of `pages`).
    Apps { page: u16, pages: u16, items: Vec<crate::apps::AppEntry> },
    /// Viewer → host: open, switch to, hide or quit one app from the list.
    /// `show`: once the app runs, share only that app (the phone shows one
    /// app at a time, like a console). Needs full access; ignored without.
    AppAction {
        id: String,
        action: crate::apps::AppAction,
        #[serde(default)]
        show: bool,
    },
}

// Helpers to build, write and read a message.
impl ControlMessage {
    // The first thing a viewer says, stamped with this version's protocol number.
    pub fn hello(capabilities: Capabilities) -> Self {
        Self::Hello { protocol: PROTOCOL_VERSION, capabilities }
    }

    // Message -> bytes to send.
    pub fn encode(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("control messages are always serialisable")
    }

    // Bytes received -> message; anything that is not a valid message is an error
    // (and ignored by the caller).
    pub fn decode(bytes: &[u8]) -> Result<Self, serde_json::Error> {
        serde_json::from_slice(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::video::{Codec, ColorRange, Resolution};

    fn caps() -> Capabilities {
        Capabilities {
            codecs: vec![Codec::Hevc],
            max_resolution: Resolution::new(1920, 1080),
            max_fps: 60,
            color_ranges: vec![ColorRange::Sdr],
            has_gamepad: true,
            can_emulate_gamepad: false,
            fixed_fps: false,
            loss_recovery: false,
            text_focus: false,
            phone_display: None,
        }
    }

    #[test]
    fn hello_round_trips() {
        let m = ControlMessage::hello(caps());
        assert_eq!(ControlMessage::decode(&m.encode()).unwrap(), m);
    }

    #[test]
    fn text_focus_round_trips_and_old_hellos_still_read() {
        for m in [
            ControlMessage::TextFocus { field: Some([0.1, 0.2, 0.3, 0.04]), secure: true },
            ControlMessage::TextFocus { field: None, secure: false },
        ] {
            assert_eq!(ControlMessage::decode(&m.encode()).unwrap(), m);
        }
        // A host older than 0.6.1 sends no `secure`: not a password field.
        let m = ControlMessage::decode(br#"{"type":"text_focus","field":null}"#).unwrap();
        assert_eq!(m, ControlMessage::TextFocus { field: None, secure: false });
        // A viewer older than 0.6.1 sends no `text_focus`: it didn't ask.
        let mut json = serde_json::to_value(ControlMessage::hello(caps())).unwrap();
        let obj = json["capabilities"].as_object_mut().unwrap();
        obj.remove("text_focus");
        obj.remove("phone_display");
        let ControlMessage::Hello { capabilities, .. } = ControlMessage::decode(json.to_string().as_bytes()).unwrap()
        else {
            panic!("not a hello")
        };
        assert!(!capabilities.text_focus);
        assert_eq!(capabilities.phone_display, None, "no phone mode from a viewer that doesn't ask");
        // A phone asking for Phone mode is understood.
        let mut c = caps();
        c.phone_display = Some([1080, 500]);
        let m = ControlMessage::hello(c);
        assert_eq!(ControlMessage::decode(&m.encode()).unwrap(), m);
    }

    #[test]
    fn garbage_is_rejected() {
        assert!(ControlMessage::decode(b"{not json").is_err());
    }
}
