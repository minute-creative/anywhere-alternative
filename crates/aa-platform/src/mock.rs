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
    started: Instant,
    frame: u32,
    next_due: Instant,
}

impl MockCapture {
    pub fn new(res: Resolution, fps: u16) -> Self {
        let now = Instant::now();
        Self { res, fps, started: now, frame: 0, next_due: now }
    }

    fn frame_interval(&self) -> Duration {
        Duration::from_secs_f64(1.0 / f64::from(self.fps.max(1)))
    }
}

impl ScreenCapture for MockCapture {
    fn next_frame(&mut self, timeout: Duration) -> Result<Option<CapturedFrame>> {
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
    pub events: Vec<InputEvent>,
}

impl InputInjector for MockInput {
    fn inject(&mut self, event: InputEvent) -> Result<()> {
        tracing::debug!(?event, "mock input");
        self.events.push(event);
        Ok(())
    }
}

#[derive(Debug, Default)]
pub struct MockGamepad {
    pub last: Option<(u8, GamepadState)>,
}

impl VirtualGamepad for MockGamepad {
    fn update(&mut self, slot: u8, state: GamepadState) -> Result<()> {
        self.last = Some((slot, state));
        Ok(())
    }

    fn poll_rumble(&mut self) -> Result<Option<Rumble>> {
        Ok(None)
    }
}

fn mock_capabilities(res: Resolution, fps: u16) -> Capabilities {
    Capabilities {
        codecs: vec![Codec::H264],
        max_resolution: res,
        max_fps: fps,
        color_ranges: vec![ColorRange::Sdr],
        has_gamepad: false,
        can_emulate_gamepad: true,
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
    Ok(HostBackends {
        capture: Box::new(MockCapture::new(res, fps)),
        encoder,
        input: Box::new(MockInput::default()),
        gamepad: Some(Box::new(MockGamepad::default())),
        audio: None,
        capabilities: mock_capabilities(res, fps),
    })
}

pub fn viewer_backends(raw: bool) -> crate::Result<ViewerBackends> {
    let decoder: Box<dyn VideoDecoder> =
        if raw { Box::new(MockDecoder) } else { Box::new(crate::sw::SwDecoder::new()?) };
    Ok(ViewerBackends { decoder, capabilities: mock_capabilities(Resolution::new(7680, 4320), 240) })
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
