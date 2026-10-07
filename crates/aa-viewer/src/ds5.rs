// Helpers used only by the Mac/Windows reader.
#![cfg_attr(not(any(target_os = "macos", target_os = "windows")), allow(dead_code))]
#![allow(clippy::doc_markdown)] // product names read better plain
//! DualSense pass-through, viewer side: read the real controller's raw
//! reports and write back what the PC's games send it.
//!
//! Why not through `gilrs` like other pads: gilrs turns a controller into
//! "buttons and sticks", which throws away the touchpad, motion sensors and
//! battery, and has no way to send adaptive-trigger, light-bar or LED
//! commands back. Here the controller's own bytes go to the PC unchanged,
//! where a virtual DualSense replays them, and the game's own commands come
//! back unchanged. `gilrs` is told to leave DualSense pads alone while this
//! reader has them (see [`RAW_ACTIVE`]).
//!
//! USB and Bluetooth both work. Over Bluetooth the controller starts in a
//! reduced mode; reading its calibration (feature report 0x05) switches it
//! to full reports, which we convert to USB layout (and outputs back to
//! Bluetooth layout, with the checksum the controller insists on).

use std::sync::atomic::{AtomicUsize, Ordering};

use aa_core::ds5::PadMsg;
use tokio::sync::mpsc;

use crate::link::ViewerCommand;

/// How many DualSenses this reader currently has open. While non-zero,
/// the generic controller reader skips DualSense pads (else the PC would
/// get every press twice).
pub static RAW_ACTIVE: AtomicUsize = AtomicUsize::new(0);

pub fn is_dualsense(vendor: Option<u16>, product: Option<u16>) -> bool {
    vendor == Some(aa_core::ds5::VID_SONY)
        && matches!(product, Some(aa_core::ds5::PID_DUALSENSE | aa_core::ds5::PID_DUALSENSE_EDGE))
}

pub fn raw_active() -> bool {
    RAW_ACTIVE.load(Ordering::Relaxed) > 0
}

/// Start the reader. Returns where to send output reports from the host.
pub fn spawn(commands: mpsc::Sender<ViewerCommand>) -> Option<std::sync::mpsc::Sender<PadMsg>> {
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    {
        real::spawn(commands)
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        drop(commands);
        None
    }
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
mod real {
    use std::ffi::CString;
    use std::sync::atomic::Ordering;
    use std::sync::mpsc as smpsc;
    use std::time::{Duration, Instant};

    use aa_core::ds5::{self, PadMsg, USB_INPUT_LEN};
    use aa_platform::audio::{OpusDecoder, PadSpeaker};
    use hidapi::{BusType, HidApi, HidDevice};
    use tokio::sync::mpsc;

    use super::RAW_ACTIVE;
    use crate::link::ViewerCommand;

    /// Feature reports are re-sent this often (UDP may lose the first).
    const FEATURE_REPEAT: Duration = Duration::from_secs(2);

    struct Open {
        path: CString,
        bt: bool,
        to_device: smpsc::Sender<[u8; ds5::USB_OUTPUT_LEN]>,
        done: smpsc::Receiver<()>,
        sound: Sound,
    }

    /// The game's sound for one controller: speaker and haptics pairs.
    struct Sound {
        speaker: Option<OpusDecoder>,
        haptics: Option<OpusDecoder>,
        pcm: Vec<i16>,
        /// Haptics are being imitated with the rumble motors right now.
        rumbling: bool,
        last_haptics: Instant,
    }

    impl Sound {
        fn new() -> Self {
            Self {
                speaker: OpusDecoder::new().ok(),
                haptics: OpusDecoder::new().ok(),
                pcm: vec![0; aa_platform::audio::FRAME_LEN_I16],
                rumbling: false,
                last_haptics: Instant::now(),
            }
        }
    }

    /// The DualSense sound card on this machine, opened when first needed
    /// (and retried now and then: it only exists while a cable is in).
    #[derive(Default)]
    struct Card {
        card: Option<PadSpeaker>,
        tried: Option<Instant>,
    }

    impl Card {
        fn get(&mut self) -> Option<&PadSpeaker> {
            if self.card.as_ref().is_some_and(PadSpeaker::broken) {
                self.card = None;
            }
            if self.card.is_none() && self.tried.map_or(true, |t| t.elapsed() > Duration::from_secs(5)) {
                self.tried = Some(Instant::now());
                match PadSpeaker::open() {
                    Ok(c) => self.card = Some(c),
                    Err(e) => tracing::info!("DualSense haptics will be imitated with rumble: {e}"),
                }
            }
            self.card.as_ref()
        }
    }

    /// Play (or, without the sound card, imitate) the game's sound.
    fn play(o: &mut Open, card: &mut Card, speaker: &[u8], haptics: &[u8]) {
        let snd = &mut o.sound;
        let usb_card = if o.bt { None } else { card.get() };
        if !speaker.is_empty() {
            if let (Some(dec), Some(c)) = (snd.speaker.as_mut(), usb_card) {
                if dec.decode(speaker, &mut snd.pcm).is_ok() {
                    c.push_speaker(&snd.pcm);
                }
            }
        }
        if haptics.is_empty() {
            return;
        }
        let Some(dec) = snd.haptics.as_mut() else { return };
        if dec.decode(haptics, &mut snd.pcm).is_err() {
            return;
        }
        snd.last_haptics = Instant::now();
        match usb_card {
            Some(c) => c.push_haptics(&snd.pcm),
            None => {
                let (l, r) = ds5::haptics_to_rumble(&snd.pcm);
                snd.rumbling = l > 0 || r > 0;
                let _ = o.to_device.send(ds5::rumble_output(l, r));
            }
        }
    }

    pub fn spawn(commands: mpsc::Sender<ViewerCommand>) -> Option<smpsc::Sender<PadMsg>> {
        let (out_tx, out_rx) = smpsc::channel::<PadMsg>();
        std::thread::Builder::new()
            .name("aa-ds5".into())
            .spawn(move || {
                let mut api = match HidApi::new() {
                    Ok(a) => a,
                    Err(e) => {
                        tracing::warn!("DualSense direct access unavailable ({e}); it will work as a basic controller");
                        return;
                    }
                };
                let mut slots: [Option<Open>; ds5::SLOTS as usize] = Default::default();
                let mut card = Card::default();
                let mut last_scan = Instant::now().checked_sub(Duration::from_secs(5)).unwrap_or_else(Instant::now);
                loop {
                    // Outputs from the host → the right device thread, for
                    // 30 ms; then housekeeping.
                    let until = Instant::now() + Duration::from_millis(30);
                    while let Some(left) = until.checked_duration_since(Instant::now()) {
                        match out_rx.recv_timeout(left) {
                            Ok(PadMsg::Output { slot, report }) => {
                                if let Some(o) = slots.get(usize::from(slot)).and_then(Option::as_ref) {
                                    let _ = o.to_device.send(report);
                                }
                            }
                            Ok(PadMsg::Audio { slot, speaker, haptics, .. }) => {
                                if let Some(o) = slots.get_mut(usize::from(slot)).and_then(Option::as_mut) {
                                    play(o, &mut card, &speaker, &haptics);
                                }
                            }
                            Ok(_) => {}
                            Err(smpsc::RecvTimeoutError::Timeout) => break,
                            Err(smpsc::RecvTimeoutError::Disconnected) => return,
                        }
                    }
                    // Haptics stopped: stop the motors that imitated them.
                    for o in slots.iter_mut().flatten() {
                        if o.sound.rumbling && o.sound.last_haptics.elapsed() > Duration::from_millis(60) {
                            o.sound.rumbling = false;
                            let _ = o.to_device.send(ds5::rumble_output(0, 0));
                        }
                    }
                    // Device threads that ended (unplugged).
                    for s in &mut slots {
                        if s.as_ref().is_some_and(|o| o.done.try_recv().is_ok()) {
                            *s = None;
                        }
                    }
                    if last_scan.elapsed() < Duration::from_secs(1) {
                        continue;
                    }
                    last_scan = Instant::now();
                    if api.refresh_devices().is_err() {
                        continue;
                    }
                    let found: Vec<(CString, bool, bool)> = api
                        .device_list()
                        .filter(|d| super::is_dualsense(Some(d.vendor_id()), Some(d.product_id())))
                        .map(|d| {
                            (
                                d.path().to_owned(),
                                matches!(d.bus_type(), BusType::Bluetooth),
                                d.product_id() == ds5::PID_DUALSENSE_EDGE,
                            )
                        })
                        .collect();
                    for (path, bt, edge) in found {
                        if slots.iter().flatten().any(|o| o.path == path) {
                            continue;
                        }
                        let Some(free) = slots.iter().position(Option::is_none) else { break };
                        let Some(dev) = open(&api, &path) else { continue };
                        let slot = u8::try_from(free).unwrap_or(0);
                        let (to_device, from_host) = smpsc::channel();
                        let (done_tx, done) = smpsc::channel();
                        let cmds = commands.clone();
                        tracing::info!(slot, bluetooth = bt, edge, "DualSense connected (full pass-through)");
                        RAW_ACTIVE.fetch_add(1, Ordering::Relaxed);
                        let _ = std::thread::Builder::new().name("aa-ds5-dev".into()).spawn(move || {
                            device_loop(&dev, slot, bt, &cmds, &from_host);
                            RAW_ACTIVE.fetch_sub(1, Ordering::Relaxed);
                            tracing::info!(slot, "DualSense disconnected");
                            // Lost packets: say it a few times.
                            for _ in 0..3 {
                                let _ = cmds.blocking_send(ViewerCommand::Pad(PadMsg::Detach { slot }));
                                std::thread::sleep(Duration::from_millis(300));
                            }
                            let _ = done_tx.send(());
                        });
                        slots[free] = Some(Open { path, bt, to_device, done, sound: Sound::new() });
                    }
                }
            })
            .ok()?;
        Some(out_tx)
    }

    fn open(api: &HidApi, path: &CString) -> Option<HidDevice> {
        // macOS: exclusive access stops the Mac itself reacting to the pad
        // (the PS button opening Launchpad, etc.). If something else holds
        // it, share instead.
        match api.open_path(path) {
            Ok(d) => Some(d),
            Err(e) => {
                #[cfg(target_os = "macos")]
                {
                    api.set_open_exclusive(false);
                    let r = api.open_path(path);
                    api.set_open_exclusive(true);
                    if let Ok(d) = r {
                        return Some(d);
                    }
                }
                tracing::warn!("could not open the DualSense directly ({e}); it will work as a basic controller");
                None
            }
        }
    }

    /// Read the three feature reports the PC will ask for. Over Bluetooth
    /// reading 0x05 is also what turns on full input reports.
    fn features(dev: &HidDevice) -> Vec<Vec<u8>> {
        ds5::FEATURES_AT_ATTACH
            .iter()
            .filter_map(|&id| {
                let mut buf = [0u8; 64];
                buf[0] = id;
                let n = dev.get_feature_report(&mut buf).ok()?;
                (n >= 2 && buf[0] == id).then(|| buf[..n].to_vec())
            })
            .collect()
    }

    fn device_loop(
        dev: &HidDevice,
        slot: u8,
        bt: bool,
        cmds: &mpsc::Sender<ViewerCommand>,
        from_host: &smpsc::Receiver<[u8; ds5::USB_OUTPUT_LEN]>,
    ) {
        let send = |m: PadMsg| cmds.blocking_send(ViewerCommand::Pad(m)).is_ok();
        let mut feats = features(dev);
        let mut feats_sent = Instant::now().checked_sub(FEATURE_REPEAT).unwrap_or_else(Instant::now);
        let mut buf = [0u8; 128];
        let mut bt_seq = 0u8;
        let mut short_reports = 0u32;
        loop {
            if feats_sent.elapsed() >= FEATURE_REPEAT {
                feats_sent = Instant::now();
                if feats.len() < ds5::FEATURES_AT_ATTACH.len() {
                    feats = features(dev);
                }
                for f in &feats {
                    if !send(PadMsg::Feature { slot, report: f.clone() }) {
                        return;
                    }
                }
            }
            let n = match dev.read_timeout(&mut buf, 2) {
                Ok(n) => n,
                Err(e) => {
                    tracing::debug!("DualSense read ended: {e}");
                    return;
                }
            };
            let report: Option<[u8; USB_INPUT_LEN]> = match (n, buf[0]) {
                (0, _) => None,
                (n, 0x01) if n >= USB_INPUT_LEN => buf[..USB_INPUT_LEN].try_into().ok(),
                (n, 0x31) if n >= ds5::BT_REPORT_LEN => ds5::bt_input_to_usb(&buf[..n]),
                // Bluetooth reduced mode: ask for calibration again, which
                // switches it to full reports.
                (_, 0x01) => {
                    short_reports += 1;
                    if short_reports % 250 == 1 {
                        feats = features(dev);
                    }
                    None
                }
                _ => None,
            };
            if let Some(r) = report {
                // Newest wins: if the queue is full, drop this one.
                if cmds.try_send(ViewerCommand::Pad(PadMsg::Input { slot, report: r })).is_err() && cmds.is_closed() {
                    return;
                }
            }
            // The game's commands → the controller (only the newest matters).
            let mut latest = None;
            while let Ok(r) = from_host.try_recv() {
                latest = Some(r);
            }
            if let Some(r) = latest {
                let written = if bt {
                    bt_seq = bt_seq.wrapping_add(1);
                    dev.write(&ds5::usb_output_to_bt(&r, bt_seq))
                } else {
                    dev.write(&r)
                };
                if let Err(e) = written {
                    tracing::debug!("DualSense write failed: {e}");
                }
            }
        }
    }
}

/// `--test-ds5`: a pretend DualSense in slot 0 (stick circling, Cross
/// tapped once a second, motion data changing), its feature reports, and a
/// log line for every output report the PC's "game" sends back.
pub fn spawn_test(commands: mpsc::Sender<ViewerCommand>) -> std::sync::mpsc::Sender<PadMsg> {
    let (tx, rx) = std::sync::mpsc::channel::<PadMsg>();
    let _ = std::thread::Builder::new().name("aa-test-ds5-out".into()).spawn(move || {
        let mut n = 0u64;
        let mut audio = 0u64;
        while let Ok(m) = rx.recv() {
            if let PadMsg::Audio { slot, haptics, .. } = &m {
                audio += 1;
                if audio == 1 || audio % 100 == 0 {
                    tracing::info!(slot, bytes = haptics.len(), "DualSense haptics from host #{audio}");
                }
            }
            if let PadMsg::Output { slot, report } = m {
                n += 1;
                if n <= 3 || n % 20 == 0 {
                    tracing::info!(
                        slot,
                        rumble_right = report[3],
                        rumble_left = report[4],
                        right_trigger_mode = report[11],
                        "DualSense output from host #{n}"
                    );
                }
            }
        }
    });
    let _ = std::thread::Builder::new().name("aa-test-ds5".into()).spawn(move || {
        let send = |m| commands.blocking_send(ViewerCommand::Pad(m)).is_ok();
        let mut t = 0u32;
        loop {
            if t % 500 == 0 {
                for id in aa_core::ds5::FEATURES_AT_ATTACH {
                    let mut f = vec![0u8; 41];
                    f[0] = id;
                    if !send(PadMsg::Feature { slot: 0, report: f }) {
                        return;
                    }
                }
            }
            let mut r = aa_core::ds5::neutral_input();
            let a = f64::from(t) * 0.01;
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            {
                r[1] = (128.0 + a.sin() * 120.0) as u8;
                r[2] = (128.0 + a.cos() * 120.0) as u8;
            }
            if t % 250 < 25 {
                r[8] |= 0x20; // Cross
            }
            r[7] = t.to_le_bytes()[0]; // sequence
            r[16..18].copy_from_slice(&(t.to_le_bytes()[0..2])); // "gyro"
            if commands.try_send(ViewerCommand::Pad(PadMsg::Input { slot: 0, report: r })).is_err()
                && commands.is_closed()
            {
                return;
            }
            t = t.wrapping_add(1);
            std::thread::sleep(std::time::Duration::from_millis(4));
        }
    });
    tx
}
