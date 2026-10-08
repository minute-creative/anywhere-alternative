//! Sharing the PC before anyone signs in (the Windows sign-in screen).
//!
//! **Why this is needed.** Windows draws the sign-in screen, the lock
//! screen and "do you allow this app" prompts on a separate, protected
//! desktop (`Winlogon`). Ordinary programs can neither see nor type into
//! it, which is why the picture used to pause on "the PC is locked". The
//! one account Windows lets in is the system itself (`SYSTEM`).
//!
//! **How.** A small Windows service (`aa-host --windows-service`) starts
//! with Windows, before anyone signs in, and runs as `SYSTEM`. Services
//! live in an invisible session of their own, so it starts the real host
//! (`aa-host --service`) *into the session on the screen*, still as
//! `SYSTEM`. That host follows whichever desktop is showing (sign-in
//! screen, the user's desktop, a security prompt) for capture and input.
//! When someone signs in or out, the service makes sure exactly one host
//! runs in the session on the screen.

// FFI code: `unsafe` is the point here, each block carries a SAFETY note.
#![allow(unsafe_code, clippy::pedantic)]

use std::cell::Cell;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, ERROR_CANCELLED, HANDLE, HWND, WAIT_TIMEOUT};
use windows::Win32::Security::{
    DuplicateTokenEx, SecurityIdentification, SetTokenInformation, TokenPrimary, TokenSessionId, TOKEN_ACCESS_MASK,
    TOKEN_ADJUST_DEFAULT, TOKEN_ADJUST_SESSIONID, TOKEN_ALL_ACCESS, TOKEN_ASSIGN_PRIMARY, TOKEN_DUPLICATE, TOKEN_QUERY,
};
use windows::Win32::System::RemoteDesktop::WTSGetActiveConsoleSessionId;
use windows::Win32::System::Services::{
    RegisterServiceCtrlHandlerExW, SetServiceStatus, StartServiceCtrlDispatcherW, SERVICE_ACCEPT_SHUTDOWN,
    SERVICE_ACCEPT_STOP, SERVICE_CONTROL_INTERROGATE, SERVICE_CONTROL_SHUTDOWN, SERVICE_CONTROL_STOP, SERVICE_RUNNING,
    SERVICE_STATUS, SERVICE_STATUS_CURRENT_STATE, SERVICE_STATUS_HANDLE, SERVICE_STOPPED, SERVICE_TABLE_ENTRYW,
    SERVICE_WIN32_OWN_PROCESS,
};
use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, OpenInputDesktop, SetThreadDesktop, DESKTOP_ACCESS_FLAGS, DESKTOP_CONTROL_FLAGS, HDESK,
};
use windows::Win32::System::Threading::{
    CreateProcessAsUserW, GetCurrentProcess, GetExitCodeProcess, OpenProcessToken, TerminateProcess,
    WaitForSingleObject, CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT, PROCESS_INFORMATION, STARTUPINFOW,
};
use windows::Win32::UI::Shell::{ShellExecuteExW, SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW};

pub const SERVICE_NAME: &str = "AnywhereHost";

// ---------------------------------------------------------------------------
// Following the desktop that is on the screen
// ---------------------------------------------------------------------------

thread_local! {
    /// The desktop handle this thread switched to (ours to close later).
    static HELD: Cell<usize> = const { Cell::new(0) };
    static LAST_CHECK: Cell<Option<Instant>> = const { Cell::new(None) };
}

/// Move the calling thread onto the desktop currently on the screen (sign-in
/// screen, user desktop, security prompt). Only `SYSTEM` may open the
/// protected ones; for anyone else this fails quietly and nothing changes.
pub fn follow_input_desktop() -> bool {
    const GENERIC_ALL: u32 = 0x1000_0000;
    // SAFETY: plain Win32 calls; the handle is closed on failure, or kept
    // for this thread (the previous one we kept is closed instead).
    unsafe {
        let Ok(d) = OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, DESKTOP_ACCESS_FLAGS(GENERIC_ALL)) else {
            return false;
        };
        if SetThreadDesktop(d).is_err() {
            let _ = CloseDesktop(d);
            return false;
        }
        let old = HELD.with(|h| h.replace(d.0 as usize));
        if old != 0 && old != d.0 as usize {
            let _ = CloseDesktop(HDESK(old as *mut _));
        }
    }
    true
}

/// [`follow_input_desktop`] at most four times a second (input path).
pub fn follow_input_desktop_throttled() {
    let due = LAST_CHECK.with(|c| {
        let due = c.get().map_or(true, |t| t.elapsed() > Duration::from_millis(250));
        if due {
            c.set(Some(Instant::now()));
        }
        due
    });
    if due {
        follow_input_desktop();
    }
}

// ---------------------------------------------------------------------------
// The service
// ---------------------------------------------------------------------------

static STOP: AtomicBool = AtomicBool::new(false);
static STATUS_HANDLE: AtomicUsize = AtomicUsize::new(0);

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn set_state(state: SERVICE_STATUS_CURRENT_STATE, accept: u32) {
    let h = STATUS_HANDLE.load(Ordering::Relaxed);
    if h == 0 {
        return;
    }
    let status = SERVICE_STATUS {
        dwServiceType: SERVICE_WIN32_OWN_PROCESS,
        dwCurrentState: state,
        dwControlsAccepted: accept,
        dwWin32ExitCode: 0,
        dwServiceSpecificExitCode: 0,
        dwCheckPoint: 0,
        dwWaitHint: 5000,
    };
    // SAFETY: the handle came from RegisterServiceCtrlHandlerExW.
    let _ = unsafe { SetServiceStatus(SERVICE_STATUS_HANDLE(h as *mut _), &status) };
}

unsafe extern "system" fn control(code: u32, _: u32, _: *mut core::ffi::c_void, _: *mut core::ffi::c_void) -> u32 {
    match code {
        SERVICE_CONTROL_STOP | SERVICE_CONTROL_SHUTDOWN => {
            STOP.store(true, Ordering::Relaxed);
            0
        }
        SERVICE_CONTROL_INTERROGATE => 0,
        _ => 120, // ERROR_CALL_NOT_IMPLEMENTED
    }
}

unsafe extern "system" fn service_main(_argc: u32, _argv: *mut PWSTR) {
    let name = wide(SERVICE_NAME);
    // SAFETY: name outlives the call; `control` is a valid handler.
    let Ok(h) = (unsafe { RegisterServiceCtrlHandlerExW(PCWSTR(name.as_ptr()), Some(control), None) }) else {
        return;
    };
    STATUS_HANDLE.store(h.0 as usize, Ordering::Relaxed);
    set_state(SERVICE_RUNNING, SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SHUTDOWN);
    supervise();
    set_state(SERVICE_STOPPED, 0);
}

/// Entry point for `aa-host --windows-service` (called by Windows).
pub fn run_service() -> anyhow::Result<()> {
    let mut name = wide(SERVICE_NAME);
    let table = [
        SERVICE_TABLE_ENTRYW { lpServiceName: PWSTR(name.as_mut_ptr()), lpServiceProc: Some(service_main) },
        SERVICE_TABLE_ENTRYW::default(),
    ];
    // SAFETY: the table and name live until the dispatcher returns (when
    // the service has stopped).
    unsafe { StartServiceCtrlDispatcherW(table.as_ptr()) }
        .map_err(|e| anyhow::anyhow!("not started by Windows as a service ({e})"))
}

struct Child {
    process: HANDLE,
    session: u32,
}

fn alive(c: &Child) -> bool {
    // SAFETY: a process handle we own.
    unsafe { WaitForSingleObject(c.process, 0) == WAIT_TIMEOUT }
}

/// Keep exactly one host running in the session that is on the screen.
fn supervise() {
    let dir = crate::trust::host_dir();
    let stop_file = dir.join("service.stop");
    let exe = std::env::current_exe().unwrap_or_default();
    let mut child: Option<Child> = None;
    let mut last_fail: Option<Instant> = None;
    tracing::info!("sharing service running");
    while !STOP.load(Ordering::Relaxed) {
        // SAFETY: no arguments.
        let session = unsafe { WTSGetActiveConsoleSessionId() };
        if let Some(c) = child.take() {
            if alive(&c) && c.session == session {
                child = Some(c);
            } else {
                if alive(&c) {
                    tracing::info!(from = c.session, to = session, "a different session is on the screen now");
                    end_child(&c, &stop_file);
                } else {
                    let mut code = 0u32;
                    // SAFETY: valid handle and out-pointer.
                    let _ = unsafe { GetExitCodeProcess(c.process, &mut code) };
                    tracing::info!(code, "the host stopped; starting it again");
                }
                // SAFETY: closing our own handle once.
                let _ = unsafe { CloseHandle(c.process) };
            }
        }
        let backoff = last_fail.is_some_and(|t| t.elapsed() < Duration::from_secs(5));
        if child.is_none() && session != u32::MAX && !backoff {
            match launch_in_session(&exe, session, &stop_file) {
                Ok(process) => {
                    tracing::info!(session, "host started on the screen's session");
                    child = Some(Child { process, session });
                }
                Err(e) => {
                    tracing::warn!(session, "could not start the host: {e}");
                    last_fail = Some(Instant::now());
                }
            }
        }
        for _ in 0..5 {
            if STOP.load(Ordering::Relaxed) {
                break;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }
    if let Some(c) = child {
        end_child(&c, &stop_file);
        // SAFETY: closing our own handle once.
        let _ = unsafe { CloseHandle(c.process) };
    }
    tracing::info!("sharing service stopped");
}

/// Ask the host to stop (it says goodbye and restores the speakers), and
/// make sure it has after a few seconds.
fn end_child(c: &Child, stop_file: &std::path::Path) {
    let _ = std::fs::write(stop_file, b"stop");
    // SAFETY: a process handle we own.
    unsafe {
        if WaitForSingleObject(c.process, 4000) == WAIT_TIMEOUT {
            let _ = TerminateProcess(c.process, 1);
        }
    }
    let _ = std::fs::remove_file(stop_file);
}

/// Start `aa-host --service` as SYSTEM inside `session`.
fn launch_in_session(exe: &std::path::Path, session: u32, stop_file: &std::path::Path) -> anyhow::Result<HANDLE> {
    // SAFETY: standard token duplication; every handle is closed below.
    unsafe {
        let mut own = HANDLE::default();
        let access: TOKEN_ACCESS_MASK =
            TOKEN_DUPLICATE | TOKEN_QUERY | TOKEN_ASSIGN_PRIMARY | TOKEN_ADJUST_SESSIONID | TOKEN_ADJUST_DEFAULT;
        OpenProcessToken(GetCurrentProcess(), access, &mut own)?;
        let mut token = HANDLE::default();
        let dup = DuplicateTokenEx(own, TOKEN_ALL_ACCESS, None, SecurityIdentification, TokenPrimary, &mut token);
        let _ = CloseHandle(own);
        dup?;
        let set = SetTokenInformation(
            token,
            TokenSessionId,
            (&session as *const u32).cast(),
            std::mem::size_of::<u32>() as u32,
        );
        if let Err(e) = set {
            let _ = CloseHandle(token);
            return Err(e.into());
        }
        let mut desktop = wide("winsta0\\default");
        let si = STARTUPINFOW {
            cb: std::mem::size_of::<STARTUPINFOW>() as u32,
            lpDesktop: PWSTR(desktop.as_mut_ptr()),
            ..Default::default()
        };
        let mut cmd = wide(&format!("\"{}\" --service --stop-file \"{}\"", exe.display(), stop_file.display()));
        let mut pi = PROCESS_INFORMATION::default();
        let r = CreateProcessAsUserW(
            Some(token),
            PCWSTR::null(),
            Some(PWSTR(cmd.as_mut_ptr())),
            None,
            None,
            false,
            CREATE_NO_WINDOW | CREATE_UNICODE_ENVIRONMENT,
            None,
            PCWSTR::null(),
            &si,
            &mut pi,
        );
        let _ = CloseHandle(token);
        r?;
        let _ = CloseHandle(pi.hThread);
        Ok(pi.hProcess)
    }
}

// ---------------------------------------------------------------------------
// Running a setup step as administrator (the app's one-time setup)
// ---------------------------------------------------------------------------

/// Run `file params` elevated (Windows shows its "allow changes?" prompt)
/// and wait for it. Returns its exit code; an error if the prompt was
/// declined.
pub fn run_elevated(file: &str, params: &str) -> anyhow::Result<u32> {
    let (verb, file_w, params_w) = (wide("runas"), wide(file), wide(params));
    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC,
        hwnd: HWND::default(),
        lpVerb: PCWSTR(verb.as_ptr()),
        lpFile: PCWSTR(file_w.as_ptr()),
        lpParameters: PCWSTR(params_w.as_ptr()),
        nShow: 0, // SW_HIDE
        ..Default::default()
    };
    // SAFETY: every string outlives the call; the process handle is ours.
    unsafe {
        if let Err(e) = ShellExecuteExW(&mut info) {
            if e.code() == ERROR_CANCELLED.to_hresult() {
                anyhow::bail!("cancelled");
            }
            return Err(e.into());
        }
        if info.hProcess.is_invalid() {
            return Ok(0);
        }
        let _ = WaitForSingleObject(info.hProcess, 120_000);
        let mut code = 0u32;
        let _ = GetExitCodeProcess(info.hProcess, &mut code);
        let _ = CloseHandle(info.hProcess);
        Ok(code)
    }
}
