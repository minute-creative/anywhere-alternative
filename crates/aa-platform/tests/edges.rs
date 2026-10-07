//! Edge cases for the device-free parts of aa-platform: odd device formats
//! for the audio path, garbage audio packets, controller extremes, PNG
//! clipboard images that lie about themselves, LAN helpers.

use aa_core::input::{gamepad_buttons as b, GamepadState};
use aa_platform::audio::{Depacketizer, OpusEncoder, FRAME_LEN_I16};
use aa_platform::clipboard::{png_to_rgba, rgba_to_png};
use aa_platform::padmap::{to_ds4, to_xinput};
use aa_platform::playout::{InputFramer, Jitter};
use bytes::{BufMut, Bytes, BytesMut};

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
}

/// Any device shape a driver might report: 0..=8 channels, silly sample
/// rates, buffers that aren't a whole number of frames.
#[test]
fn playout_survives_any_device_shape() {
    let mut r = Rng(1);
    let mut j = Jitter::new(true);
    for _ in 0..20_000 {
        if r.below(3) == 0 {
            let n = r.below(3000) as usize;
            let pcm: Vec<i16> = (0..n).map(|_| r.next() as i16).collect();
            j.push(&pcm);
        }
        let channels = r.below(9) as usize;
        let rate = match r.below(6) {
            0 => 0,
            1 => 1,
            2 => 8_000,
            3 => 44_100,
            4 => 384_000,
            _ => r.below(1_000_000) as u32,
        };
        let mut out = vec![f32::NAN; r.below(5000) as usize];
        j.pull(&mut out, channels, rate);
        assert!(out.iter().all(|s| s.is_finite() && s.abs() <= 1.0), "bad sample for {channels} ch @ {rate} Hz");
    }
}

/// Microphones: any rate and channel count, including nonsense, must not
/// hang, crash or emit wrong-sized frames.
#[test]
fn mic_framer_survives_any_device_shape() {
    let mut r = Rng(2);
    for _ in 0..2_000 {
        let mut f = InputFramer::default();
        let channels = r.below(9) as usize;
        let rate = match r.below(5) {
            0 => 0,
            1 => 1,
            2 => 16_000,
            3 => 192_000,
            _ => r.below(400_000) as u32,
        };
        let input: Vec<f32> = (0..r.below(4096)).map(|_| (r.next() as f32 / u64::MAX as f32) * 4.0 - 2.0).collect();
        let started = std::time::Instant::now();
        let mut frames = 0usize;
        f.push(&input, channels, rate, |fr| {
            assert_eq!(fr.len(), FRAME_LEN_I16);
            frames += 1;
        });
        assert!(started.elapsed().as_millis() < 200, "{channels} ch @ {rate} Hz took too long ({frames} frames)");
    }
}

#[test]
fn depacketizer_survives_garbage_and_wraparound() {
    let mut r = Rng(3);
    let mut d = Depacketizer::new().unwrap();
    let mut enc = OpusEncoder::new(128_000).unwrap();
    let pcm = vec![0i16; FRAME_LEN_I16];
    let mut frame_no: u16 = 65_500; // crosses the wrap
    for i in 0..5_000 {
        let mut b = BytesMut::new();
        b.put_u32(0);
        if i % 7 == 0 {
            // Garbage payload, random frame number.
            b.put_u16(r.next() as u16);
            for _ in 0..r.below(400) {
                b.put_u8(r.next() as u8);
            }
        } else {
            frame_no = frame_no.wrapping_add(if r.below(20) == 0 { 3 } else { 1 });
            b.put_u16(frame_no);
            b.put_slice(enc.encode(&pcm).unwrap());
        }
        let mut played = 0;
        d.handle(b.freeze(), |f| {
            assert_eq!(f.len(), FRAME_LEN_I16);
            played += 1;
        });
        assert!(played <= 6, "one packet produced {played} frames");
    }
    // Truncated headers.
    for n in 0..6 {
        d.handle(Bytes::from(vec![1u8; n]), |_| panic!("played from a truncated packet"));
    }
}

#[test]
fn controller_mapping_is_total() {
    let mut r = Rng(4);
    for _ in 0..100_000 {
        let s = GamepadState {
            buttons: r.next() as u32,
            left_x: r.next() as i16,
            left_y: r.next() as i16,
            right_x: r.next() as i16,
            right_y: r.next() as i16,
            left_trigger: r.next() as u8,
            right_trigger: r.next() as u8,
        };
        let _ = to_xinput(&s);
        let d = to_ds4(&s);
        assert!(d.buttons & 0xF <= 8, "d-pad nibble out of range");
    }
    // The extremes map to the extremes.
    let s =
        GamepadState { left_x: i16::MIN, left_y: i16::MIN, buttons: b::DPAD_UP | b::DPAD_DOWN, ..Default::default() };
    let d = to_ds4(&s);
    assert_eq!((d.thumb_lx, d.thumb_ly), (0, 255));
    assert_eq!(d.buttons & 0xF, 8, "up+down together is no direction");
}

#[test]
fn png_decoder_refuses_lies_and_garbage() {
    let mut r = Rng(5);
    for _ in 0..2_000 {
        let n = r.below(2_000) as usize;
        let junk: Vec<u8> = (0..n).map(|_| r.next() as u8).collect();
        let _ = png_to_rgba(&junk);
    }
    // A real PNG with its body cut off.
    let good = rgba_to_png(64, 64, &vec![200u8; 64 * 64 * 4]).unwrap();
    for cut in [8, 33, good.len() / 2, good.len() - 1] {
        // Missing only the end marker is fine if the pixels are all there;
        // what must never happen is a wrong-sized picture.
        if let Some((w, h, px)) = png_to_rgba(&good[..cut]) {
            assert_eq!((w, h, px.len()), (64, 64, 64 * 64 * 4), "cut at {cut}");
        }
    }
    // Wrong buffer size for the stated dimensions must not panic.
    assert!(rgba_to_png(10, 10, &[0u8; 7]).is_none());
}

/// The virtual-USB server faces whatever connects to localhost: garbage,
/// truncated commands, lies about sizes, sudden disconnects. It must keep
/// serving, and a proper client must still get a working `DualSense` after.
#[test]
fn usbip_server_survives_garbage_and_still_serves() {
    use aa_platform::ds5dev::VirtualDualSense;
    use aa_platform::usbip::{client::Client, Export, Server, UsbDevice};
    use std::io::{Read, Write};
    use std::sync::Arc;

    let server = Server::start("127.0.0.1:0".parse().unwrap()).unwrap();
    let dev: Arc<dyn UsbDevice> =
        Arc::new(VirtualDualSense::new(0, false, Box::new(|_| {})).with_audio(Box::new(|_| {})));
    let export = server.add(Export::new("1-1", 1, dev));
    let mut r = Rng(9);
    for i in 0..300 {
        let Ok(mut s) = std::net::TcpStream::connect(server.addr()) else { continue };
        s.set_read_timeout(Some(std::time::Duration::from_millis(20))).unwrap();
        let mut junk: Vec<u8> = (0..r.below(400)).map(|_| r.next() as u8).collect();
        if i % 3 == 0 {
            // A real import first, then garbage URBs (some with huge sizes).
            let mut req = vec![0x01, 0x11, 0x80, 0x03, 0, 0, 0, 0];
            let mut b = b"1-1".to_vec();
            b.resize(32, 0);
            req.extend(b);
            req.extend(junk);
            junk = req;
        }
        let _ = s.write_all(&junk);
        let mut sink = [0u8; 4096];
        let _ = s.read(&mut sink);
    }
    // Still alive and serving properly.
    let (mut c, _) = Client::import(server.addr(), "1-1").expect("server still serves");
    let (status, d) = c.get_descriptor(1, 0, 0, 0, 18).unwrap();
    assert_eq!((status, d.len()), (0, 18));
    drop(c);
    std::thread::sleep(std::time::Duration::from_millis(50));
    assert!(!export.attached(), "closed connections are forgotten");
}
