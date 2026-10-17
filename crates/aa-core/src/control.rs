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

    #[test]
    fn hello_round_trips() {
        let caps = Capabilities {
            codecs: vec![Codec::Hevc],
            max_resolution: Resolution::new(1920, 1080),
            max_fps: 60,
            color_ranges: vec![ColorRange::Sdr],
            has_gamepad: true,
            can_emulate_gamepad: false,
            fixed_fps: false,
            loss_recovery: false,
        };
        let m = ControlMessage::hello(caps);
        assert_eq!(ControlMessage::decode(&m.encode()).unwrap(), m);
    }

    #[test]
    fn garbage_is_rejected() {
        assert!(ControlMessage::decode(b"{not json").is_err());
    }
}
