//! `aa-viewer` as a library: everything the watching side does apart from
//! drawing a desktop window. The desktop program (`main.rs`) adds the
//! window; the iPhone app (`crates/aa-mobile`) adds a Swift screen instead
//! and builds this crate without the `window` feature.
//!
//! In plain words: connecting, staying connected, receiving and decoding
//! the picture and sound, and sending input all live here, so the Mac/PC
//! viewer and the phone app share one copy of that code.

// This used to be a program, not a library; its types were never meant to
// be shown in other crates' docs, so the "every public type prints with
// {:?}" rule is switched off here rather than adding Debug to each one.
#![allow(missing_debug_implementations)]

pub mod audio;
pub mod camera;
pub mod ds5;
pub mod link;
pub mod net;
pub mod pacing;
pub mod pads;
pub mod reconnect;
pub mod session;
pub mod udp;
pub mod update_note;

#[cfg(feature = "window")]
pub mod keymap;
#[cfg(feature = "window")]
pub mod overlay;
#[cfg(feature = "window")]
pub mod window;
