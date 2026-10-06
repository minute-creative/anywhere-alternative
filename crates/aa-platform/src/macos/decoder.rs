//! Hardware video decode on macOS through `VideoToolbox`.
//!
//! Why hardware: Apple Silicon has a dedicated media engine that decodes
//! 4K H.264/HEVC at a few watts while the CPU does nothing. The software
//! path (OpenH264) costs 5–10 ms of CPU per 2880×1800 frame; the media
//! engine does it in about a millisecond and leaves the CPU for input
//! handling and the window.
//!
//! The host sends H.264 as an Annex-B stream (NAL units separated by start
//! codes, parameter sets inline before each keyframe). `VideoToolbox` wants
//! the parameter sets handed over separately as a *format description* and
//! the slices length-prefixed, so each frame goes through a small
//! repackaging step before it is submitted. Decoding is synchronous: the
//! output callback runs before `decode_frame` returns, which keeps the
//! decoder trait simple (one frame in, one frame out).
//!
//! Output is BGRA in CPU memory, matching the window's existing upload
//! path. Zero-copy (`IOSurface` → Metal texture) is the next step once this
//! is proven on real hardware.
//!
//! Bindings come from the `objc2-*` framework crates, which are generated
//! from Apple's SDK headers.

#![allow(unsafe_code, clippy::pedantic)]

use std::ffi::c_void;
use std::ptr::{self, NonNull};

use aa_core::video::{Codec, PixelFormat, Resolution};
use bytes::Bytes;
use objc2_core_foundation::{CFDictionary, CFNumber, CFRetained, CFString};
use objc2_core_media::{
    kCMBlockBufferAssureMemoryNowFlag, CMBlockBuffer, CMFormatDescription, CMSampleBuffer, CMTime,
    CMVideoFormatDescriptionCreateFromH264ParameterSets, CMVideoFormatDescriptionCreateFromHEVCParameterSets,
};
use objc2_core_video::{
    kCVPixelBufferPixelFormatTypeKey, kCVPixelFormatType_32BGRA, CVImageBuffer, CVPixelBufferGetBaseAddress,
    CVPixelBufferGetBytesPerRow, CVPixelBufferGetHeight, CVPixelBufferGetWidth, CVPixelBufferLockBaseAddress,
    CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
};
use objc2_video_toolbox::{
    VTDecodeFrameFlags, VTDecodeInfoFlags, VTDecompressionOutputCallbackRecord, VTDecompressionSession,
};

use crate::{DecodedFrame, FrameBuffer, PlatformError, Result, VideoDecoder};

fn os_err(status: i32, what: &str) -> PlatformError {
    PlatformError::Backend(anyhow::anyhow!("{what}: OSStatus {status}"))
}

// ---------------------------------------------------------------------------
// Annex-B helpers
// ---------------------------------------------------------------------------

/// Split an Annex-B byte stream into NAL units (start codes removed).
fn split_nals(data: &[u8]) -> Vec<&[u8]> {
    let mut bodies = Vec::new(); // (body_start, code_start_of_next)
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 {
            if data[i + 2] == 1 {
                bodies.push((i, i + 3));
                i += 3;
                continue;
            }
            if i + 4 <= data.len() && data[i + 2] == 0 && data[i + 3] == 1 {
                bodies.push((i, i + 4));
                i += 4;
                continue;
            }
        }
        i += 1;
    }
    let mut out = Vec::with_capacity(bodies.len());
    for (n, &(_, body)) in bodies.iter().enumerate() {
        let mut end = bodies.get(n + 1).map_or(data.len(), |&(next_code, _)| next_code);
        // Zero bytes just before a start code are padding, not payload.
        while end > body && data[end - 1] == 0 {
            end -= 1;
        }
        if end > body {
            out.push(&data[body..end]);
        }
    }
    out
}

fn nal_type(codec: Codec, nal: &[u8]) -> u8 {
    match codec {
        Codec::H264 => nal[0] & 0x1f,
        Codec::Hevc => (nal[0] >> 1) & 0x3f,
        _ => 0,
    }
}

/// SPS/PPS (H.264) or VPS/SPS/PPS (HEVC): the stream's "header" NALs.
fn is_parameter_set(codec: Codec, t: u8) -> bool {
    match codec {
        Codec::H264 => t == 7 || t == 8,
        Codec::Hevc => (32..=34).contains(&t),
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Output callback
// ---------------------------------------------------------------------------

/// Filled by the output callback; read back right after `decode_frame`.
struct Output {
    bgra: Vec<u8>,
    width: u32,
    height: u32,
    status: i32,
    got_frame: bool,
}

unsafe extern "C-unwind" fn on_frame(
    refcon: *mut c_void,
    _source_frame_refcon: *mut c_void,
    status: i32,
    _info: VTDecodeInfoFlags,
    image: *mut CVImageBuffer,
    _pts: CMTime,
    _duration: CMTime,
) {
    // SAFETY: refcon is the `Output` owned by the `VtDecoder` that created
    // the session, and the session never outlives it (see Drop order).
    let out = unsafe { &mut *refcon.cast::<Output>() };
    out.status = status;
    out.got_frame = false;
    if status != 0 || image.is_null() {
        return;
    }
    // SAFETY: VideoToolbox hands us a valid pixel buffer for the duration of
    // the callback; we lock it read-only while copying.
    unsafe {
        let pb = &*image;
        if CVPixelBufferLockBaseAddress(pb, CVPixelBufferLockFlags::ReadOnly) != 0 {
            return;
        }
        let w = CVPixelBufferGetWidth(pb);
        let h = CVPixelBufferGetHeight(pb);
        let stride = CVPixelBufferGetBytesPerRow(pb);
        let base = CVPixelBufferGetBaseAddress(pb).cast::<u8>();
        if !base.is_null() {
            let row = w * 4;
            out.bgra.resize(row * h, 0);
            if stride == row {
                ptr::copy_nonoverlapping(base, out.bgra.as_mut_ptr(), row * h);
            } else {
                for y in 0..h {
                    ptr::copy_nonoverlapping(base.add(y * stride), out.bgra.as_mut_ptr().add(y * row), row);
                }
            }
            out.width = w as u32;
            out.height = h as u32;
            out.got_frame = true;
        }
        CVPixelBufferUnlockBaseAddress(pb, CVPixelBufferLockFlags::ReadOnly);
    }
}

// ---------------------------------------------------------------------------
// Decoder
// ---------------------------------------------------------------------------

pub struct VtDecoder {
    codec: Codec,
    /// Parameter sets currently in force, to spot a change (new resolution).
    param_sets: Vec<Vec<u8>>,
    format: Option<CFRetained<CMFormatDescription>>,
    session: Option<CFRetained<VTDecompressionSession>>,
    /// Boxed so its address is stable for the callback's refcon.
    output: Box<Output>,
    /// Scratch for the length-prefixed sample.
    avcc: Vec<u8>,
}

// SAFETY: all VideoToolbox calls happen from the one decode thread that
// owns this; the CF objects are not shared.
unsafe impl Send for VtDecoder {}

impl std::fmt::Debug for VtDecoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VtDecoder").field("codec", &self.codec).field("ready", &self.session.is_some()).finish()
    }
}

impl VtDecoder {
    pub fn new(codec: Codec) -> Result<Self> {
        if !matches!(codec, Codec::H264 | Codec::Hevc) {
            return Err(PlatformError::Unavailable(format!("VideoToolbox: unsupported codec {codec:?}")));
        }
        Ok(Self {
            codec,
            param_sets: Vec::new(),
            format: None,
            session: None,
            output: Box::new(Output { bgra: Vec::new(), width: 0, height: 0, status: 0, got_frame: false }),
            avcc: Vec::new(),
        })
    }

    /// (Re)build the format description and session from parameter sets.
    fn configure(&mut self, sets: Vec<Vec<u8>>) -> Result<()> {
        let ptrs: Vec<NonNull<u8>> =
            sets.iter().map(|s| NonNull::new(s.as_ptr().cast_mut()).expect("non-empty")).collect();
        let sizes: Vec<usize> = sets.iter().map(Vec::len).collect();
        let mut fmt_out: *const CMFormatDescription = ptr::null();

        // SAFETY: the pointer/size arrays are valid for the call; the
        // out-pointer is a local.
        let status = unsafe {
            match self.codec {
                Codec::H264 => CMVideoFormatDescriptionCreateFromH264ParameterSets(
                    None,
                    sets.len(),
                    NonNull::new(ptrs.as_ptr().cast_mut()).expect("ptrs"),
                    NonNull::new(sizes.as_ptr().cast_mut()).expect("sizes"),
                    4,
                    NonNull::from(&mut fmt_out),
                ),
                _ => CMVideoFormatDescriptionCreateFromHEVCParameterSets(
                    None,
                    sets.len(),
                    NonNull::new(ptrs.as_ptr().cast_mut()).expect("ptrs"),
                    NonNull::new(sizes.as_ptr().cast_mut()).expect("sizes"),
                    4,
                    None,
                    NonNull::from(&mut fmt_out),
                ),
            }
        };
        if status != 0 {
            return Err(os_err(status, "CMVideoFormatDescriptionCreate"));
        }
        // SAFETY: a successful Create returns a +1 reference we now own.
        let format = unsafe { CFRetained::from_raw(NonNull::new(fmt_out.cast_mut()).expect("format")) };

        // Ask for BGRA so the window can upload it directly.
        let fmt_num = CFNumber::new_i32(kCVPixelFormatType_32BGRA as i32);
        // SAFETY: the key is a valid static CFString provided by CoreVideo.
        let key: &CFString = unsafe { kCVPixelBufferPixelFormatTypeKey };
        let attrs: CFRetained<CFDictionary<CFString, CFNumber>> = CFDictionary::from_slices(&[key], &[&*fmt_num]);

        let record = VTDecompressionOutputCallbackRecord {
            decompressionOutputCallback: Some(on_frame),
            decompressionOutputRefCon: ptr::addr_of_mut!(*self.output).cast::<c_void>(),
        };
        let mut session_out: *mut VTDecompressionSession = ptr::null_mut();
        // SAFETY: all arguments are valid for the call; `record` outlives it
        // (VideoToolbox copies the struct).
        let status = unsafe {
            VTDecompressionSession::create(
                None,
                &format,
                None,
                Some(attrs.as_opaque()),
                &record,
                NonNull::from(&mut session_out),
            )
        };
        if status != 0 {
            return Err(os_err(status, "VTDecompressionSessionCreate"));
        }
        // SAFETY: +1 reference from a successful Create.
        let session = unsafe { CFRetained::from_raw(NonNull::new(session_out).expect("session")) };

        if let Some(old) = self.session.take() {
            // SAFETY: tearing down a session we own.
            unsafe { old.invalidate() };
        }
        self.session = Some(session);
        self.format = Some(format);
        self.param_sets = sets;
        tracing::info!(codec = ?self.codec, "VideoToolbox decoder configured");
        Ok(())
    }
}

impl VideoDecoder for VtDecoder {
    fn decode(&mut self, frame_id: u32, data: &Bytes) -> Result<Option<DecodedFrame>> {
        let nals = split_nals(data);
        let mut sets: Vec<Vec<u8>> = Vec::new();
        self.avcc.clear();
        for nal in &nals {
            let t = nal_type(self.codec, nal);
            if is_parameter_set(self.codec, t) {
                sets.push(nal.to_vec());
            } else {
                // AVCC: 4-byte big-endian length then the NAL body.
                self.avcc.extend_from_slice(&(nal.len() as u32).to_be_bytes());
                self.avcc.extend_from_slice(nal);
            }
        }
        if !sets.is_empty() && sets != self.param_sets {
            // New stream header (first keyframe, or a resolution change).
            sets.sort_by_key(|s| nal_type(self.codec, s));
            self.configure(sets)?;
        }
        let Some(session) = self.session.as_ref() else {
            // No parameter sets seen yet: nothing to decode against. The
            // viewer's keyframe gate asks the host for one.
            return Ok(None);
        };
        if self.avcc.is_empty() {
            return Ok(None);
        }
        let format = self.format.as_ref().expect("format set with session");

        // Wrap our bytes in a block buffer (copied in: the buffer may outlive
        // this call inside VideoToolbox), then a sample buffer.
        let len = self.avcc.len();
        let mut bb_out: *mut CMBlockBuffer = ptr::null_mut();
        // SAFETY: null memory block + AssureMemoryNow makes CoreMedia
        // allocate `len` bytes; we then copy ours in.
        let status = unsafe {
            CMBlockBuffer::create_with_memory_block(
                None,
                ptr::null_mut(),
                len,
                None,
                ptr::null(),
                0,
                len,
                kCMBlockBufferAssureMemoryNowFlag,
                NonNull::from(&mut bb_out),
            )
        };
        if status != 0 {
            return Err(os_err(status, "CMBlockBufferCreateWithMemoryBlock"));
        }
        // SAFETY: +1 reference from Create.
        let block = unsafe { CFRetained::from_raw(NonNull::new(bb_out).expect("block")) };
        // SAFETY: source is `len` valid bytes; destination has `len` bytes.
        let status = unsafe {
            CMBlockBuffer::replace_data_bytes(
                NonNull::new(self.avcc.as_mut_ptr().cast::<c_void>()).expect("avcc"),
                &block,
                0,
                len,
            )
        };
        if status != 0 {
            return Err(os_err(status, "CMBlockBufferReplaceDataBytes"));
        }

        let mut sb_out: *mut CMSampleBuffer = ptr::null_mut();
        let sample_size = len;
        // SAFETY: valid block/format; one sample of `len` bytes; no timing
        // (we present frames as they arrive).
        let status = unsafe {
            CMSampleBuffer::create_ready(
                None,
                Some(&block),
                Some(format),
                1,
                0,
                ptr::null(),
                1,
                &sample_size,
                NonNull::from(&mut sb_out),
            )
        };
        if status != 0 {
            return Err(os_err(status, "CMSampleBufferCreateReady"));
        }
        // SAFETY: +1 reference from Create.
        let sample = unsafe { CFRetained::from_raw(NonNull::new(sb_out).expect("sample")) };

        self.output.got_frame = false;
        self.output.status = 0;
        let mut info = VTDecodeInfoFlags(0);
        // No async flag: the callback fires before this returns, filling
        // `self.output`.
        // SAFETY: session and sample are live; refcon points at our Output.
        let status = unsafe { session.decode_frame(&sample, VTDecodeFrameFlags(0), ptr::null_mut(), &mut info) };
        if status != 0 {
            return Err(os_err(status, "VTDecompressionSessionDecodeFrame"));
        }
        if self.output.status != 0 {
            return Err(os_err(self.output.status, "decode callback"));
        }
        if !self.output.got_frame {
            return Ok(None);
        }
        Ok(Some(DecodedFrame {
            buffer: FrameBuffer::Cpu(Bytes::copy_from_slice(&self.output.bgra)),
            format: PixelFormat::Bgra8,
            resolution: Resolution::new(self.output.width, self.output.height),
            frame_id,
        }))
    }
}

impl Drop for VtDecoder {
    fn drop(&mut self) {
        if let Some(s) = self.session.take() {
            // SAFETY: orderly teardown before `output` is freed, so no late
            // callback can touch it.
            unsafe { s.invalidate() };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_annex_b_with_both_start_code_lengths() {
        let data = [0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 9, 9, 0];
        let nals = split_nals(&data);
        assert_eq!(nals, vec![&[0x67, 1, 2][..], &[0x68, 3][..], &[0x65, 9, 9][..]]);
        assert_eq!(nal_type(Codec::H264, nals[0]), 7);
        assert_eq!(nal_type(Codec::H264, nals[1]), 8);
        assert_eq!(nal_type(Codec::H264, nals[2]), 5);
    }
}
