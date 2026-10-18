//! When Valorant's anti-cheat runs, click through a virtual USB mouse.
//!
//! In plain words: while Riot Vanguard runs, clicks sent the normal way
//! (`SendInput`) are thrown away. Every two seconds the input thread asks
//! [`route`] whether Vanguard is running. If it is, we start a tiny USB/IP
//! server with one virtual mouse (`hidmouse.rs`) and ask usbip-win2 to
//! plug it in; from then on clicks, aiming (game mouse) and the wheel go
//! through it. Pointing still uses `SetCursorPos`, which Vanguard allows.
//! When Vanguard stops, the mouse is unplugged and everything goes back to
//! the normal path.
//!
//! Needs usbip-win2 (the same free driver the DualSense uses). Without it,
//! clicks stay blocked and the viewer's corner note says what to install.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::hidmouse::VirtualMouse;
use crate::usbip::{Export, Server};

/// How often to ask whether Vanguard runs (the question costs ~1 ms).
const RECHECK: Duration = Duration::from_secs(2);
const BUSID: &str = "1-1";

/// What the viewer should be told (see `crate::input_blocker`).
pub const NOT_NEEDED: u8 = 0;
pub const STARTING: u8 = 1;
pub const WORKING: u8 = 2;
pub const NO_DRIVER: u8 = 3;
static STATUS: AtomicU8 = AtomicU8::new(NOT_NEEDED);

/// Current state, for the viewer's corner note.
pub fn status() -> u8 {
    STATUS.load(Ordering::Relaxed)
}

struct Plugged {
    export: Arc<Export>,
    mouse: Arc<VirtualMouse>,
}

#[derive(Default)]
pub struct Router {
    checked: Option<Instant>,
    /// Started on first need and kept (it only listens on this PC).
    server: Option<Server>,
    plugged: Option<Plugged>,
    /// usbip-win2 failed once this Vanguard run: don't retry every 2 s.
    gave_up: bool,
}

impl std::fmt::Debug for Router {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Router").field("plugged", &self.plugged.is_some()).field("gave_up", &self.gave_up).finish()
    }
}

impl Router {
    /// The virtual mouse to use for clicks and relative moves right now,
    /// or None for the normal path. Call from the input thread.
    pub fn route(&mut self) -> Option<Arc<VirtualMouse>> {
        if self.checked.map_or(true, |t| t.elapsed() >= RECHECK) {
            self.checked = Some(Instant::now());
            self.recheck();
        }
        let p = self.plugged.as_ref()?;
        if p.export.attached() {
            if STATUS.swap(WORKING, Ordering::Relaxed) != WORKING {
                tracing::info!("virtual USB mouse plugged in: clicks work while Vanguard runs");
            }
            Some(Arc::clone(&p.mouse))
        } else {
            None
        }
    }

    fn recheck(&mut self) {
        if STATUS.load(Ordering::Relaxed) == NO_DRIVER && self.plugged.is_some() {
            // usbip-win2 missing or failing: stop trying until Vanguard
            // restarts (the corner note tells the viewer what to install).
            self.unplug();
            self.gave_up = true;
        }
        let vanguard = super::vanguard_running();
        match (vanguard, self.plugged.is_some()) {
            (true, false) if !self.gave_up => self.plug(),
            (false, true) => {
                self.unplug();
                tracing::info!("Vanguard stopped: virtual USB mouse unplugged, normal clicks again");
                STATUS.store(NOT_NEEDED, Ordering::Relaxed);
            }
            (false, false) => {
                self.gave_up = false;
                STATUS.store(NOT_NEEDED, Ordering::Relaxed);
            }
            _ => {}
        }
    }

    fn unplug(&mut self) {
        if let Some(p) = self.plugged.take() {
            p.mouse.release_all();
            p.export.unplug();
        }
        if let Some(s) = &self.server {
            s.remove(BUSID);
        }
    }

    fn plug(&mut self) {
        if self.server.is_none() {
            match Server::start("127.0.0.1:0".parse().expect("address")) {
                Ok(s) => self.server = Some(s),
                Err(e) => {
                    tracing::warn!("virtual mouse: couldn't start the USB server: {e}");
                    self.gave_up = true;
                    return;
                }
            }
        }
        let Some(server) = self.server.as_ref() else { return };
        let mouse = Arc::new(VirtualMouse::default());
        let mut export = Export::new(BUSID, 1, Arc::clone(&mouse) as Arc<dyn crate::usbip::UsbDevice>);
        export.doorbell = Arc::clone(&mouse.doorbell);
        let export = server.add(export);
        let addr = server.addr();
        STATUS.store(STARTING, Ordering::Relaxed);
        tracing::info!("Vanguard is running: plugging in a virtual USB mouse so clicks get through");
        // `usbip attach` blocks until the device is connected or fails.
        let _ = std::thread::Builder::new().name("aa-vmouse-attach".into()).spawn(move || {
            if let Err(e) = super::usbip_attach(addr, BUSID) {
                tracing::warn!("virtual mouse: {e}");
                STATUS.store(NO_DRIVER, Ordering::Relaxed);
            }
        });
        self.plugged = Some(Plugged { export, mouse });
    }

    /// The viewer left: let go of any held button.
    pub fn release_all(&self) {
        if let Some(p) = &self.plugged {
            p.mouse.release_all();
        }
    }
}

/// One router for the input thread (the injector is rebuilt rarely, the
/// plugged mouse should survive that).
pub fn shared() -> &'static Mutex<Router> {
    static R: std::sync::OnceLock<Mutex<Router>> = std::sync::OnceLock::new();
    R.get_or_init(Mutex::default)
}
