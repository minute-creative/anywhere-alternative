//! Software H.264 via Cisco's `OpenH264`. Runs on every OS and every GPU-less
//! VM, which makes it two things at once:
//!
//! 1. the universal fallback when no hardware encoder/decoder opens, and
//! 2. the codec the mock host streams with, so the test pipeline carries
//!    real compressed video (~10 Mbps at 1080p instead of 450 Mbps raw).
//!
//! It is *not* the path for 4K or 240 fps: it costs CPU, it only does H.264
//! Baseline-ish, and colour conversion happens on the CPU. Hardware
//! backends replace it transparently through the same traits.

use std::time::Instant;

use aa_core::video::{Codec, EncodedFrameMeta, PixelFormat, Resolution};
use bytes::Bytes;
use openh264::encoder::{
    BitRate, Complexity, Encoder, EncoderConfig, FrameRate, FrameType, IntraFramePeriod, Profile, RateControlMode,
    SpsPpsStrategy, UsageType,
};
use openh264::formats::{BgraSliceU8, RgbaSliceU8, YUVBuffer, YUVSource};
use openh264::OpenH264API;

use crate::{
    CapturedFrame, DecodedFrame, EncodedFrame, FrameBuffer, PlatformError, Result, VideoDecoder, VideoEncoder,
};

/// Keyframe cadence when intra-refresh isn't available (`OpenH264` has no
/// rolling intra-refresh, so a periodic IDR is the fallback for recovery).
const IDR_PERIOD_FRAMES: u32 = 300;

pub struct SwEncoder {
    encoder: Encoder,
    yuv: YUVBuffer,
    res: Resolution,
    fps: u16,
    bitrate_kbps: u32,
    next_id: u32,
}

impl std::fmt::Debug for SwEncoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SwEncoder")
            .field("res", &self.res)
            .field("bitrate_kbps", &self.bitrate_kbps)
            .finish_non_exhaustive()
    }
}

impl SwEncoder {
    pub fn new(res: Resolution, fps: u16, bitrate_kbps: u32) -> Result<Self> {
        let encoder = Self::open(res, fps, bitrate_kbps)?;
        Ok(Self {
            encoder,
            yuv: YUVBuffer::new(res.width as usize, res.height as usize),
            res,
            fps,
            bitrate_kbps,
            next_id: 0,
        })
    }

    fn open(res: Resolution, fps: u16, bitrate_kbps: u32) -> Result<Encoder> {
        let threads = std::thread::available_parallelism().map_or(2, |n| n.get().clamp(1, 8)) as u16;
        let config = EncoderConfig::new()
            .usage_type(UsageType::ScreenContentRealTime)
            .rate_control_mode(RateControlMode::Bitrate)
            .bitrate(BitRate::from_bps(bitrate_kbps.saturating_mul(1000)))
            .max_frame_rate(FrameRate::from_hz(f32::from(fps.max(1))))
            .profile(Profile::High)
            .complexity(Complexity::Low) // latency over compression efficiency
            // OpenH264's bitrate control needs frame skipping allowed; it only
            // skips when badly over budget, which beats a bitrate spike.
            .skip_frames(true)
            .scene_change_detect(true) // required by screen-content mode
            .background_detection(false) // unsupported for screen content
            .intra_frame_period(IntraFramePeriod::from_num_frames(IDR_PERIOD_FRAMES))
            .sps_pps_strategy(SpsPpsStrategy::ConstantId)
            .num_threads(threads);
        let _ = res; // dimensions come from each frame; config is per session
        Encoder::with_api_config(OpenH264API::from_source(), config)
            .map_err(|e| PlatformError::Backend(anyhow::anyhow!("openh264 encoder: {e}")))
    }
}

impl VideoEncoder for SwEncoder {
    fn encode(&mut self, frame: &CapturedFrame, force_keyframe: bool) -> Result<EncodedFrame> {
        let FrameBuffer::Cpu(px) = &frame.buffer else {
            return Err(PlatformError::Backend(anyhow::anyhow!("software encoder needs CPU frames")));
        };
        if frame.resolution != self.res {
            tracing::info!(old = ?self.res, new = ?frame.resolution, "resolution changed, reopening encoder");
            self.res = frame.resolution;
            self.yuv = YUVBuffer::new(self.res.width as usize, self.res.height as usize);
            self.encoder = Self::open(self.res, self.fps, self.bitrate_kbps)?;
        }
        let dims = (self.res.width as usize, self.res.height as usize);
        match frame.format {
            PixelFormat::Bgra8 => self.yuv.read_bgra8(BgraSliceU8::new(px, dims)),
            PixelFormat::Rgba8 => self.yuv.read_rgba8(RgbaSliceU8::new(px, dims)),
            other => return Err(PlatformError::Backend(anyhow::anyhow!("software encoder can't take {other:?}"))),
        }
        if force_keyframe {
            self.encoder.force_intra_frame();
        }
        let bitstream =
            self.encoder.encode(&self.yuv).map_err(|e| PlatformError::Backend(anyhow::anyhow!("encode: {e}")))?;
        let is_keyframe = matches!(bitstream.frame_type(), FrameType::IDR | FrameType::I);
        let mut v = Vec::new();
        bitstream.write_vec(&mut v);
        let data = Bytes::from(v);

        let id = self.next_id;
        self.next_id += 1;
        Ok(EncodedFrame {
            meta: EncodedFrameMeta {
                frame_id: id,
                capture_ts_us: frame.capture_ts_us,
                is_keyframe,
                codec: Codec::H264,
            },
            data,
        })
    }

    fn set_bitrate_kbps(&mut self, kbps: u32) -> Result<()> {
        if kbps != self.bitrate_kbps {
            self.bitrate_kbps = kbps;
            // OpenH264 can change bitrate live, but the Rust wrapper doesn't
            // expose it; reopening costs one keyframe, acceptable at the rate
            // the controller changes it (seconds, not frames).
            self.encoder = Self::open(self.res, self.fps, kbps)?;
        }
        Ok(())
    }

    fn request_intra_refresh(&mut self) -> Result<()> {
        // No rolling refresh in OpenH264: a keyframe is the recovery path.
        self.encoder.force_intra_frame();
        Ok(())
    }
}

pub struct SwDecoder {
    decoder: openh264::decoder::Decoder,
    nv12: Vec<u8>,
}

impl std::fmt::Debug for SwDecoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SwDecoder").finish_non_exhaustive()
    }
}

impl SwDecoder {
    pub fn new() -> Result<Self> {
        let decoder = openh264::decoder::Decoder::new()
            .map_err(|e| PlatformError::Backend(anyhow::anyhow!("openh264 decoder: {e}")))?;
        Ok(Self { decoder, nv12: Vec::new() })
    }
}

impl VideoDecoder for SwDecoder {
    fn decode(&mut self, frame_id: u32, data: &Bytes) -> Result<Option<DecodedFrame>> {
        let Some(yuv) =
            self.decoder.decode(data).map_err(|e| PlatformError::Backend(anyhow::anyhow!("decode: {e}")))?
        else {
            return Ok(None);
        };
        // NV12 out: the window turns YUV into RGB on the graphics card. The
        // old per-pixel RGB conversion here cost more CPU than decoding.
        let (w, h) = yuv.dimensions();
        let (w, h) = (w & !1, h & !1);
        let (ys, us, vs) = yuv.strides();
        i420_to_nv12(&mut self.nv12, (yuv.y(), ys), (yuv.u(), us), (yuv.v(), vs), w, h);
        Ok(Some(DecodedFrame {
            buffer: FrameBuffer::Cpu(Bytes::copy_from_slice(&self.nv12)),
            format: PixelFormat::Nv12,
            resolution: Resolution::new(w as u32, h as u32),
            frame_id,
        }))
    }
}

/// Planar 4:2:0 (separate U and V planes) → NV12 (one interleaved UV
/// plane), tightly packed `w` × `h`. Pure data shuffling, no maths.
#[allow(clippy::many_single_char_names)] // the planes are called y, u, v
pub fn i420_to_nv12(out: &mut Vec<u8>, y: (&[u8], usize), u: (&[u8], usize), v: (&[u8], usize), w: usize, h: usize) {
    out.clear();
    out.reserve(w * h * 3 / 2);
    for r in 0..h {
        out.extend_from_slice(&y.0[r * y.1..r * y.1 + w]);
    }
    for r in 0..h / 2 {
        let (ur, vr) = (&u.0[r * u.1..r * u.1 + w / 2], &v.0[r * v.1..r * v.1 + w / 2]);
        for (a, b) in ur.iter().zip(vr) {
            out.push(*a);
            out.push(*b);
        }
    }
}

/// Encoder throughput on this machine: frames per second it can sustain at
/// `res`, measured on a synthetic moving picture. Used by `aa-host bench`.
pub fn bench_encoder(res: Resolution, frames: u32) -> Result<f64> {
    use crate::mock::MockCapture;
    use crate::ScreenCapture;
    let mut cap = MockCapture::new(res, 10_000);
    let mut enc = SwEncoder::new(res, 60, 20_000)?;
    let start = Instant::now();
    for i in 0..frames {
        let f = cap.next_frame(std::time::Duration::from_secs(1))?.expect("mock always has a frame");
        enc.encode(&f, i == 0)?;
    }
    Ok(f64::from(frames) / start.elapsed().as_secs_f64())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::MockCapture;
    use crate::ScreenCapture;
    use std::time::Duration;

    #[test]
    fn encode_then_decode_round_trips_dimensions() {
        let res = Resolution::new(320, 180);
        let mut cap = MockCapture::new(res, 10_000);
        let mut enc = SwEncoder::new(res, 60, 2_000).unwrap();
        let mut dec = SwDecoder::new().unwrap();

        let mut decoded = 0;
        let mut first_size = 0;
        for i in 0..10 {
            let f = cap.next_frame(Duration::from_secs(1)).unwrap().unwrap();
            let e = enc.encode(&f, i == 0).unwrap();
            if i == 0 {
                assert!(e.meta.is_keyframe, "first frame must be a keyframe");
                first_size = e.data.len();
            }
            assert!(e.data.len() < res.pixels() as usize * 4, "compressed must beat raw");
            if let Some(d) = dec.decode(e.meta.frame_id, &e.data).unwrap() {
                assert_eq!(d.resolution, res);
                assert_eq!(d.format, PixelFormat::Nv12);
                assert_eq!(d.buffer_len(), (res.pixels() * 3 / 2) as usize);
                decoded += 1;
            }
        }
        assert!(decoded >= 8, "decoded {decoded}/10");
        assert!(first_size > 0);
    }

    #[test]
    fn forced_keyframe_is_reported() {
        let res = Resolution::new(64, 64);
        let mut cap = MockCapture::new(res, 10_000);
        let mut enc = SwEncoder::new(res, 60, 1_000).unwrap();
        let f0 = cap.next_frame(Duration::from_secs(1)).unwrap().unwrap();
        enc.encode(&f0, true).unwrap();
        let f1 = cap.next_frame(Duration::from_secs(1)).unwrap().unwrap();
        let p = enc.encode(&f1, false).unwrap();
        assert!(!p.meta.is_keyframe);
        let f2 = cap.next_frame(Duration::from_secs(1)).unwrap().unwrap();
        let k = enc.encode(&f2, true).unwrap();
        assert!(k.meta.is_keyframe);
    }

    #[test]
    fn planar_to_nv12_interleaves_chroma() {
        // 4x2 picture, strides wider than the picture (as decoders pad).
        let y = [1, 2, 3, 4, 99, 99, 5, 6, 7, 8, 99, 99];
        let u = [10, 11, 99];
        let v = [20, 21, 99];
        let mut out = Vec::new();
        i420_to_nv12(&mut out, (&y, 6), (&u, 3), (&v, 3), 4, 2);
        assert_eq!(out, [1, 2, 3, 4, 5, 6, 7, 8, 10, 20, 11, 21]);
    }

    #[test]
    fn bench_runs() {
        let fps = bench_encoder(Resolution::new(160, 90), 20).unwrap();
        assert!(fps > 0.0);
    }
}
