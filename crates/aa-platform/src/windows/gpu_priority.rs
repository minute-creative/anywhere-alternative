//! Keep the graphics chip working for us while a game uses all of it.
//!
//! In plain words: a game asks the GPU for every frame it can draw. Our
//! screen capture and video encoder use the same GPU, and by default
//! Windows treats both alike, so with the game at 100% our work waits in
//! line. The owner's Valorant test (0.5.9, Intel Arc 130T) gave the Mac a
//! steady 24 fps of a 120 fps stream. Game streaming hosts such as Sunshine
//! solve this by asking Windows for a higher GPU priority for their own
//! process and devices, which is what this does:
//! 1. the process: `D3DKMTSetProcessSchedulingPriorityClass` to REALTIME
//!    (needs the "increase base priority" privilege, which the sharing
//!    service, running as SYSTEM, can switch on), falling back to HIGH;
//! 2. each capture/encode device: `IDXGIDevice::SetGPUThreadPriority(7)`.
//!
//! The cost: the game may lose a few frames a second on the PC itself so
//! the stream keeps up, which is the right trade while someone watches.

#![allow(unsafe_code, clippy::pedantic)]

use std::sync::Once;

use windows::core::{s, w, Interface, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE, LUID};
use windows::Win32::Graphics::Direct3D11::ID3D11Device;
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::Security::{
    AdjustTokenPrivileges, LookupPrivilegeValueW, LUID_AND_ATTRIBUTES, SE_INC_BASE_PRIORITY_NAME, SE_PRIVILEGE_ENABLED,
    TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES, TOKEN_QUERY,
};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

/// `D3DKMT_SCHEDULINGPRIORITYCLASS` values.
const REALTIME: i32 = 5;
const HIGH: i32 = 4;

type SetClass = unsafe extern "system" fn(HANDLE, i32) -> i32;

/// Raise the GPU priority of this process (once) and of `device`.
pub fn raise(device: &ID3D11Device) {
    static PROCESS: Once = Once::new();
    PROCESS.call_once(raise_process);
    // SAFETY: COM call on a live device.
    let r = unsafe { device.cast::<IDXGIDevice>().and_then(|d| d.SetGPUThreadPriority(7)) };
    if let Err(e) = r {
        tracing::debug!("GPU thread priority unchanged: {e}");
    }
}

fn raise_process() {
    // SAFETY: plain Win32 calls; the token handle is closed below; the
    // function pointer comes from gdi32 by its documented name and
    // signature (NTSTATUS fn(HANDLE, D3DKMT_SCHEDULINGPRIORITYCLASS)).
    unsafe {
        let mut token = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY, &mut token).is_ok() {
            let mut luid = LUID::default();
            if LookupPrivilegeValueW(PCWSTR::null(), SE_INC_BASE_PRIORITY_NAME, &mut luid).is_ok() {
                let tp = TOKEN_PRIVILEGES {
                    PrivilegeCount: 1,
                    Privileges: [LUID_AND_ATTRIBUTES { Luid: luid, Attributes: SE_PRIVILEGE_ENABLED }],
                };
                let _ = AdjustTokenPrivileges(token, false, Some(&tp), 0, None, None);
            }
            let _ = CloseHandle(token);
        }
        let Ok(gdi) = LoadLibraryW(w!("gdi32.dll")) else { return };
        let Some(f) = GetProcAddress(gdi, s!("D3DKMTSetProcessSchedulingPriorityClass")) else { return };
        let set: SetClass = std::mem::transmute(f);
        for (class, name) in [(REALTIME, "realtime"), (HIGH, "high")] {
            let status = set(GetCurrentProcess(), class);
            if status == 0 {
                tracing::info!(priority = name, "GPU priority raised so games don't starve the stream");
                return;
            }
            tracing::debug!(priority = name, status, "GPU priority refused");
        }
        tracing::warn!("couldn't raise the GPU priority: games using the whole GPU may slow the stream");
    }
}
