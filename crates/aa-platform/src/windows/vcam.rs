//! The Anywhere app's side of "Anywhere Camera" for every Windows app.
//!
//! [`create_camera`] asks Windows 11 to add the camera (it then appears in
//! Teams, Zoom, the Camera app and browsers). The pictures come from the
//! `aa-vcam` DLL that Windows' camera service loads; while an app uses the
//! camera, that DLL creates a block of shared memory, and [`Feeder`] finds
//! it and fills it with pictures.

#![allow(unsafe_code)]

use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, Ordering};

use aa_core::calls::vcam;
use windows::core::{Interface, GUID, HRESULT, PCSTR, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Media::MediaFoundation::{
    IMFAsyncCallback, IMFVirtualCamera, MFVirtualCameraAccess_AllUsers, MFVirtualCameraAccess_CurrentUser,
    MFVirtualCameraLifetime_Session, MFVirtualCameraType_SoftwareCameraSource,
};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::System::Memory::{
    MapViewOfFile, OpenFileMappingW, UnmapViewOfFile, FILE_MAP_READ, FILE_MAP_WRITE, MEMORY_MAPPED_VIEW_ADDRESS,
};

use crate::{PlatformError, Result};

type CreateFn = unsafe extern "system" fn(
    kind: i32,
    lifetime: i32,
    access: i32,
    name: PCWSTR,
    source: PCWSTR,
    categories: *const GUID,
    count: u32,
    out: *mut *mut c_void,
) -> HRESULT;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

/// Add "Anywhere Camera" to Windows for as long as this program runs.
/// Windows 11 only; on Windows 10 the browser add-on is the way.
pub fn create_camera() -> Result<()> {
    // SAFETY: loading a system DLL and calling its documented function by
    // pointer (looked up at run time so the program still starts on
    // Windows 10, where the function doesn't exist).
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let lib = LoadLibraryW(PCWSTR(wide("mfsensorgroup.dll").as_ptr()))
            .map_err(|_| PlatformError::Unavailable("Anywhere Camera for apps needs Windows 11".into()))?;
        let f = GetProcAddress(lib, PCSTR(c"MFCreateVirtualCamera".as_ptr().cast()))
            .ok_or_else(|| PlatformError::Unavailable("Anywhere Camera for apps needs Windows 11".into()))?;
        let create: CreateFn = std::mem::transmute(f);
        let name = wide(vcam::NAME);
        let clsid = wide(vcam::CLSID_STR);
        // For everyone on this PC when we may (the always-on service), else
        // for whoever is signed in.
        let mut last = HRESULT(0);
        for access in [MFVirtualCameraAccess_AllUsers, MFVirtualCameraAccess_CurrentUser] {
            let mut out: *mut c_void = std::ptr::null_mut();
            let hr = create(
                MFVirtualCameraType_SoftwareCameraSource.0,
                MFVirtualCameraLifetime_Session.0,
                access.0,
                PCWSTR(name.as_ptr()),
                PCWSTR(clsid.as_ptr()),
                std::ptr::null(),
                0,
                &mut out,
            );
            if hr.is_ok() && !out.is_null() {
                let cam = IMFVirtualCamera::from_raw(out);
                cam.Start(None::<&IMFAsyncCallback>).map_err(|e| {
                    PlatformError::Unavailable(format!(
                        "Windows refused to start Anywhere Camera ({e}); reinstalling Anywhere registers it again"
                    ))
                })?;
                tracing::info!(
                    all_users = access == MFVirtualCameraAccess_AllUsers,
                    "Anywhere Camera added to Windows"
                );
                // Keep it for the life of this program (Session lifetime:
                // Windows removes it when we exit).
                std::mem::forget(cam);
                return Ok(());
            }
            last = hr;
        }
        Err(PlatformError::Unavailable(format!("Windows didn't add Anywhere Camera ({last:?})")))
    }
}

/// Fills the shared memory while an app uses the camera.
#[derive(Debug, Default)]
pub struct Feeder {
    map: Option<(HANDLE, MEMORY_MAPPED_VIEW_ADDRESS)>,
}

// SAFETY: a kernel handle and a mapping of plain memory.
unsafe impl Send for Feeder {}

impl Feeder {
    fn field(&self, off: usize) -> Option<&AtomicU32> {
        let (_, view) = self.map.as_ref()?;
        // SAFETY: 4-byte aligned offset inside the mapping.
        Some(unsafe { &*(view.Value.cast::<u8>().add(off).cast::<AtomicU32>()) })
    }

    fn close(&mut self) {
        if let Some((h, view)) = self.map.take() {
            // SAFETY: unmapping and closing what we opened.
            unsafe {
                let _ = UnmapViewOfFile(view);
                let _ = CloseHandle(h);
            }
        }
    }

    /// Is an app using Anywhere Camera right now?
    pub fn wanted(&mut self) -> bool {
        if self.map.is_none() {
            let name = wide(vcam::SECTION);
            // SAFETY: opening a named mapping; failure just means "not in use".
            unsafe {
                let Ok(h) = OpenFileMappingW((FILE_MAP_READ | FILE_MAP_WRITE).0, false, PCWSTR(name.as_ptr())) else {
                    return false;
                };
                let view = MapViewOfFile(h, FILE_MAP_READ | FILE_MAP_WRITE, 0, 0, vcam::SIZE);
                if view.Value.is_null() {
                    let _ = CloseHandle(h);
                    return false;
                }
                self.map = Some((h, view));
            }
        }
        let ok = self.field(vcam::OFF_MAGIC).is_some_and(|m| m.load(Ordering::Acquire) == vcam::MAGIC)
            && self.field(vcam::OFF_WANTED).is_some_and(|w| w.load(Ordering::Acquire) == 1);
        if !ok {
            // The app stopped: let go, so the memory disappears and a new
            // use starts clean.
            self.close();
        }
        ok
    }

    /// Hand one 1280x720 NV12 picture to the camera.
    pub fn write(&mut self, nv12: &[u8]) {
        let Some(seq) = self.field(vcam::OFF_SEQ) else { return };
        let Some((_, view)) = self.map.as_ref() else { return };
        let n = seq.load(Ordering::Relaxed);
        // Odd while writing, so the camera never shows half a picture.
        seq.store(n.wrapping_add(1) | 1, Ordering::Release);
        // SAFETY: the data area lies inside the mapping.
        unsafe {
            let dst = view.Value.cast::<u8>().add(vcam::OFF_DATA);
            std::ptr::copy_nonoverlapping(nv12.as_ptr(), dst, nv12.len().min(vcam::FRAME_LEN));
        }
        let next = (n.wrapping_add(1) | 1).wrapping_add(1);
        seq.store(if next == 0 { 2 } else { next }, Ordering::Release);
    }
}

impl Drop for Feeder {
    fn drop(&mut self) {
        self.close();
    }
}
