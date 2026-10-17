//! The camera of the computer you sit at, sent to the other computer so a
//! call there (Google Meet, Zoom, Teams) can use it as "Anywhere Camera".
//!
//! The picture is small next to a screen stream: 1280x720 at 30 frames a
//! second, compressed to about 2.5 Mbps with H.264 (`OpenH264`, on the
//! processor: a camera picture is cheap to encode, and H.264 is what every
//! browser can decode). Each OS module only has to hand over NV12 pictures
//! of that size; this file holds what is shared: the encoder, a moving test
//! picture for tests, and the device list.

use std::time::{Duration, Instant};

use openh264::encoder::{
    BitRate, Complexity, Encoder, EncoderConfig, FrameRate, FrameType, IntraFramePeriod, Profile, RateControlMode,
    SpsPpsStrategy, UsageType,
};
use openh264::formats::YUVSlices;
use openh264::OpenH264API;

use crate::{PlatformError, Result};

/// Picture size and rate we ask every camera for (scaled if it differs).
pub const WIDTH: u32 = 1280;
pub const HEIGHT: u32 = 720;
pub const FPS: u16 = 30;
/// About what a video call sends for 720p; sharp for a face, light on data.
pub const BITRATE_KBPS: u32 = 2_500;
/// A full picture every 2 seconds, so a browser that starts watching (or a
/// lost packet) never waits long.
const KEYFRAME_EVERY: u32 = 60;

/// One camera picture: NV12 (a full-size brightness plane, then a
/// half-size plane of interleaved colour pairs), tightly packed.
#[derive(Debug, Clone)]
pub struct CameraFrame {
    pub nv12: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// Where pictures come from: a real camera, or the test picture.
pub trait CameraSource: Send {
    /// The next picture, or `None` if none came within `timeout`.
    fn next_frame(&mut self, timeout: Duration) -> Result<Option<CameraFrame>>;
    /// The camera's name, for the other computer and the settings.
    fn name(&self) -> String;
}

/// Names of the cameras on this computer, for the Settings list.
pub fn list() -> Vec<String> {
    #[cfg(target_os = "macos")]
    {
        crate::macos::camera::list()
    }
    #[cfg(target_os = "windows")]
    {
        crate::windows::camera::list()
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        Vec::new()
    }
}

/// Open the camera whose name contains `prefer` (any camera if none does,
/// or if `prefer` is `None`).
pub fn open(prefer: Option<&str>) -> Result<Box<dyn CameraSource>> {
    #[cfg(target_os = "macos")]
    {
        Ok(Box::new(crate::macos::camera::MacCamera::open(prefer)?))
    }
    #[cfg(target_os = "windows")]
    {
        Ok(Box::new(crate::windows::camera::MfCamera::open(prefer)?))
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = prefer;
        Err(PlatformError::Unavailable("no camera support on this system".into()))
    }
}

/// Pick the device to use from `names`: the first containing `prefer`
/// (ignoring case), else the first one. Shared by both OS modules.
pub fn choose(names: &[String], prefer: Option<&str>) -> Option<usize> {
    if names.is_empty() {
        return None;
    }
    let want = prefer.map(str::to_lowercase).filter(|p| !p.is_empty() && p != "auto");
    Some(want.and_then(|p| names.iter().position(|n| n.to_lowercase().contains(&p))).unwrap_or(0))
}

/// A moving test picture (colour bars sliding sideways, a box going round),
/// for tests and `--test-camera`: real compressed video without a camera.
#[derive(Debug)]
pub struct TestPattern {
    frame: u32,
    next: Instant,
}

impl Default for TestPattern {
    fn default() -> Self {
        Self { frame: 0, next: Instant::now() }
    }
}

impl CameraSource for TestPattern {
    fn next_frame(&mut self, timeout: Duration) -> Result<Option<CameraFrame>> {
        let wait = self.next.saturating_duration_since(Instant::now());
        if wait > timeout {
            std::thread::sleep(timeout);
            return Ok(None);
        }
        std::thread::sleep(wait);
        self.next += Duration::from_micros(1_000_000 / u64::from(FPS));
        let (w, h) = (WIDTH as usize, HEIGHT as usize);
        let mut nv12 = vec![0u8; w * h * 3 / 2];
        let shift = (self.frame as usize * 8) % w;
        // Brightness: eight bars, sliding.
        for y in 0..h {
            for x in 0..w {
                nv12[y * w + x] = (40 + ((x + shift) % w) / (w / 8) * 25) as u8;
            }
        }
        // A bright box circling, so motion is obvious.
        let t = f64::from(self.frame) / 30.0;
        let cx = (w as f64 / 2.0 + t.cos() * 300.0) as usize;
        let cy = (h as f64 / 2.0 + t.sin() * 200.0) as usize;
        for y in cy.saturating_sub(60)..(cy + 60).min(h) {
            for x in cx.saturating_sub(60)..(cx + 60).min(w) {
                nv12[y * w + x] = 235;
            }
        }
        // Colour: a slow hue sweep across the picture.
        let uv = &mut nv12[w * h..];
        for y in 0..h / 2 {
            for x in 0..w / 2 {
                uv[y * w + x * 2] = (64 + (x * 128) / (w / 2)) as u8;
                uv[y * w + x * 2 + 1] = (192 - (y * 128) / (h / 2)) as u8;
            }
        }
        self.frame += 1;
        Ok(Some(CameraFrame { nv12, width: WIDTH, height: HEIGHT }))
    }

    fn name(&self) -> String {
        "Test picture".into()
    }
}

/// H.264 for camera pictures. Baseline profile: the one every browser's
/// decoder takes without being told the details.
pub struct CameraEncoder {
    encoder: Encoder,
    u: Vec<u8>,
    v: Vec<u8>,
}

impl std::fmt::Debug for CameraEncoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CameraEncoder").finish_non_exhaustive()
    }
}

impl CameraEncoder {
    pub fn new() -> Result<Self> {
        let config = EncoderConfig::new()
            .usage_type(UsageType::CameraVideoRealTime)
            .rate_control_mode(RateControlMode::Bitrate)
            .bitrate(BitRate::from_bps(BITRATE_KBPS * 1000))
            .max_frame_rate(FrameRate::from_hz(f32::from(FPS)))
            .profile(Profile::Baseline)
            .complexity(Complexity::Low)
            .skip_frames(true)
            .intra_frame_period(IntraFramePeriod::from_num_frames(KEYFRAME_EVERY))
            .sps_pps_strategy(SpsPpsStrategy::ConstantId)
            .num_threads(2);
        let encoder = Encoder::with_api_config(OpenH264API::from_source(), config)
            .map_err(|e| PlatformError::Backend(anyhow::anyhow!("camera encoder: {e}")))?;
        Ok(Self { encoder, u: Vec::new(), v: Vec::new() })
    }

    /// Compress one picture. Returns the H.264 bytes (Annex B, the start-code
    /// form browsers decode directly) and whether it is a keyframe. Empty
    /// bytes: the encoder skipped this picture to stay within the bitrate.
    pub fn encode(&mut self, f: &CameraFrame, keyframe: bool) -> Result<(Vec<u8>, bool)> {
        let (w, h) = (f.width as usize, f.height as usize);
        if w < 2 || h < 2 || f.nv12.len() < w * h * 3 / 2 {
            return Err(PlatformError::Backend(anyhow::anyhow!("camera picture has the wrong size")));
        }
        // NV12 → separate U and V planes (what OpenH264 takes).
        let (cw, ch) = (w / 2, h / 2);
        self.u.resize(cw * ch, 0);
        self.v.resize(cw * ch, 0);
        let uv = &f.nv12[w * h..];
        for y in 0..ch {
            for x in 0..cw {
                self.u[y * cw + x] = uv[y * w + x * 2];
                self.v[y * cw + x] = uv[y * w + x * 2 + 1];
            }
        }
        let src = YUVSlices::new((&f.nv12[..w * h], &self.u, &self.v), (w, h), (w, cw, cw));
        if keyframe {
            self.encoder.force_intra_frame();
        }
        let out =
            self.encoder.encode(&src).map_err(|e| PlatformError::Backend(anyhow::anyhow!("camera encode: {e}")))?;
        let key = matches!(out.frame_type(), FrameType::IDR | FrameType::I);
        let mut v = Vec::new();
        out.write_vec(&mut v);
        Ok((v, key))
    }
}

/// Scale and convert a packed picture of any size to 1280x720 NV12 by
/// nearest-neighbour sampling. Used when a camera can't scale for us; a
/// camera picture is soft anyway, so the simple method is enough.
pub fn fit_nv12(src: &[u8], sw: usize, sh: usize, stride: usize) -> CameraFrame {
    let (w, h) = (WIDTH as usize, HEIGHT as usize);
    let mut out = vec![0u8; w * h * 3 / 2];
    for y in 0..h {
        let sy = y * sh / h;
        for x in 0..w {
            out[y * w + x] = src[sy * stride + x * sw / w];
        }
    }
    let uv_src = &src[stride * sh..];
    for y in 0..h / 2 {
        let sy = y * (sh / 2) / (h / 2);
        for x in 0..w / 2 {
            let sx = x * (sw / 2) / (w / 2);
            out[w * h + y * w + x * 2] = uv_src[sy * stride + sx * 2];
            out[w * h + y * w + x * 2 + 1] = uv_src[sy * stride + sx * 2 + 1];
        }
    }
    CameraFrame { nv12: out, width: WIDTH, height: HEIGHT }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_picture_encodes_to_small_h264_with_keyframes() {
        let mut cam = TestPattern::default();
        let mut enc = CameraEncoder::new().unwrap();
        let mut total = 0;
        let mut keys = 0;
        for i in 0..90 {
            cam.next = Instant::now(); // don't wait in tests
            let f = cam.next_frame(Duration::from_secs(1)).unwrap().unwrap();
            let (data, key) = enc.encode(&f, i == 0).unwrap();
            if i == 0 {
                assert!(key);
                // Annex B: starts with a start code.
                assert!(data.starts_with(&[0, 0, 0, 1]) || data.starts_with(&[0, 0, 1]));
            }
            keys += usize::from(key);
            total += data.len();
        }
        // 3 seconds at 2.5 Mbps is ~940 KB; allow the rate control slack.
        assert!(total < 2_000_000, "{total} bytes");
        assert!(keys >= 2, "a keyframe every 2 s: {keys}");
    }

    #[test]
    fn choosing_a_camera_by_name() {
        let names = vec!["FaceTime HD Camera".to_owned(), "Logitech C920".to_owned()];
        assert_eq!(choose(&names, Some("c920")), Some(1));
        assert_eq!(choose(&names, Some("auto")), Some(0));
        assert_eq!(choose(&names, Some("gone")), Some(0));
        assert_eq!(choose(&[], None), None);
    }

    #[test]
    fn fitting_keeps_the_size_right() {
        let (sw, sh) = (640, 480);
        let src = vec![128u8; sw * sh * 3 / 2];
        let f = fit_nv12(&src, sw, sh, sw);
        assert_eq!(f.nv12.len(), 1280 * 720 * 3 / 2);
    }
}
