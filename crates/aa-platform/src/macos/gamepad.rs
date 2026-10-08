//! Virtual controllers on a Mac host: each controller on the viewer
//! becomes a virtual DualSense on this Mac.
//!
//! macOS lets a program create a pretend USB HID device in user space
//! (`IOHIDUserDevice`). Games, Steam and Apple's GameController framework
//! then see a DualSense, which every Mac game supports. The catch: macOS
//! only allows it for programs running as administrator (or carrying an
//! entitlement only Apple can grant). So controllers on a Mac host need
//! `aa-host` started with `sudo`; without it everything else still works
//! and the log says how to turn controllers on.
//!
//! Whatever the controller on the viewer is (DualSense, Xbox, Switch…),
//! its state is written as DualSense input reports. Rumble a game sends to
//! the virtual pad is picked out of its output reports and passed back.

#![allow(unsafe_code, clippy::pedantic)]

use std::collections::VecDeque;
use std::ffi::c_void;
use std::sync::{Arc, Mutex};

use aa_core::ds5;
use aa_core::input::{GamepadKind, GamepadState, Rumble};
use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchRetained};
use objc2_core_foundation::{CFData, CFDictionary, CFNumber, CFRetained, CFString, CFType};

use crate::{PlatformError, Result, VirtualGamepad};

type IOReturn = i32;
const IO_SUCCESS: IOReturn = 0;
const IO_UNSUPPORTED: IOReturn = 0xE000_02C7_u32 as i32; // kIOReturnUnsupported
/// IOHIDReportType values.
const REPORT_OUTPUT: u32 = 1;
const REPORT_FEATURE: u32 = 2;

#[link(name = "IOKit", kind = "framework")]
extern "C" {
    fn IOHIDUserDeviceCreateWithProperties(
        allocator: *const c_void,
        properties: *const c_void,
        options: u32,
    ) -> *mut c_void;
    fn IOHIDUserDeviceRegisterGetReportBlock(device: *mut c_void, block: *mut c_void);
    fn IOHIDUserDeviceRegisterSetReportBlock(device: *mut c_void, block: *mut c_void);
    fn IOHIDUserDeviceSetDispatchQueue(device: *mut c_void, queue: *mut c_void);
    fn IOHIDUserDeviceActivate(device: *mut c_void);
    fn IOHIDUserDeviceCancel(device: *mut c_void);
    fn IOHIDUserDeviceHandleReportWithTimeStamp(
        device: *mut c_void,
        timestamp: u64,
        report: *const u8,
        len: isize,
    ) -> IOReturn;
}

extern "C" {
    fn mach_absolute_time() -> u64;
    fn CFRelease(cf: *const c_void);
}

type GetBlock = RcBlock<dyn Fn(u32, u32, *mut u8, *mut isize) -> IOReturn>;
type SetBlock = RcBlock<dyn Fn(u32, u32, *const u8, isize) -> IOReturn>;

/// One virtual DualSense.
struct Pad {
    device: *mut c_void,
    _get: GetBlock,
    _set: SetBlock,
    seq: u8,
    last: GamepadState,
}

impl Drop for Pad {
    fn drop(&mut self) {
        // SAFETY: cancelling then releasing the device we created; the
        // blocks stay alive until after this (fields drop afterwards).
        unsafe {
            IOHIDUserDeviceCancel(self.device);
            CFRelease(self.device);
        }
    }
}

pub struct MacPads {
    pads: Vec<Option<Pad>>,
    queue: DispatchRetained<DispatchQueue>,
    rumble: Arc<Mutex<VecDeque<Rumble>>>,
    /// We already found out macOS won't let us (not running as admin).
    refused: bool,
}

// SAFETY: the device handles are only used from the input thread that
// owns this struct; IOKit calls back on our own dispatch queue.
unsafe impl Send for MacPads {}

impl std::fmt::Debug for MacPads {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MacPads").finish_non_exhaustive()
    }
}

fn properties(slot: u8) -> CFRetained<CFDictionary<CFString, CFType>> {
    let desc = CFData::from_bytes(&crate::ds5dev::report_descriptor());
    let num = |v: i32| CFNumber::new_i32(v);
    let (vid, pid, ver) = (num(i32::from(ds5::VID_SONY)), num(i32::from(ds5::PID_DUALSENSE)), num(0x100));
    let (page, usage, country) = (num(1), num(5), num(0));
    let transport = CFString::from_str("USB");
    let product = CFString::from_str("DualSense Wireless Controller");
    let maker = CFString::from_str("Sony Interactive Entertainment");
    let serial = CFString::from_str(&format!("AA-VIRTUAL-{slot}"));
    let names = [
        "ReportDescriptor",
        "VendorID",
        "ProductID",
        "VersionNumber",
        "Transport",
        "Product",
        "Manufacturer",
        "SerialNumber",
        "PrimaryUsagePage",
        "PrimaryUsage",
        "CountryCode",
    ];
    let values: [&CFType; 11] =
        [&desc, &vid, &pid, &ver, &transport, &product, &maker, &serial, &page, &usage, &country];
    let keys: Vec<CFRetained<CFString>> = names.iter().map(|k| CFString::from_str(k)).collect();
    let key_refs: Vec<&CFString> = keys.iter().map(|k| &**k).collect();
    CFDictionary::from_slices(&key_refs, &values)
}

impl MacPads {
    pub fn new() -> Self {
        Self {
            pads: (0..8).map(|_| None).collect(),
            queue: DispatchQueue::new("aa.virtual-pads", None),
            rumble: Arc::default(),
            refused: false,
        }
    }

    fn create(&mut self, slot: u8) -> Result<Pad> {
        let props = properties(slot);
        // SAFETY: valid dictionary; a null result means macOS refused.
        let device = unsafe {
            IOHIDUserDeviceCreateWithProperties(
                std::ptr::null(),
                (props.as_opaque() as *const CFDictionary).cast::<c_void>(),
                0,
            )
        };
        if device.is_null() {
            return Err(PlatformError::Permission(
                "macOS only lets administrators create virtual controllers. Start aa-host with sudo to use \
                 controllers on this Mac: cargo build --release, then sudo ./target/release/aa-host"
                    .into(),
            ));
        }
        // Feature reports (calibration, serial, firmware) the system and
        // games ask for when the pad appears.
        let get: GetBlock = RcBlock::new(move |kind: u32, id: u32, buf: *mut u8, len: *mut isize| -> IOReturn {
            if kind != REPORT_FEATURE || buf.is_null() || len.is_null() {
                return IO_UNSUPPORTED;
            }
            let Some(report) = u8::try_from(id).ok().and_then(|id| crate::ds5dev::default_feature(id, slot)) else {
                return IO_UNSUPPORTED;
            };
            // SAFETY: IOKit gives a buffer of *len bytes; we write at most that.
            unsafe {
                let n = report.len().min(usize::try_from(*len).unwrap_or(0));
                std::ptr::copy_nonoverlapping(report.as_ptr(), buf, n);
                *len = n as isize;
            }
            IO_SUCCESS
        });
        // Output reports from games: take the rumble motors.
        let rumble = Arc::clone(&self.rumble);
        let set: SetBlock = RcBlock::new(move |kind: u32, _id: u32, buf: *const u8, len: isize| -> IOReturn {
            if kind == REPORT_OUTPUT && !buf.is_null() && len >= 5 {
                // SAFETY: IOKit gives `len` readable bytes.
                let r = unsafe { std::slice::from_raw_parts(buf, len as usize) };
                // With the report id first: [0x02, flags0, flags1, right motor, left motor, …].
                let off = usize::from(r[0] != 0x02);
                if r.len() >= 5 - off && r.get(1 - off).is_some_and(|f| f & 0x03 != 0) {
                    let (high, low) = (r[3 - off], r[4 - off]);
                    if let Ok(mut q) = rumble.lock() {
                        q.push_back(Rumble { slot, low_freq: low, high_freq: high });
                        while q.len() > 32 {
                            q.pop_front();
                        }
                    }
                }
            }
            IO_SUCCESS
        });
        // SAFETY: registering blocks and a queue on a fresh, inactive device;
        // the blocks live in the returned Pad until the device is cancelled.
        unsafe {
            IOHIDUserDeviceRegisterGetReportBlock(device, &*get as *const _ as *mut c_void);
            IOHIDUserDeviceRegisterSetReportBlock(device, &*set as *const _ as *mut c_void);
            IOHIDUserDeviceSetDispatchQueue(device, &*self.queue as *const DispatchQueue as *mut c_void);
            IOHIDUserDeviceActivate(device);
        }
        tracing::info!(slot, "virtual DualSense plugged into this Mac");
        Ok(Pad { device, _get: get, _set: set, seq: 0, last: GamepadState::default() })
    }

    fn send(pad: &mut Pad, state: GamepadState) {
        pad.seq = pad.seq.wrapping_add(1);
        pad.last = state;
        let report = ds5::state_to_usb_input(&state, pad.seq);
        // SAFETY: a live device and a 64-byte report.
        let r = unsafe {
            IOHIDUserDeviceHandleReportWithTimeStamp(
                pad.device,
                mach_absolute_time(),
                report.as_ptr(),
                report.len() as isize,
            )
        };
        if r != IO_SUCCESS {
            tracing::debug!("virtual pad report rejected: {r:#x}");
        }
    }
}

impl Default for MacPads {
    fn default() -> Self {
        Self::new()
    }
}

impl VirtualGamepad for MacPads {
    fn attach(&mut self, slot: u8, _kind: GamepadKind) -> Result<()> {
        let ix = usize::from(slot);
        if ix >= self.pads.len() || self.pads[ix].is_some() || self.refused {
            return Ok(());
        }
        match self.create(slot) {
            Ok(p) => self.pads[ix] = Some(p),
            Err(e) => {
                self.refused = true;
                tracing::warn!("{e}");
            }
        }
        Ok(())
    }

    fn detach(&mut self, slot: u8) -> Result<()> {
        if let Some(p) = self.pads.get_mut(usize::from(slot)).and_then(Option::take) {
            drop(p);
            tracing::info!(slot, "virtual DualSense unplugged");
        }
        Ok(())
    }

    fn update(&mut self, slot: u8, state: GamepadState) -> Result<()> {
        let ix = usize::from(slot);
        if ix < self.pads.len() && self.pads[ix].is_none() {
            // A state before the attach notice (lost packet): plug in now.
            self.attach(slot, GamepadKind::PlayStation)?;
        }
        if let Some(p) = self.pads.get_mut(ix).and_then(Option::as_mut) {
            Self::send(p, state);
        }
        Ok(())
    }

    fn poll_rumble(&mut self) -> Result<Option<Rumble>> {
        Ok(self.rumble.lock().ok().and_then(|mut q| q.pop_front()))
    }
}
