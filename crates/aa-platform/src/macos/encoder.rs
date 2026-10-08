//! Hardware video encoding on macOS through `VideoToolbox`.
//!
//! Apple Silicon has a dedicated media engine: it encodes 4K H.264 or HEVC
//! at well over 120 fps for a few watts, leaving the CPU and GPU to the
//! game. The captured picture is already on the GPU in the encoder's own
//! format (NV12), so it goes in without a copy.
//!
//! Settings that matter for streaming (as opposed to making a video file):
//! - real-time mode and no frame reordering (no B-frames): each frame comes
//!   out as soon as it goes in;
//! - Apple's low-latency rate control, which keeps every frame close to the
//!   target size so a burst never clogs the network;
//! - keyframes only when asked (the viewer asks when it lost something);
//! - BT.709 video-range colour tags, matching what the viewer expects.
//!
//! Output is converted to Annex-B (start codes, parameter sets before each
//! keyframe), the same layout the Windows encoder produces, so viewers
//! don't care which kind of host they talk to.

#![allow(unsafe_code, clippy::pedantic)]

use std::ffi::c_void;
use std::ptr::{self, NonNull};
use std::sync::Mutex;

use aa_core::video::{Codec, EncodedFrameMeta, Resolution};
use bytes::Bytes;
use objc2_core_foundation::{CFBoolean, CFDictionary, CFNumber, CFRetained, CFString, CFType};
use objc2_core_media::{
    kCMTimeInvalid, kCMVideoCodecType_H264, kCMVideoCodecType_HEVC, CMFormatDescription, CMSampleBuffer, CMTime,
    CMVideoFormatDescriptionGetH264ParameterSetAtIndex, CMVideoFormatDescriptionGetHEVCParameterSetAtIndex,
};
use objc2_core_video::{
    kCVImageBufferColorPrimaries_ITU_R_709_2, kCVImageBufferTransferFunction_ITU_R_709_2,
    kCVImageBufferYCbCrMatrix_ITU_R_709_2, CVPixelBuffer,
};
use objc2_video_toolbox::{
    kVTCompressionPropertyKey_AllowFrameReordering, kVTCompressionPropertyKey_AverageBitRate,
    kVTCompressionPropertyKey_ColorPrimaries, kVTCompressionPropertyKey_ExpectedFrameRate,
    kVTCompressionPropertyKey_MaxKeyFrameInterval, kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration,
    kVTCompressionPropertyKey_ProfileLevel, kVTCompressionPropertyKey_RealTime,
    kVTCompressionPropertyKey_TransferFunction, kVTCompressionPropertyKey_YCbCrMatrix,
    kVTEncodeFrameOptionKey_ForceKeyFrame, kVTProfileLevel_H264_High_AutoLevel, kVTProfileLevel_HEVC_Main_AutoLevel,
    kVTVideoEncoderSpecification_EnableLowLatencyRateControl, VTCompressionSession, VTEncodeInfoFlags,
    VTSessionSetProperty,
};

use crate::{CapturedFrame, EncodedFrame, FrameBuffer, GpuApi, PlatformError, Result, VideoEncoder};

fn os_err(status: i32, what: &str) -> PlatformError {
    PlatformError::Backend(anyhow::anyhow!("{what}: OSStatus {status}"))
}

/// Where the output callback leaves the encoded frame.
#[derive(Default)]
struct Slot {
    out: Option<std::result::Result<(Vec<u8>, bool), i32>>,
}

pub struct VtEncoder {
    session: CFRetained<VTCompressionSession>,
    slot: Box<Mutex<Slot>>,
    codec: Codec,
    resolution: Resolution,
    key_next: bool,
}

// SAFETY: the session is only used from the capture thread that owns this
// encoder; VideoToolbox sessions may be used from any thread.
unsafe impl Send for VtEncoder {}

impl std::fmt::Debug for VtEncoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VtEncoder").field("codec", &self.codec).field("resolution", &self.resolution).finish()
    }
}

fn set(session: &VTCompressionSession, key: &CFString, value: &CFType) -> i32 {
    let s: &CFType = session;
    // SAFETY: valid session, key and value.
    unsafe { VTSessionSetProperty(s, key, Some(value)) }
}

/// Is this NAL unit the start of a keyframe?
fn is_key_nal(codec: Codec, nal: &[u8]) -> bool {
    let Some(&h) = nal.first() else { return false };
    match codec {
        Codec::Hevc => (16..=21).contains(&((h >> 1) & 0x3F)), // IRAP pictures
        _ => h & 0x1F == 5,                                    // IDR slice
    }
}

/// The parameter sets (SPS/PPS, plus VPS for HEVC) of a format description.
fn parameter_sets(codec: Codec, fmt: &CMFormatDescription) -> Vec<Vec<u8>> {
    let get = |i: usize, p: *mut *const u8, n: *mut usize, count: *mut usize| -> i32 {
        // SAFETY: pointers are valid or null, as the call allows.
        unsafe {
            match codec {
                Codec::Hevc => CMVideoFormatDescriptionGetHEVCParameterSetAtIndex(fmt, i, p, n, count, ptr::null_mut()),
                _ => CMVideoFormatDescriptionGetH264ParameterSetAtIndex(fmt, i, p, n, count, ptr::null_mut()),
            }
        }
    };
    let mut count = 0usize;
    if get(0, ptr::null_mut(), ptr::null_mut(), &mut count) != 0 {
        return Vec::new();
    }
    (0..count)
        .filter_map(|i| {
            let (mut p, mut n) = (ptr::null::<u8>(), 0usize);
            if get(i, &mut p, &mut n, ptr::null_mut()) != 0 || p.is_null() {
                return None;
            }
            // SAFETY: VideoToolbox returned a pointer to `n` bytes inside `fmt`, which we hold.
            Some(unsafe { std::slice::from_raw_parts(p, n) }.to_vec())
        })
        .collect()
}

/// Length-prefixed NAL units (what VideoToolbox emits) → Annex-B, with the
/// parameter sets put in front of keyframes.
fn to_annex_b(codec: Codec, avcc: &[u8], sets: impl FnOnce() -> Vec<Vec<u8>>) -> (Vec<u8>, bool) {
    let mut nals = Vec::new();
    let mut i = 0;
    while i + 4 <= avcc.len() {
        let len = u32::from_be_bytes([avcc[i], avcc[i + 1], avcc[i + 2], avcc[i + 3]]) as usize;
        i += 4;
        let Some(body) = avcc.get(i..i + len) else { break };
        nals.push(body);
        i += len;
    }
    let key = nals.iter().any(|n| is_key_nal(codec, n));
    let mut out = Vec::with_capacity(avcc.len() + 128);
    if key {
        for s in sets() {
            out.extend_from_slice(&[0, 0, 0, 1]);
            out.extend_from_slice(&s);
        }
    }
    for n in nals {
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(n);
    }
    (out, key)
}

/// Called by VideoToolbox with each finished frame.
unsafe extern "C-unwind" fn on_encoded(
    refcon: *mut c_void,
    codec_ref: *mut c_void,
    status: i32,
    _flags: VTEncodeInfoFlags,
    sample: *mut CMSampleBuffer,
) {
    // SAFETY: refcon is the `Box<Mutex<Slot>>` owned by the encoder, alive
    // for the session's lifetime; codec_ref is the codec number we passed.
    let slot = unsafe { &*(refcon as *const Mutex<Slot>) };
    let codec = Codec::from_u8(codec_ref as usize as u8).unwrap_or(Codec::H264);
    let result = match NonNull::new(sample) {
        _ if status != 0 => Err(status),
        None => Err(-1), // dropped frame
        Some(sample) => {
            // SAFETY: the sample is valid during this callback.
            let sample = unsafe { sample.as_ref() };
            match unsafe { sample.data_buffer() } {
                None => Err(-2),
                Some(block) => {
                    let len = unsafe { block.data_length() };
                    let mut avcc = vec![0u8; len];
                    // SAFETY: copying `len` bytes into a buffer of that size.
                    let st =
                        unsafe { block.copy_data_bytes(0, len, NonNull::new(avcc.as_mut_ptr().cast()).expect("vec")) };
                    if st == 0 {
                        Ok(to_annex_b(codec, &avcc, || {
                            unsafe { sample.format_description() }
                                .map(|f| parameter_sets(codec, &f))
                                .unwrap_or_default()
                        }))
                    } else {
                        Err(st)
                    }
                }
            }
        }
    };
    if let Ok(mut s) = slot.lock() {
        s.out = Some(result);
    }
}

impl VtEncoder {
    pub fn new(codec: Codec, resolution: Resolution, fps: u16, kbps: u32) -> Result<Self> {
        let codec_type = match codec {
            Codec::H264 => kCMVideoCodecType_H264,
            Codec::Hevc => kCMVideoCodecType_HEVC,
            Codec::Av1 => return Err(PlatformError::Unavailable("Macs have no AV1 encoder".into())),
        };
        let slot: Box<Mutex<Slot>> = Box::default();
        // Low-latency rate control first; older macOS (or HEVC on older
        // versions) may refuse it, then plain real-time mode.
        let low_latency = {
            // SAFETY: static CFString from VideoToolbox.
            let key: &CFString = unsafe { kVTVideoEncoderSpecification_EnableLowLatencyRateControl };
            CFDictionary::<CFString, CFBoolean>::from_slices(&[key], &[CFBoolean::new(true)])
        };
        let mut session = None;
        let mut last = 0;
        for spec in [Some(&*low_latency), None] {
            let mut out: *mut VTCompressionSession = ptr::null_mut();
            // SAFETY: all arguments valid; the refcon outlives the session.
            let status = unsafe {
                VTCompressionSession::create(
                    None,
                    resolution.width as i32,
                    resolution.height as i32,
                    codec_type,
                    spec.map(|d| d.as_opaque()),
                    None,
                    None,
                    Some(on_encoded),
                    ptr::addr_of!(*slot).cast_mut().cast::<c_void>(),
                    NonNull::from(&mut out),
                )
            };
            if status == 0 && !out.is_null() {
                // SAFETY: +1 reference from a successful create.
                session = Some((unsafe { CFRetained::from_raw(NonNull::new(out).expect("session")) }, spec.is_some()));
                break;
            }
            last = status;
        }
        let Some((session, low_latency_on)) = session else {
            return Err(os_err(last, "VTCompressionSessionCreate"));
        };

        // SAFETY: statics from VideoToolbox / CoreVideo.
        unsafe {
            let yes: &CFType = CFBoolean::new(true);
            let no: &CFType = CFBoolean::new(false);
            set(&session, kVTCompressionPropertyKey_RealTime, yes);
            set(&session, kVTCompressionPropertyKey_AllowFrameReordering, no);
            let profile = if codec == Codec::Hevc {
                kVTProfileLevel_HEVC_Main_AutoLevel
            } else {
                kVTProfileLevel_H264_High_AutoLevel
            };
            set(&session, kVTCompressionPropertyKey_ProfileLevel, profile);
            // Keyframes only on request (a very long interval otherwise).
            set(&session, kVTCompressionPropertyKey_MaxKeyFrameInterval, &CFNumber::new_i32(i32::MAX));
            set(&session, kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration, &CFNumber::new_f64(3600.0));
            set(&session, kVTCompressionPropertyKey_ExpectedFrameRate, &CFNumber::new_i32(i32::from(fps)));
            set(&session, kVTCompressionPropertyKey_ColorPrimaries, kCVImageBufferColorPrimaries_ITU_R_709_2);
            set(&session, kVTCompressionPropertyKey_TransferFunction, kCVImageBufferTransferFunction_ITU_R_709_2);
            set(&session, kVTCompressionPropertyKey_YCbCrMatrix, kCVImageBufferYCbCrMatrix_ITU_R_709_2);
        }
        let mut enc = Self { session, slot, codec, resolution, key_next: true };
        enc.set_bitrate_kbps(kbps)?;
        tracing::info!(
            ?codec,
            ?resolution,
            fps,
            kbps,
            low_latency = low_latency_on,
            "encoder: VideoToolbox (media engine)"
        );
        Ok(enc)
    }
}

impl Drop for VtEncoder {
    fn drop(&mut self) {
        // SAFETY: tearing down our own session; no callbacks after this.
        unsafe { self.session.invalidate() };
    }
}

impl VideoEncoder for VtEncoder {
    fn encode(&mut self, frame: &CapturedFrame, force_keyframe: bool) -> Result<EncodedFrame> {
        let FrameBuffer::Gpu { api: GpuApi::Metal, handle } = frame.buffer else {
            return Err(PlatformError::Backend(anyhow::anyhow!(
                "VideoToolbox encoder needs a Metal (IOSurface) frame"
            )));
        };
        let pixels = NonNull::new(handle as *mut CVPixelBuffer)
            .ok_or_else(|| PlatformError::Backend(anyhow::anyhow!("null frame")))?;
        // SAFETY: the capture keeps this pixel buffer alive until the next frame.
        let pixels: &CVPixelBuffer = unsafe { pixels.as_ref() };
        let key = force_keyframe || std::mem::take(&mut self.key_next);
        let props = key.then(|| {
            // SAFETY: static key from VideoToolbox.
            let k: &CFString = unsafe { kVTEncodeFrameOptionKey_ForceKeyFrame };
            CFDictionary::<CFString, CFBoolean>::from_slices(&[k], &[CFBoolean::new(true)])
        });
        self.slot.lock().expect("slot").out = None;
        // SAFETY: valid session and pixel buffer; times are plain values.
        let status = unsafe {
            let pts = CMTime::new(frame.capture_ts_us as i64, 1_000_000);
            let st = self.session.encode_frame(
                pixels,
                pts,
                kCMTimeInvalid,
                props.as_ref().map(|d| d.as_opaque()),
                self.codec as u8 as usize as *mut c_void,
                ptr::null_mut(),
            );
            if st == 0 {
                // Real-time, no reordering: this returns once the frame is out.
                self.session.complete_frames(pts)
            } else {
                st
            }
        };
        if status != 0 {
            return Err(os_err(status, "encode"));
        }
        match self.slot.lock().expect("slot").out.take() {
            Some(Ok((data, is_keyframe))) => Ok(EncodedFrame {
                meta: EncodedFrameMeta {
                    frame_id: 0,
                    capture_ts_us: frame.capture_ts_us,
                    is_keyframe,
                    codec: self.codec,
                },
                data: Bytes::from(data),
            }),
            Some(Err(st)) => {
                self.key_next = true;
                Err(os_err(st, "encoded frame"))
            }
            None => {
                self.key_next = true;
                Err(PlatformError::Backend(anyhow::anyhow!("encoder produced nothing")))
            }
        }
    }

    fn set_bitrate_kbps(&mut self, kbps: u32) -> Result<()> {
        let bps = CFNumber::new_i64(i64::from(kbps) * 1000);
        // SAFETY: static key.
        let st = set(&self.session, unsafe { kVTCompressionPropertyKey_AverageBitRate }, &bps);
        if st == 0 {
            Ok(())
        } else {
            Err(os_err(st, "set bitrate"))
        }
    }

    fn request_intra_refresh(&mut self) -> Result<()> {
        // VideoToolbox has no gradual refresh; a keyframe does the job.
        self.key_next = true;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn length_prefixed_becomes_annex_b_with_sets_on_keyframes() {
        let idr = [0x65u8, 1, 2, 3];
        let p = [0x41u8, 9];
        let mut avcc = Vec::new();
        for n in [&idr[..], &p[..]] {
            avcc.extend_from_slice(&(n.len() as u32).to_be_bytes());
            avcc.extend_from_slice(n);
        }
        let (out, key) = to_annex_b(Codec::H264, &avcc, || vec![vec![0x67, 7], vec![0x68, 8]]);
        assert!(key);
        assert_eq!(out, [0, 0, 0, 1, 0x67, 7, 0, 0, 0, 1, 0x68, 8, 0, 0, 0, 1, 0x65, 1, 2, 3, 0, 0, 0, 1, 0x41, 9]);
        let (out, key) = to_annex_b(Codec::H264, &avcc[8..], || panic!("no sets for a P frame"));
        assert!(!key);
        assert_eq!(out, [0, 0, 0, 1, 0x41, 9]);
        // Truncated input: stop, don't panic.
        let (out, _) = to_annex_b(Codec::H264, &[0, 0, 0, 50, 1], Vec::new);
        assert!(out.is_empty());
    }
}
