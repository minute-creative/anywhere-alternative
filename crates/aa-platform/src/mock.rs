//! Hardware-free implementations of every trait.
//!
//! * [`MockCapture`] draws a moving test pattern at a fixed rate.
//! * [`MockEncoder`] / [`MockDecoder`] use a trivial "codec": the raw BGRA
//!   bytes pass through untouched, tagged with a 4-byte magic. This is not
//!   compression; it exists so the *pipeline* (capture → slice → UDP →
//!   reassemble → present) can be tested end to end on any machine.
//! * [`MockInput`] logs events instead of injecting them.
//!
//! The mock codec is huge (1080p ≈ 8 MB per frame) and only sane on LAN at
//! low resolution. That is fine: it is a test rig, not a product.

use std::time::{Duration, Instant};

use aa_core::capability::Capabilities;
use aa_core::input::{GamepadState, InputEvent, Rumble};
use aa_core::video::{Codec, ColorRange, EncodedFrameMeta, PixelFormat, Resolution};
use bytes::{Bytes, BytesMut};

use crate::{
    CapturedFrame, DecodedFrame, EncodedFrame, FrameBuffer, HostBackends, InputInjector, PlatformError, Result,
    ScreenCapture, VideoDecoder, VideoEncoder, ViewerBackends, VirtualGamepad,
};

const MAGIC: &[u8; 4] = b"AAMK";

/// Generates a scrolling colour gradient with a frame counter baked into
/// the top-left pixel, so a viewer can verify frame order visually.
#[derive(Debug)]
pub struct MockCapture {
    res: Resolution,
    fps: u16,
    /// Size and rate it was created with; faults change `res`/`fps` for a while.
    base_res: Resolution,
    base_fps: u16,
    started: Instant,
    frame: u32,
    next_due: Instant,
    locked: bool,
}

/// `AA_SIMULATE_CAPTURE_FAULTS=1`: on a 30 s cycle the mock screen does
/// what real Windows screens do to a host, so recovery can be tested:
/// 6-8 s resolution drops to 3/4; 10-11 s capture errors; 14 s the "GPU"
/// is lost once; 18-21 s the "PC is locked"; 24-26 s refresh rate doubles.
fn faults_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("AA_SIMULATE_CAPTURE_FAULTS").is_some())
}

/// Process-wide clock for the fault schedule, so a rebuilt capture carries on
/// where the old one stopped, and the device loss fires once per cycle.
fn fault_clock() -> &'static (Instant, std::sync::atomic::AtomicU64) {
    static C: std::sync::OnceLock<(Instant, std::sync::atomic::AtomicU64)> = std::sync::OnceLock::new();
    C.get_or_init(|| (Instant::now(), std::sync::atomic::AtomicU64::new(u64::MAX)))
}

impl MockCapture {
    pub fn new(res: Resolution, fps: u16) -> Self {
        let now = Instant::now();
        Self { res, fps, base_res: res, base_fps: fps, started: now, frame: 0, next_due: now, locked: false }
    }

    fn frame_interval(&self) -> Duration {
        Duration::from_secs_f64(1.0 / f64::from(self.fps.max(1)))
    }

    /// Apply the fault schedule; `Err` means "fail this call".
    fn faults(&mut self) -> Result<()> {
        if !faults_on() {
            return Ok(());
        }
        let (start, lost_cycle) = fault_clock();
        let t = start.elapsed().as_secs();
        let (cycle, sec) = (t / 30, t % 30);
        self.res = if (6..8).contains(&sec) {
            Resolution::new((self.base_res.width * 3 / 4) & !1, (self.base_res.height * 3 / 4) & !1)
        } else {
            self.base_res
        };
        self.fps = if (24..26).contains(&sec) { self.base_fps * 2 } else { self.base_fps };
        self.locked = (18..21).contains(&sec);
        if sec == 10 {
            return Err(PlatformError::Backend(anyhow::anyhow!("simulated capture error")));
        }
        if sec == 14 && lost_cycle.swap(cycle, std::sync::atomic::Ordering::Relaxed) != cycle {
            return Err(PlatformError::DeviceLost("simulated GPU reset".into()));
        }
        Ok(())
    }
}

impl ScreenCapture for MockCapture {
    fn unavailable_reason(&self) -> Option<&'static str> {
        self.locked.then_some("PC is locked")
    }

    fn next_frame(&mut self, timeout: Duration) -> Result<Option<CapturedFrame>> {
        if let Err(e) = self.faults() {
            std::thread::sleep(Duration::from_millis(50));
            return Err(e);
        }
        if self.locked {
            std::thread::sleep(timeout);
            return Ok(None);
        }
        let now = Instant::now();
        if self.next_due > now {
            let wait = self.next_due - now;
            if wait > timeout {
                std::thread::sleep(timeout);
                return Ok(None);
            }
            std::thread::sleep(wait);
        }
        self.next_due += self.frame_interval();

        let w = self.res.width as usize;
        let h = self.res.height as usize;
        let mut px = BytesMut::with_capacity(w * h * 4);
        let shift = (self.frame % 256) as u8;
        for y in 0..h {
            for x in 0..w {
                px.extend_from_slice(&[
                    (x as u8).wrapping_add(shift), // B
                    (y as u8).wrapping_add(shift), // G
                    shift,                         // R
                    255,                           // A
                ]);
            }
        }
        // Encode the frame counter into the first pixel for visual debugging.
        px[0..4].copy_from_slice(&self.frame.to_le_bytes());

        let frame = CapturedFrame {
            buffer: FrameBuffer::Cpu(px.freeze()),
            format: PixelFormat::Bgra8,
            resolution: self.res,
            capture_ts_us: self.started.elapsed().as_micros() as u64,
        };
        self.frame += 1;
        Ok(Some(frame))
    }

    fn resolution(&self) -> Resolution {
        self.res
    }

    fn refresh_rate_hz(&self) -> u16 {
        self.fps
    }
}

#[derive(Debug, Default)]
pub struct MockEncoder {
    next_id: u32,
}

impl VideoEncoder for MockEncoder {
    fn encode(&mut self, frame: &CapturedFrame, _force_keyframe: bool) -> Result<EncodedFrame> {
        let FrameBuffer::Cpu(px) = &frame.buffer else {
            return Err(PlatformError::Backend(anyhow::anyhow!("mock encoder only accepts CPU frames")));
        };
        let mut data = BytesMut::with_capacity(4 + 8 + px.len());
        data.extend_from_slice(MAGIC);
        data.extend_from_slice(&frame.resolution.width.to_le_bytes());
        data.extend_from_slice(&frame.resolution.height.to_le_bytes());
        data.extend_from_slice(px);
        let id = self.next_id;
        self.next_id += 1;
        Ok(EncodedFrame {
            meta: EncodedFrameMeta {
                frame_id: id,
                capture_ts_us: frame.capture_ts_us,
                is_keyframe: true,
                codec: Codec::H264,
            },
            data: data.freeze(),
        })
    }

    fn set_bitrate_kbps(&mut self, _kbps: u32) -> Result<()> {
        Ok(())
    }

    fn request_intra_refresh(&mut self) -> Result<()> {
        Ok(())
    }
}

#[derive(Debug, Default)]
pub struct MockDecoder;

impl VideoDecoder for MockDecoder {
    fn decode(&mut self, frame_id: u32, data: &Bytes) -> Result<Option<DecodedFrame>> {
        if data.len() < 12 || &data[..4] != MAGIC {
            return Err(PlatformError::Backend(anyhow::anyhow!("not a mock-codec frame")));
        }
        let w = u32::from_le_bytes(data[4..8].try_into().expect("4 bytes"));
        let h = u32::from_le_bytes(data[8..12].try_into().expect("4 bytes"));
        let expected = w as usize * h as usize * 4;
        if data.len() - 12 != expected {
            return Err(PlatformError::Backend(anyhow::anyhow!("mock frame size mismatch")));
        }
        Ok(Some(DecodedFrame {
            buffer: FrameBuffer::Cpu(data.slice(12..)),
            format: PixelFormat::Bgra8,
            resolution: Resolution::new(w, h),
            frame_id,
        }))
    }
}

#[derive(Debug, Default)]
pub struct MockInput {
    /// Most recent event (keeping all of them would grow without bound in
    /// long test runs).
    pub last: Option<InputEvent>,
    pub count: u64,
}

impl InputInjector for MockInput {
    fn inject(&mut self, event: InputEvent) -> Result<()> {
        self.count += 1;
        if self.count % 50 == 1 {
            tracing::info!(count = self.count, ?event, "mock input");
        }
        self.last = Some(event);
        Ok(())
    }
}

#[derive(Debug, Default)]
pub struct MockGamepad {
    pub last: Option<(u8, GamepadState)>,
    updates: u64,
    plugged: [Option<aa_core::input::GamepadKind>; 8],
    /// The pretend game rumbles every pad once a second.
    last_rumble: Option<Instant>,
}

impl VirtualGamepad for MockGamepad {
    fn attach(&mut self, slot: u8, kind: aa_core::input::GamepadKind) -> Result<()> {
        if let Some(p) = self.plugged.get_mut(usize::from(slot)) {
            if *p != Some(kind) {
                *p = Some(kind);
                tracing::info!(slot, ?kind, "mock controller plugged in");
            }
        }
        Ok(())
    }

    fn detach(&mut self, slot: u8) -> Result<()> {
        if let Some(p) = self.plugged.get_mut(usize::from(slot)) {
            if p.take().is_some() {
                tracing::info!(slot, "mock controller unplugged");
            }
        }
        Ok(())
    }

    fn update(&mut self, slot: u8, state: GamepadState) -> Result<()> {
        self.updates += 1;
        if self.updates % 250 == 1 {
            let x = crate::padmap::to_xinput(&state);
            tracing::info!(
                slot,
                updates = self.updates,
                buttons = format_args!("{:#06x}", x.buttons),
                lx = x.thumb_lx,
                "mock controller state"
            );
        }
        self.last = Some((slot, state));
        Ok(())
    }

    fn poll_rumble(&mut self) -> Result<Option<Rumble>> {
        let Some(slot) = self.plugged.iter().position(Option::is_some) else { return Ok(None) };
        if self.last_rumble.is_some_and(|t| t.elapsed() < Duration::from_secs(1)) {
            return Ok(None);
        }
        self.last_rumble = Some(Instant::now());
        Ok(Some(Rumble { slot: u8::try_from(slot).unwrap_or(0), low_freq: 120, high_freq: 40 }))
    }
}

fn mock_capabilities(res: Resolution, fps: u16) -> Capabilities {
    Capabilities {
        codecs: vec![Codec::H264],
        max_resolution: res,
        max_fps: fps,
        color_ranges: vec![ColorRange::Sdr],
        has_gamepad: true,
        can_emulate_gamepad: true,
    }
}

/// The mock clipboard of this process. `--test-clipboard` copies into it on
/// a timer, and the clipboard worker logs what arrives from the other side.
pub fn test_clipboard() -> &'static crate::clipboard::MemoryClipboard {
    static CB: std::sync::OnceLock<crate::clipboard::MemoryClipboard> = std::sync::OnceLock::new();
    CB.get_or_init(Default::default)
}

/// Stands in for the PC's USB/IP driver in mock runs: connects to the
/// virtual `DualSense` the way usbip-win2 does, reads what Windows would
/// (descriptors, feature reports, a stream of input reports), and plays a
/// "game" that sends rumble and an adaptive-trigger effect back once a
/// second. Its log lines are what the smoke test checks.
pub fn loopback_attach() -> crate::padhub::Attach {
    std::sync::Arc::new(|addr, busid| {
        // AA_SIMULATE_NO_USBIP=1: the PC lacks the driver (fallback test).
        if std::env::var_os("AA_SIMULATE_NO_USBIP").is_some() {
            anyhow::bail!("simulated: usbip-win2 is not installed");
        }
        let busid = busid.to_owned();
        std::thread::Builder::new().name("aa-mock-usbip".into()).spawn(move || {
            if let Err(e) = mock_game(addr, &busid) {
                tracing::info!("mock game: virtual DualSense gone ({e})");
            }
        })?;
        Ok(())
    })
}

fn mock_game(addr: std::net::SocketAddr, busid: &str) -> std::io::Result<()> {
    use crate::usbip::client::Client;
    let (mut c, _) = Client::import(addr, busid)?;
    c.s.set_read_timeout(Some(Duration::from_secs(15)))?;
    let (_, dd) = c.get_descriptor(1, 0, 0, 0, 18)?;
    let (_, rd) = c.get_descriptor(0x22, 0, 1, 3, 1024)?;
    let mut feature_bytes = 0;
    for id in aa_core::ds5::FEATURES_AT_ATTACH {
        c.submit(true, 0, 64, [0xA1, 1, id, 3, 3, 0, 64, 0], &[])?;
        feature_bytes += c.reply_in()?.2.len();
    }
    tracing::info!(
        vid = format!("{:02x}{:02x}", dd[9], dd[8]),
        pid = format!("{:02x}{:02x}", dd[11], dd[10]),
        report_descriptor = rd.len(),
        feature_bytes,
        "mock game: virtual DualSense enumerated"
    );
    let (mut n, mut changed, mut last) = (0u64, 0u64, Vec::new());
    let started = Instant::now();
    let mut next_effect = Instant::now() + Duration::from_secs(1);
    loop {
        c.submit(true, 0x84, 64, [0; 8], &[])?;
        let (_, status, data) = c.reply_in()?;
        if status != 0 {
            return Err(std::io::Error::other(format!("transfer status {status}")));
        }
        n += 1;
        if data != last {
            changed += 1;
            last = data;
        }
        if Instant::now() >= next_effect {
            next_effect += Duration::from_secs(1);
            let mut out = [0u8; aa_core::ds5::USB_OUTPUT_LEN];
            out[0] = 0x02;
            out[1] = 0x0F; // rumble + right trigger effect
            out[3] = if n % 2 == 0 { 200 } else { 60 };
            out[11] = 0x26; // trigger effect mode
            c.submit(false, 0x03, 48, [0; 8], &out)?;
            let _ = c.reply()?;
            // …and 50 ms of haptics, as 4-channel USB audio (a 160 Hz thud
            // on both actuators).
            let mut pcm = Vec::with_capacity(3840);
            for i in 0..480u32 {
                let v = if (i / 150) % 2 == 0 { 9000i16 } else { -9000 };
                for v in [0, 0, v, v] {
                    pcm.extend_from_slice(&i16::to_le_bytes(v));
                }
            }
            for _ in 0..5 {
                c.submit_iso(false, 0x01, 384, 10, &pcm)?;
            }
            for _ in 0..5 {
                c.reply_iso(false)?;
            }
            #[allow(clippy::cast_precision_loss)]
            let rate = n as f64 / started.elapsed().as_secs_f64();
            tracing::info!(
                reports = n,
                changed,
                rate = format!("{rate:.0}/s"),
                "mock game: DualSense input, sent rumble + trigger effect"
            );
        }
    }
}

/// Mock host. `raw = true` uses the passthrough codec (huge, lossless, for
/// pipeline debugging); otherwise the software H.264 encoder, which is what
/// you want for anything resembling a real test.
pub fn host_backends(res: Resolution, fps: u16, raw: bool) -> crate::Result<HostBackends> {
    let encoder: Box<dyn VideoEncoder> = if raw {
        Box::new(MockEncoder::default())
    } else {
        let kbps = aa_core::config::StreamConfig::suggested_bitrate_kbps(res, fps);
        Box::new(crate::sw::SwEncoder::new(res, fps, kbps)?)
    };
    let factory = move || -> crate::EncoderFactory {
        Box::new(move |_codec, res, fps| -> crate::Result<Box<dyn VideoEncoder>> {
            if raw {
                Ok(Box::new(MockEncoder::default()))
            } else {
                let kbps = aa_core::config::StreamConfig::suggested_bitrate_kbps(res, fps);
                Ok(Box::new(crate::sw::SwEncoder::new(res, fps, kbps)?))
            }
        })
    };
    Ok(HostBackends {
        capture: Box::new(MockCapture::new(res, fps)),
        encoder,
        input: Box::new(MockInput::default()),
        gamepad: Some(Box::new(MockGamepad::default())),
        encoder_factory: Some(factory()),
        rebuild: Some(Box::new(move || {
            Ok((Box::new(MockCapture::new(res, fps)) as Box<dyn ScreenCapture>, factory()))
        })),
        audio: None,
        speaker: None,
        clipboard: Some(Box::new(test_clipboard().clone())),
        pad_attach: Some(loopback_attach()),
        capabilities: mock_capabilities(res, fps),
    })
}

pub fn viewer_backends(raw: bool) -> crate::Result<ViewerBackends> {
    let decoder: crate::DecoderFactory = Box::new(move |_codec| -> crate::Result<Box<dyn VideoDecoder>> {
        if raw {
            Ok(Box::new(MockDecoder))
        } else {
            Ok(Box::new(crate::sw::SwDecoder::new()?))
        }
    });
    Ok(ViewerBackends {
        decoder,
        clipboard: Some(Box::new(test_clipboard().clone())),
        capabilities: mock_capabilities(Resolution::new(7680, 4320), 240),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_encode_decode_round_trip() {
        let res = Resolution::new(64, 32);
        let mut cap = MockCapture::new(res, 1000);
        let mut enc = MockEncoder::default();
        let mut dec = MockDecoder;

        let f0 = cap.next_frame(Duration::from_secs(1)).unwrap().expect("frame");
        let e0 = enc.encode(&f0, true).unwrap();
        assert_eq!(e0.meta.frame_id, 0);
        let d0 = dec.decode(e0.meta.frame_id, &e0.data).unwrap().expect("decoded");
        assert_eq!(d0.resolution, res);
        let (FrameBuffer::Cpu(a), FrameBuffer::Cpu(b)) = (&f0.buffer, &d0.buffer) else { panic!("cpu") };
        assert_eq!(a, b);

        let f1 = cap.next_frame(Duration::from_secs(1)).unwrap().expect("frame");
        let FrameBuffer::Cpu(px) = &f1.buffer else { panic!("cpu") };
        assert_eq!(u32::from_le_bytes(px[0..4].try_into().unwrap()), 1, "frame counter in first pixel");
    }

    #[test]
    fn capture_respects_frame_rate() {
        let mut cap = MockCapture::new(Resolution::new(8, 8), 100);
        let start = Instant::now();
        for _ in 0..5 {
            cap.next_frame(Duration::from_secs(1)).unwrap();
        }
        // 5 frames at 100 fps: first is immediate, then 4 × 10 ms.
        assert!(start.elapsed() >= Duration::from_millis(38), "{:?}", start.elapsed());
    }

    #[test]
    fn decoder_rejects_garbage() {
        let mut dec = MockDecoder;
        assert!(dec.decode(0, &Bytes::from_static(b"nope")).is_err());
    }
}
