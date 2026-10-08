//! Control-plane messages: handshake and in-session settings changes.
//!
//! These are rare and small, so they use JSON for readability. Everything
//! on the hot path (video, input) is hand-packed binary in [`crate::wire`]
//! and [`crate::input`].

use serde::{Deserialize, Serialize};

use crate::capability::{Capabilities, Negotiated};
use crate::PROTOCOL_VERSION;

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
    },
    /// Host → whole LAN, every second, unasked: "I'm here, connect on
    /// `port`". The viewer only listens, so this works even when the PC's
    /// firewall drops incoming broadcasts (outgoing ones are allowed).
    Beacon {
        name: String,
        port: u16,
        #[serde(default)]
        key: String,
    },
    /// Host → viewer: why the picture is paused ("the PC is locked…"), or
    /// `None` when it is back. Repeated every few seconds while set.
    HostStatus { message: Option<String> },
}

impl ControlMessage {
    pub fn hello(capabilities: Capabilities) -> Self {
        Self::Hello { protocol: PROTOCOL_VERSION, capabilities }
    }

    pub fn encode(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("control messages are always serialisable")
    }

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
        };
        let m = ControlMessage::hello(caps);
        assert_eq!(ControlMessage::decode(&m.encode()).unwrap(), m);
    }

    #[test]
    fn garbage_is_rejected() {
        assert!(ControlMessage::decode(b"{not json").is_err());
    }
}
