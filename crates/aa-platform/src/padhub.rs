#![allow(clippy::doc_markdown)] // product names read better plain
//! The host's side of DualSense pass-through: one virtual DualSense per
//! controller on the viewer, plugged into this PC over USB/IP.
//!
//! Life of a controller:
//! 1. The viewer starts sending its reports. A slot is created; the
//!    virtual controller is *not* plugged in yet, because the PC asks a new
//!    DualSense for its calibration/serial/firmware at once, and it should
//!    get the real ones. So we wait for those (or one second at most).
//! 2. It is plugged in: the server offers it and the platform's `attach`
//!    asks the USB/IP driver to connect (on Windows, usbip-win2).
//! 3. Input reports are replayed to the PC; output reports from games are
//!    sent back to the viewer, which writes them to the real controller.
//! 4. Unplugged when the viewer says so, or after 10 s of silence.
//!
//! If the driver is missing or attaching fails, the controller still
//! works: it falls back to a generic pad (a DualShock 4 through ViGEm, slot
//! 4..7 so it never collides with other controllers), with rumble, but
//! without adaptive triggers, light bar, touchpad and motion.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use aa_core::ds5::{self, PadMsg, USB_OUTPUT_LEN};
use aa_core::input::{GamepadKind, InputEvent, Rumble};

use crate::ds5dev::VirtualDualSense;
use crate::usbip::{Export, Server, UsbDevice};

/// Plugs a device offered at `server` with `busid` into this computer.
pub type Attach = Arc<dyn Fn(SocketAddr, &str) -> anyhow::Result<()> + Send + Sync>;

/// Wait at most this long for the real feature reports before plugging in.
const FEATURE_WAIT: Duration = Duration::from_secs(1);
/// After asking the driver to attach, give up on it after this long.
const ATTACH_WAIT: Duration = Duration::from_secs(6);
/// A controller silent this long is unplugged.
const SILENCE: Duration = Duration::from_secs(10);
/// Output reports are sent twice (UDP may drop one); the copy goes this
/// much later, unless a newer report replaced it.
const RESEND_AFTER: Duration = Duration::from_millis(40);
/// Generic fallback pads use ViGEm slots 4..7.
pub const FALLBACK_SLOT_BASE: u8 = 4;

pub struct PadHooks {
    /// `None`: no virtual-USB support on this platform; use the fallback.
    pub attach: Option<Attach>,
    /// Sends a message to the viewer.
    pub to_viewer: Arc<dyn Fn(PadMsg) + Send + Sync>,
    /// Input for the fallback generic pads.
    pub fallback: Box<dyn Fn(InputEvent) + Send>,
}

impl std::fmt::Debug for PadHooks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PadHooks").field("attach", &self.attach.is_some()).finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Waiting for the real feature reports.
    Gathering,
    /// The driver was asked to attach at this time.
    Attaching(Instant),
    Virtual,
    Generic,
}

#[derive(Debug, Default)]
struct LastOutput {
    report: Option<[u8; USB_OUTPUT_LEN]>,
    at: Option<Instant>,
    resent: bool,
}

struct Slot {
    dev: Arc<VirtualDualSense>,
    export: Option<Arc<Export>>,
    mode: Mode,
    created: Instant,
    last_heard: Instant,
    features: u8,
    last_output: Arc<Mutex<LastOutput>>,
}

pub struct PadHub {
    hooks: PadHooks,
    server: Option<Server>,
    slots: [Option<Slot>; ds5::SLOTS as usize],
    /// Attaching failed once; don't keep trying (and failing slowly).
    attach_broken: bool,
    /// Set by the attach thread when the driver's tool reports failure.
    attach_failed: Arc<std::sync::atomic::AtomicBool>,
}

impl std::fmt::Debug for PadHub {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PadHub").field("server", &self.server).finish_non_exhaustive()
    }
}

/// Bus id of a slot as the USB/IP driver sees it.
pub fn busid(slot: u8) -> String {
    format!("1-{}", slot + 1)
}

impl PadHub {
    pub fn new(hooks: PadHooks) -> Self {
        Self { hooks, server: None, slots: Default::default(), attach_broken: false, attach_failed: Arc::default() }
    }

    /// A message from the viewer.
    pub fn handle(&mut self, msg: &PadMsg) {
        match msg {
            PadMsg::Input { slot, report } => {
                if report[0] != 0x01 {
                    return; // not an input report: never plug in for junk
                }
                let s = self.slot(*slot);
                s.last_heard = Instant::now();
                if !s.dev.set_input(report) {
                    return;
                }
                let generic = s.mode == Mode::Generic;
                if let Some(e) = s.export.as_ref().filter(|_| s.mode == Mode::Virtual) {
                    e.doorbell.ring();
                }
                if generic {
                    let state = ds5::usb_input_to_state(report);
                    (self.hooks.fallback)(InputEvent::Gamepad { slot: FALLBACK_SLOT_BASE + slot, state });
                }
            }
            PadMsg::Feature { slot, report } => {
                let s = self.slot(*slot);
                s.last_heard = Instant::now();
                s.dev.set_feature(report);
                if let Some(i) = ds5::FEATURES_AT_ATTACH.iter().position(|id| Some(id) == report.first()) {
                    s.features |= 1 << i;
                }
            }
            PadMsg::Detach { slot } => self.unplug(*slot),
            PadMsg::Output { .. } | PadMsg::Audio { .. } => {} // host → viewer only
        }
        self.tick();
    }

    /// Rumble a game sent to a fallback pad: forward it as a DualSense
    /// output report so the real controller shakes.
    pub fn rumble(&self, r: Rumble) {
        let Some(slot) = r.slot.checked_sub(FALLBACK_SLOT_BASE).filter(|s| *s < ds5::SLOTS) else { return };
        if self.slots[usize::from(slot)].as_ref().is_some_and(|s| s.mode == Mode::Generic) {
            (self.hooks.to_viewer)(PadMsg::Output { slot, report: ds5::rumble_output(r.low_freq, r.high_freq) });
        }
    }

    /// Housekeeping; call often (every few tens of milliseconds).
    pub fn tick(&mut self) {
        let now = Instant::now();
        for slot in 0..ds5::SLOTS {
            let Some(s) = self.slots[usize::from(slot)].as_ref() else { continue };
            if s.last_heard.elapsed() > SILENCE {
                tracing::info!(slot, "DualSense silent; unplugging");
                self.unplug(slot);
                continue;
            }
            match s.mode {
                Mode::Gathering if s.features == 0b111 || s.created.elapsed() > FEATURE_WAIT => self.plug(slot),
                Mode::Attaching(at) => {
                    let attached = s.export.as_ref().is_some_and(|e| e.attached());
                    if attached {
                        tracing::info!(slot, "DualSense plugged in: triggers, haptics-rumble, light bar, touchpad and motion pass through");
                        self.set_mode(slot, Mode::Virtual);
                    } else if at.elapsed() > ATTACH_WAIT
                        || self.attach_failed.load(std::sync::atomic::Ordering::Relaxed)
                    {
                        tracing::warn!(slot, "the USB/IP driver never connected; using a generic controller");
                        self.attach_broken = true;
                        self.go_generic(slot);
                    }
                }
                _ => {}
            }
            // Second copy of the latest output report.
            if let Some(s) = self.slots[usize::from(slot)].as_ref() {
                let mut lo = s.last_output.lock().expect("last output");
                if let (Some(r), Some(at), false) = (lo.report, lo.at, lo.resent) {
                    if now.duration_since(at) >= RESEND_AFTER {
                        lo.resent = true;
                        drop(lo);
                        (self.hooks.to_viewer)(PadMsg::Output { slot, report: r });
                    }
                }
            }
        }
    }

    /// Unplug everything (the viewer left).
    pub fn clear(&mut self) {
        for slot in 0..ds5::SLOTS {
            self.unplug(slot);
        }
    }

    pub fn active(&self) -> usize {
        self.slots.iter().flatten().count()
    }

    fn set_mode(&mut self, slot: u8, mode: Mode) {
        if let Some(s) = self.slots[usize::from(slot)].as_mut() {
            s.mode = mode;
        }
    }

    fn slot(&mut self, slot: u8) -> &mut Slot {
        let to_viewer = Arc::clone(&self.hooks.to_viewer);
        self.slots[usize::from(slot)].get_or_insert_with(|| {
            tracing::info!(slot, "DualSense connected on the viewer");
            let last_output: Arc<Mutex<LastOutput>> = Arc::default();
            let lo = Arc::clone(&last_output);
            let audio_to_viewer = Arc::clone(&to_viewer);
            let on_output = Box::new(move |report: [u8; USB_OUTPUT_LEN]| {
                {
                    let mut l = lo.lock().expect("last output");
                    if l.report == Some(report) && !l.resent {
                        return; // games often repeat themselves; the resend covers it
                    }
                    *l = LastOutput { report: Some(report), at: Some(Instant::now()), resent: false };
                }
                to_viewer(PadMsg::Output { slot, report });
            });
            let mut dev = VirtualDualSense::new(slot, false, on_output);
            // The sound card half (speaker + haptics). AA_DS5_NO_AUDIO=1
            // turns it off, should a PC's USB/IP driver dislike it.
            if std::env::var_os("AA_DS5_NO_AUDIO").is_none() {
                dev = dev.with_audio(audio_sink(slot, audio_to_viewer));
            }
            let now = Instant::now();
            Slot {
                dev: Arc::new(dev),
                export: None,
                mode: Mode::Gathering,
                created: now,
                last_heard: now,
                features: 0,
                last_output,
            }
        })
    }

    fn plug(&mut self, slot: u8) {
        let Some(attach) = self.hooks.attach.clone().filter(|_| !self.attach_broken) else {
            self.go_generic(slot);
            return;
        };
        if self.server.is_none() {
            match Server::start(SocketAddr::from(([127, 0, 0, 1], 0))) {
                Ok(s) => self.server = Some(s),
                Err(e) => {
                    tracing::warn!("virtual USB server failed to start ({e}); using a generic controller");
                    self.attach_broken = true;
                    self.go_generic(slot);
                    return;
                }
            }
        }
        let server = self.server.as_ref().expect("started above");
        let Some(s) = self.slots[usize::from(slot)].as_mut() else { return };
        let dev: Arc<dyn UsbDevice> = Arc::clone(&s.dev) as Arc<dyn UsbDevice>;
        let export = server.add(Export::new(busid(slot), u32::from(slot) + 1, dev));
        s.export = Some(export);
        s.mode = Mode::Attaching(Instant::now());
        let addr = server.addr();
        let id = busid(slot);
        let failed = Arc::clone(&self.attach_failed);
        // The driver's tool can take a second; never block the network loop.
        let _ = std::thread::Builder::new().name("aa-usbip-attach".into()).spawn(move || {
            if let Err(e) = attach(addr, &id) {
                tracing::warn!("could not plug in the virtual DualSense: {e:#}");
                failed.store(true, std::sync::atomic::Ordering::Relaxed);
            }
        });
    }

    fn go_generic(&mut self, slot: u8) {
        let Some(s) = self.slots[usize::from(slot)].as_mut() else { return };
        if let (Some(e), Some(server)) = (s.export.take(), self.server.as_ref()) {
            server.remove(&e.busid);
        }
        s.mode = Mode::Generic;
        tracing::info!(slot, "DualSense working as a generic controller (buttons, sticks, rumble)");
        (self.hooks.fallback)(InputEvent::GamepadAttach {
            slot: FALLBACK_SLOT_BASE + slot,
            kind: GamepadKind::PlayStation,
        });
    }

    fn unplug(&mut self, slot: u8) {
        let Some(s) = self.slots[usize::from(slot)].take() else { return };
        if let (Some(e), Some(server)) = (s.export.as_ref(), self.server.as_ref()) {
            server.remove(&e.busid);
        }
        if s.mode == Mode::Generic {
            (self.hooks.fallback)(InputEvent::GamepadDetach { slot: FALLBACK_SLOT_BASE + slot });
        }
        tracing::info!(slot, "DualSense unplugged");
    }
}

/// Below this peak a 10 ms block counts as silence and is not sent.
const SILENCE_PEAK: i16 = 64;

/// Encodes the controller's sound (speaker pair and haptics pair) for the
/// viewer, skipping silence: games keep the stream open and write zeros
/// most of the time.
fn audio_sink(slot: u8, to_viewer: Arc<dyn Fn(PadMsg) + Send + Sync>) -> crate::ds5dev::AudioSink {
    struct Enc {
        speaker: Option<crate::audio::OpusEncoder>,
        haptics: Option<crate::audio::OpusEncoder>,
        frame: u16,
        logged: bool,
    }
    let enc = Mutex::new(Enc {
        speaker: crate::audio::OpusEncoder::new(96_000).ok(),
        haptics: crate::audio::OpusEncoder::new(96_000).ok(),
        frame: 0,
        logged: false,
    });
    Box::new(move |pcm: &[i16]| {
        let pair = |first: usize| -> Vec<i16> { pcm.chunks_exact(4).flat_map(|f| [f[first], f[first + 1]]).collect() };
        let (speaker, haptics) = (pair(0), pair(2));
        let loud = |p: &[i16]| p.iter().any(|s| s.unsigned_abs() > SILENCE_PEAK.unsigned_abs());
        let mut e = enc.lock().expect("ds5 audio");
        let e = &mut *e;
        e.frame = e.frame.wrapping_add(1);
        let encode = |enc: &mut Option<crate::audio::OpusEncoder>, p: &[i16]| -> Vec<u8> {
            match enc.as_mut() {
                Some(enc) if loud(p) => enc.encode(p).map(<[u8]>::to_vec).unwrap_or_default(),
                _ => Vec::new(),
            }
        };
        let speaker = encode(&mut e.speaker, &speaker);
        let haptics = encode(&mut e.haptics, &haptics);
        if speaker.is_empty() && haptics.is_empty() {
            return;
        }
        if !e.logged {
            e.logged = true;
            tracing::info!(slot, haptics = !haptics.is_empty(), "the game is sending sound/haptics to the DualSense");
        }
        to_viewer(PadMsg::Audio { slot, frame: e.frame, speaker, haptics });
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usbip::client::Client;

    type Seen = Arc<Mutex<Vec<PadMsg>>>;

    fn hub(attach: Option<Attach>) -> (PadHub, Seen, Arc<Mutex<Vec<InputEvent>>>) {
        let seen: Seen = Arc::default();
        let events: Arc<Mutex<Vec<InputEvent>>> = Arc::default();
        let (s, e) = (Arc::clone(&seen), Arc::clone(&events));
        let hooks = PadHooks {
            attach,
            to_viewer: Arc::new(move |m| s.lock().unwrap().push(m)),
            fallback: Box::new(move |ev| e.lock().unwrap().push(ev)),
        };
        (PadHub::new(hooks), seen, events)
    }

    fn features(h: &mut PadHub, slot: u8) {
        h.handle(&PadMsg::Feature { slot, report: vec![0x05; 41] });
        h.handle(&PadMsg::Feature { slot, report: vec![0x09, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF] });
        h.handle(&PadMsg::Feature { slot, report: vec![0x20; 64] });
    }

    /// The whole path, with a USB/IP client standing in for the Windows
    /// driver: plug-in, descriptors, real feature reports, held input
    /// transfers, game output reaching the viewer (twice), unplug.
    #[test]
    #[allow(clippy::too_many_lines, clippy::many_single_char_names)] // one story, start to end
    fn pass_through_end_to_end() {
        let (tx, rx) = std::sync::mpsc::channel::<(SocketAddr, String)>();
        let tx = Mutex::new(tx);
        let attach: Attach = Arc::new(move |a, b| {
            tx.lock().unwrap().send((a, b.to_owned())).unwrap();
            Ok(())
        });
        let (mut h, seen, events) = hub(Some(attach));
        let mut report = ds5::neutral_input();
        h.handle(&PadMsg::Input { slot: 2, report });
        features(&mut h, 2);
        let (addr, busid) = rx.recv_timeout(Duration::from_secs(2)).expect("attach asked for");
        assert_eq!(busid, "1-3");

        let (mut c, summary) = Client::import(addr, &busid).unwrap();
        assert_eq!(&summary[256..259], b"1-3");
        assert_eq!(u16::from_be_bytes([summary[300], summary[301]]), ds5::VID_SONY);
        assert_eq!(u16::from_be_bytes([summary[302], summary[303]]), ds5::PID_DUALSENSE);
        let (st, dd) = c.get_descriptor(1, 0, 0, 0, 18).unwrap();
        assert_eq!((st, dd.len(), dd[8], dd[9]), (0, 18, 0x4C, 0x05));
        let (_, cfg) = c.get_descriptor(2, 0, 0, 0, 255).unwrap();
        assert_eq!(usize::from(u16::from_le_bytes([cfg[2], cfg[3]])), cfg.len());
        let (_, rd) = c.get_descriptor(0x22, 0, 1, 3, 1024).unwrap();
        assert_eq!(rd, crate::ds5dev::report_descriptor());
        let (_, name) = c.get_descriptor(3, 2, 0, 0, 255).unwrap();
        assert_eq!(name.len(), 2 + 2 * "DualSense Wireless Controller".len());

        // Feature 0x09 is the real controller's, padded to size.
        c.submit(true, 0, 64, [0xA1, 1, 0x09, 0x03, 3, 0, 64, 0], &[]).unwrap();
        let (_, st, pairing) = c.reply_in().unwrap();
        assert_eq!((st, pairing.len()), (0, 20));
        assert_eq!(pairing[1..7], [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF]);

        for _ in 0..50 {
            h.tick();
            if h.slots[2].as_ref().unwrap().mode == Mode::Virtual {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(h.slots[2].as_ref().unwrap().mode, Mode::Virtual);

        // Interrupt IN: first read gets the current report at once…
        let seq = c.submit(true, 0x84, 64, [0; 8], &[]).unwrap();
        let (got, st, data) = c.reply_in().unwrap();
        assert_eq!((got, st, data.len(), data[0]), (seq, 0, 64, 0x01));
        // …the next is held until something new arrives.
        let seq = c.submit(true, 0x84, 64, [0; 8], &[]).unwrap();
        report[8] = 0x28; // cross + d-pad none
        let t = Instant::now();
        h.handle(&PadMsg::Input { slot: 2, report });
        let (got, _, data) = c.reply_in().unwrap();
        assert_eq!((got, data[8]), (seq, 0x28));
        assert!(t.elapsed() < Duration::from_millis(50));

        // A game writes rumble + a trigger effect; the viewer gets it.
        let mut out = [0u8; USB_OUTPUT_LEN];
        out[0] = 0x02;
        out[1] = 0x0F;
        out[3] = 0x80;
        out[11] = 0x26;
        let seq = c.submit(false, 0x03, 48, [0; 8], &out).unwrap();
        let (cmd, got, st, n) = c.reply().unwrap();
        assert_eq!((cmd, got, st, n), (3, seq, 0, 48));
        std::thread::sleep(RESEND_AFTER + Duration::from_millis(10));
        h.tick();
        let outs: Vec<_> = seen.lock().unwrap().clone();
        assert_eq!(outs, vec![PadMsg::Output { slot: 2, report: out }; 2], "sent, then resent once");

        // The game plays haptics: 4-channel USB audio, 10 packets of 1 ms
        // per transfer, as Windows' audio driver sends them. Completions
        // come at real-time pace (40 ms of audio takes ~40 ms).
        seen.lock().unwrap().clear();
        let mut pcm = Vec::new();
        for i in 0..480 {
            for ch in 0..4 {
                let v: i16 = if ch == 3 && i % 24 < 12 {
                    9000
                } else if ch == 3 {
                    -9000
                } else {
                    0
                };
                pcm.extend_from_slice(&v.to_le_bytes());
            }
        }
        let t = Instant::now();
        let mut seqs = Vec::new();
        for _ in 0..4 {
            seqs.push(c.submit_iso(false, 0x01, 384, 10, &pcm[..3840]).unwrap());
        }
        for want in seqs {
            let (got, _, actual) = c.reply_iso(false).unwrap();
            assert_eq!(got, want);
            assert_eq!(actual, vec![384; 10]);
        }
        let took = t.elapsed();
        assert!(took >= Duration::from_millis(35) && took < Duration::from_millis(200), "{took:?}");
        let audio: Vec<_> =
            seen.lock().unwrap().iter().filter(|m| matches!(m, PadMsg::Audio { .. })).cloned().collect();
        assert_eq!(audio.len(), 4, "one message per 10 ms");
        assert!(
            matches!(&audio[0], PadMsg::Audio { slot: 2, speaker, haptics, .. } if speaker.is_empty() && !haptics.is_empty())
        );
        // The microphone stream: silence, at pace, right sizes.
        c.submit_iso(true, 0x82, 196, 5, &[]).unwrap();
        let (_, data, actual) = c.reply_iso(true).unwrap();
        assert_eq!((data.len(), actual.len()), (980, 5));

        // A pending transfer cancelled by the driver.
        let pending = c.submit(true, 0x84, 64, [0; 8], &[]).unwrap();
        let _ = c.reply_in(); // may complete with the stale keep-alive first
        let pending2 = c.submit(true, 0x84, 64, [0; 8], &[]).unwrap();
        let un = c.unlink(pending2).unwrap();
        loop {
            let (cmd, s, status, n) = c.reply().unwrap();
            if cmd == 4 {
                assert_eq!(s, un);
                assert!(status == -104 || status == 0);
                break;
            }
            let mut d = vec![0u8; n];
            std::io::Read::read_exact(&mut c.s, &mut d).unwrap();
            assert!(s == pending || s == pending2);
        }

        // Viewer says it is gone: the PC sees it unplugged.
        h.handle(&PadMsg::Detach { slot: 2 });
        assert_eq!(h.active(), 0);
        let mut b = [0u8; 48];
        c.s.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        let r = loop {
            match std::io::Read::read(&mut c.s, &mut b) {
                Ok(0) | Err(_) => break true,
                Ok(_) => {}
            }
        };
        assert!(r, "connection closed on unplug");
        assert!(events.lock().unwrap().is_empty(), "no fallback used");
    }

    /// Haptics written by a game as USB audio reach the viewer as Opus;
    /// silence costs nothing.
    #[test]
    fn haptic_audio_reaches_the_viewer() {
        let seen: Seen = Arc::default();
        let s = Arc::clone(&seen);
        let sink = audio_sink(1, Arc::new(move |m| s.lock().unwrap().push(m)));
        sink(&[0i16; crate::ds5dev::AUDIO_FRAME]);
        assert!(seen.lock().unwrap().is_empty(), "silence is not sent");
        let mut pcm = vec![0i16; crate::ds5dev::AUDIO_FRAME];
        for (i, f) in pcm.chunks_exact_mut(4).enumerate() {
            f[2] = if i % 24 < 12 { 8000 } else { -8000 }; // 2 kHz buzz on the left actuator
        }
        for _ in 0..5 {
            sink(&pcm);
        }
        let got = seen.lock().unwrap().clone();
        assert_eq!(got.len(), 5);
        let PadMsg::Audio { slot, frame, speaker, haptics } = &got[4] else { panic!() };
        assert_eq!((*slot, *frame, speaker.len()), (1, 6, 0));
        // Decodes back to a buzz on the left haptic channel only.
        let mut dec = crate::audio::OpusDecoder::new().unwrap();
        let mut out = vec![0i16; crate::audio::FRAME_LEN_I16];
        for m in &got {
            let PadMsg::Audio { haptics, .. } = m else { panic!() };
            dec.decode(haptics, &mut out).unwrap();
        }
        let (l, r) = aa_core::ds5::haptics_to_rumble(&out);
        assert!(l > 100 && r < 20, "left {l} right {r}");
        assert!(haptics.len() < 300);
    }

    #[test]
    fn missing_driver_falls_back_to_a_generic_pad_with_rumble() {
        let attach: Attach = Arc::new(|_, _| Err(anyhow::anyhow!("usbip.exe not found")));
        let (mut h, seen, events) = hub(Some(attach));
        let mut report = ds5::neutral_input();
        features(&mut h, 0);
        // The tool says it failed: generic at once, not after ATTACH_WAIT.
        for _ in 0..100 {
            h.tick();
            if h.slots[0].as_ref().unwrap().mode == Mode::Generic {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(h.slots[0].as_ref().unwrap().mode, Mode::Generic);
        report[8] = 0x28;
        h.handle(&PadMsg::Input { slot: 0, report });
        h.rumble(Rumble { slot: 4, low_freq: 10, high_freq: 20 });
        h.rumble(Rumble { slot: 1, low_freq: 10, high_freq: 20 }); // not ours
                                                                   // Later controllers skip the broken driver straight away.
        features(&mut h, 1);
        assert_eq!(h.slots[1].as_ref().unwrap().mode, Mode::Generic);
        h.clear();
        let ev = events.lock().unwrap().clone();
        assert_eq!(ev[0], InputEvent::GamepadAttach { slot: 4, kind: GamepadKind::PlayStation });
        assert!(
            matches!(ev[1], InputEvent::Gamepad { slot: 4, state } if state.buttons == aa_core::input::gamepad_buttons::CROSS)
        );
        assert!(ev.contains(&InputEvent::GamepadDetach { slot: 4 }));
        assert!(ev.contains(&InputEvent::GamepadDetach { slot: 5 }));
        assert_eq!(seen.lock().unwrap().clone(), vec![PadMsg::Output { slot: 0, report: ds5::rumble_output(10, 20) }]);
    }

    #[test]
    fn a_driver_that_never_connects_is_given_up_on() {
        let attach: Attach = Arc::new(|_, _| Ok(()));
        let (mut h, _, events) = hub(Some(attach));
        features(&mut h, 1);
        assert!(matches!(h.slots[1].as_ref().unwrap().mode, Mode::Attaching(_)));
        h.slots[1].as_mut().unwrap().mode = Mode::Attaching(Instant::now().checked_sub(ATTACH_WAIT * 2).unwrap());
        h.tick();
        assert_eq!(h.slots[1].as_ref().unwrap().mode, Mode::Generic);
        assert_eq!(events.lock().unwrap()[0], InputEvent::GamepadAttach { slot: 5, kind: GamepadKind::PlayStation });
    }

    #[test]
    fn no_virtual_usb_means_generic_at_once_and_silence_unplugs() {
        let (mut h, _, events) = hub(None);
        h.handle(&PadMsg::Input { slot: 3, report: ds5::neutral_input() });
        h.slots[3].as_mut().unwrap().created -= FEATURE_WAIT * 2;
        h.tick();
        assert_eq!(h.slots[3].as_ref().unwrap().mode, Mode::Generic);
        h.slots[3].as_mut().unwrap().last_heard -= SILENCE * 2;
        h.tick();
        assert_eq!(h.active(), 0);
        assert_eq!(events.lock().unwrap().last(), Some(&InputEvent::GamepadDetach { slot: 7 }));
        // Junk never creates a slot.
        h.handle(&PadMsg::Input { slot: 0, report: [0; 64] });
        assert!(h.slots[0].is_none());
    }
}
