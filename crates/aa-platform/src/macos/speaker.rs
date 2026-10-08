//! "Mute the other computer's speakers" on a Mac host.
//!
//! Mutes the Mac's current output device through Core Audio, remembering
//! how it was, and puts it back when the viewer unmutes or leaves. The
//! stream itself is unaffected: ScreenCaptureKit takes the apps' sound
//! before it reaches the speakers.
//!
//! Some outputs (HDMI and DisplayPort monitors, some USB devices) have no
//! mute switch; for those the volume is turned to zero instead and restored
//! afterwards.

#![allow(unsafe_code, clippy::pedantic)]

use std::ffi::c_void;

use crate::{PlatformError, Result};

#[repr(C)]
struct Address {
    selector: u32,
    scope: u32,
    element: u32,
}

const fn fourcc(s: &[u8; 4]) -> u32 {
    ((s[0] as u32) << 24) | ((s[1] as u32) << 16) | ((s[2] as u32) << 8) | (s[3] as u32)
}

const SYSTEM_OBJECT: u32 = 1;
const DEFAULT_OUTPUT: u32 = fourcc(b"dOut");
const MUTE: u32 = fourcc(b"mute");
const VOLUME: u32 = fourcc(b"volm");
const SCOPE_GLOBAL: u32 = fourcc(b"glob");
const SCOPE_OUTPUT: u32 = fourcc(b"outp");

#[link(name = "CoreAudio", kind = "framework")]
extern "C" {
    fn AudioObjectHasProperty(id: u32, addr: *const Address) -> u8;
    fn AudioObjectIsPropertySettable(id: u32, addr: *const Address, settable: *mut u8) -> i32;
    fn AudioObjectGetPropertyData(
        id: u32,
        addr: *const Address,
        qual_size: u32,
        qual: *const c_void,
        size: *mut u32,
        data: *mut c_void,
    ) -> i32;
    fn AudioObjectSetPropertyData(
        id: u32,
        addr: *const Address,
        qual_size: u32,
        qual: *const c_void,
        size: u32,
        data: *const c_void,
    ) -> i32;
}

fn get<T: Copy + Default>(id: u32, addr: &Address) -> Option<T> {
    let mut v = T::default();
    let mut size = std::mem::size_of::<T>() as u32;
    // SAFETY: `v` is a valid buffer of `size` bytes.
    let r = unsafe { AudioObjectGetPropertyData(id, addr, 0, std::ptr::null(), &mut size, (&mut v as *mut T).cast()) };
    (r == 0).then_some(v)
}

fn set<T: Copy>(id: u32, addr: &Address, v: T) -> bool {
    // SAFETY: `v` lives for the call; size matches its type.
    unsafe {
        AudioObjectSetPropertyData(
            id,
            addr,
            0,
            std::ptr::null(),
            std::mem::size_of::<T>() as u32,
            (&v as *const T).cast(),
        ) == 0
    }
}

fn settable(id: u32, addr: &Address) -> bool {
    let mut s = 0u8;
    // SAFETY: valid pointers.
    unsafe { AudioObjectHasProperty(id, addr) != 0 && AudioObjectIsPropertySettable(id, addr, &mut s) == 0 && s != 0 }
}

/// How the device was before we touched it.
#[derive(Debug, Clone)]
enum Saved {
    Mute {
        device: u32,
        was: u32,
    },
    /// Per element (0 = main, 1/2 = left/right): its volume before.
    Volume {
        device: u32,
        levels: Vec<(u32, f32)>,
    },
}

#[derive(Debug, Default)]
pub struct MacSpeaker {
    saved: Option<Saved>,
}

impl MacSpeaker {
    pub fn new() -> Self {
        Self::default()
    }

    fn default_output() -> Result<u32> {
        let addr = Address { selector: DEFAULT_OUTPUT, scope: SCOPE_GLOBAL, element: 0 };
        get::<u32>(SYSTEM_OBJECT, &addr)
            .filter(|&d| d != 0)
            .ok_or_else(|| PlatformError::Unavailable("no sound output device".into()))
    }

    fn mute_now(&mut self) -> Result<()> {
        let device = Self::default_output()?;
        let mute = Address { selector: MUTE, scope: SCOPE_OUTPUT, element: 0 };
        if settable(device, &mute) {
            let was = get::<u32>(device, &mute).unwrap_or(0);
            if set(device, &mute, 1u32) {
                self.saved.get_or_insert(Saved::Mute { device, was });
                return Ok(());
            }
        }
        // No mute switch: volume to zero, element by element.
        let mut levels = Vec::new();
        for element in [0u32, 1, 2] {
            let a = Address { selector: VOLUME, scope: SCOPE_OUTPUT, element };
            if settable(device, &a) {
                if let Some(v) = get::<f32>(device, &a) {
                    if set(device, &a, 0.0f32) {
                        levels.push((element, v));
                    }
                }
            }
        }
        if levels.is_empty() {
            return Err(PlatformError::Unavailable("this sound output can't be muted from software".into()));
        }
        self.saved.get_or_insert(Saved::Volume { device, levels });
        Ok(())
    }
}

impl crate::audio::SpeakerControl for MacSpeaker {
    fn set_muted(&mut self, muted: bool) -> Result<()> {
        if muted {
            self.mute_now()?;
            tracing::info!("Mac speakers muted while connected");
        } else {
            self.restore()?;
        }
        Ok(())
    }

    fn restore(&mut self) -> Result<()> {
        match self.saved.take() {
            Some(Saved::Mute { device, was }) => {
                set(device, &Address { selector: MUTE, scope: SCOPE_OUTPUT, element: 0 }, was);
                tracing::info!("Mac speakers restored");
            }
            Some(Saved::Volume { device, levels }) => {
                for (element, v) in levels {
                    set(device, &Address { selector: VOLUME, scope: SCOPE_OUTPUT, element }, v);
                }
                tracing::info!("Mac speaker volume restored");
            }
            None => {}
        }
        Ok(())
    }
}

impl Drop for MacSpeaker {
    fn drop(&mut self) {
        let _ = crate::audio::SpeakerControl::restore(self);
    }
}
