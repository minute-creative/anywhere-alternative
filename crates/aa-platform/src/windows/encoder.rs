//! Hardware H.264/HEVC encoding through Media Foundation's encoder MFTs.
//!
//! Why this and not a vendor SDK first: Windows ships a hardware encoder
//! transform for Intel (QuickSync), NVIDIA (NVENC) and AMD (AMF) behind one
//! API, it needs no extra runtime installed, and it accepts a D3D11 texture
//! as input. So the captured desktop texture goes GPU → encoder with no CPU
//! copy at all, which removes both of the software path's bottlenecks (the
//! 20 MB/frame readback and the CPU encode). Vendor SDKs can be added later
//! for features MF doesn't expose (true intra-refresh, AV1 on some GPUs).
//!
//! Pipeline per frame:
//!
//! ```text
//!  capture texture ──CopyResource──► our BGRA texture ──video processor──►
//!  NV12 (BT.709 video range, convert.rs) ──► IMFSample ──► ProcessInput
//!  ──► drain ProcessOutput ──► Annex-B bitstream
//! ```
//!
//! Hardware MFTs are *asynchronous*: we must unlock them with
//! `MF_TRANSFORM_ASYNC_UNLOCK` and drive them by events (`METransformNeedInput`
//! / `METransformHaveOutput`). We handle that with a simple blocking loop:
//! feed one frame, then pull events until the matching output arrives.

// FFI code: `unsafe` is the point here, each block carries a SAFETY note;
// the pedantic cast/pointer lints add noise, not safety.
#![allow(unsafe_code, clippy::pedantic)]

use std::time::{Duration, Instant};

/// Longest we wait for the hardware encoder to react before declaring it stuck.
const EVENT_TIMEOUT: Duration = Duration::from_millis(250);

use aa_core::video::{Codec, EncodedFrameMeta, PixelFormat, Resolution};
use bytes::Bytes;
use windows::core::{Interface, GUID};
use windows::Win32::Foundation::{VARIANT_FALSE, VARIANT_TRUE};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11DeviceContext, ID3D11Multithread, ID3D11Texture2D, D3D11_BIND_RENDER_TARGET,
    D3D11_BIND_SHADER_RESOURCE, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Media::MediaFoundation::{
    eAVEncCommonRateControlMode_CBR, eAVEncH265VProfile_Main_420_8, CODECAPI_AVEncCommonLowLatency,
    CODECAPI_AVEncCommonMeanBitRate, CODECAPI_AVEncCommonRateControlMode, CODECAPI_AVEncCommonRealTime,
    CODECAPI_AVEncMPVDefaultBPictureCount, CODECAPI_AVEncMPVGOPSize, CODECAPI_AVEncVideoForceKeyFrame,
    CODECAPI_AVLowLatencyMode, ICodecAPI, IMFActivate, IMFDXGIDeviceManager, IMFMediaEventGenerator, IMFSample,
    IMFTransform, METransformHaveOutput, METransformNeedInput, MFCreateDXGIDeviceManager, MFCreateDXGISurfaceBuffer,
    MFCreateMediaType, MFCreateSample, MFMediaType_Video, MFNominalRange_0_255, MFNominalRange_16_235,
    MFSampleExtension_CleanPoint, MFStartup, MFTEnumEx, MFVideoFormat_ARGB32, MFVideoFormat_H264, MFVideoFormat_HEVC,
    MFVideoFormat_NV12, MFVideoInterlace_Progressive, MFVideoPrimaries_BT709, MFVideoTransFunc_709,
    MFVideoTransferMatrix_BT709, MFSTARTUP_NOSOCKET, MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_FLAG_HARDWARE,
    MFT_ENUM_FLAG_SORTANDFILTER, MFT_MESSAGE_COMMAND_FLUSH, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING,
    MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_MESSAGE_SET_D3D_MANAGER, MFT_OUTPUT_DATA_BUFFER,
    MFT_OUTPUT_STREAM_PROVIDES_SAMPLES, MFT_REGISTER_TYPE_INFO, MF_EVENT_FLAG_NO_WAIT, MF_E_NO_EVENTS_AVAILABLE,
    MF_E_TRANSFORM_NEED_MORE_INPUT, MF_E_TRANSFORM_STREAM_CHANGE, MF_LOW_LATENCY, MF_MT_ALL_SAMPLES_INDEPENDENT,
    MF_MT_AVG_BITRATE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE, MF_MT_MPEG2_PROFILE,
    MF_MT_SUBTYPE, MF_MT_TRANSFER_FUNCTION, MF_MT_VIDEO_NOMINAL_RANGE, MF_MT_VIDEO_PRIMARIES, MF_MT_YUV_MATRIX,
    MF_SA_D3D11_AWARE, MF_TRANSFORM_ASYNC_UNLOCK, MF_VERSION,
};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
use windows::Win32::System::Variant::{VARIANT, VT_BOOL, VT_UI4};

use crate::{CapturedFrame, EncodedFrame, FrameBuffer, GpuApi, PlatformError, Result, VideoEncoder};

fn win(e: windows::core::Error, what: &str) -> PlatformError {
    PlatformError::Backend(anyhow::anyhow!("{what}: {e}"))
}

fn variant_u32(v: u32) -> VARIANT {
    let mut var = VARIANT::default();
    // SAFETY: writing the tag and matching union member of a zeroed VARIANT.
    unsafe {
        (*var.Anonymous.Anonymous).vt = VT_UI4;
        (*var.Anonymous.Anonymous).Anonymous.ulVal = v;
    }
    var
}

fn variant_bool(b: bool) -> VARIANT {
    let mut var = VARIANT::default();
    // SAFETY: as above. VARIANT_BOOL true is -1 (all bits set).
    unsafe {
        (*var.Anonymous.Anonymous).vt = VT_BOOL;
        (*var.Anonymous.Anonymous).Anonymous.boolVal = if b { VARIANT_TRUE } else { VARIANT_FALSE };
    }
    var
}

/// Which codec to ask the hardware for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HwCodec {
    H264,
    Hevc,
}

impl HwCodec {
    fn subtype(self) -> GUID {
        match self {
            HwCodec::H264 => MFVideoFormat_H264,
            HwCodec::Hevc => MFVideoFormat_HEVC,
        }
    }
    fn as_codec(self) -> Codec {
        match self {
            HwCodec::H264 => Codec::H264,
            HwCodec::Hevc => Codec::Hevc,
        }
    }
}

pub struct MfEncoder {
    mft: IMFTransform,
    events: IMFMediaEventGenerator,
    codec_api: Option<ICodecAPI>,
    _dxgi_manager: IMFDXGIDeviceManager,
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    /// A texture we own that the encoder reads from; capture copies into it
    /// so the encoder never holds the desktop duplication frame hostage.
    input_tex: ID3D11Texture2D,
    /// BGRA->NV12 with explicit colour maths; `None` = encoder takes BGRA.
    converter: Option<super::convert::NvConverter>,
    input_id: u32,
    output_id: u32,
    output_provides_samples: bool,
    res: Resolution,
    fps: u16,
    codec: HwCodec,
    name: String,
    next_id: u32,
    started: Instant,
    frame_duration_100ns: i64,
}

// SAFETY: all COM objects are used from the capture thread only; the D3D11
// device has multithread protection enabled because the MFT touches it from
// its own worker threads.
unsafe impl Send for MfEncoder {}

impl std::fmt::Debug for MfEncoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MfEncoder")
            .field("name", &self.name)
            .field("codec", &self.codec)
            .field("res", &self.res)
            .finish_non_exhaustive()
    }
}

/// Names of hardware encoders present, best first (for `bench`/diagnostics).
pub fn list_hardware_encoders(codec: HwCodec) -> Vec<String> {
    let mut names = Vec::new();
    // SAFETY: standard MF enumeration; the returned array is CoTaskMem that we free.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let _ = MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET);
        let out = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: codec.subtype() };
        let mut list: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut count = 0u32;
        if MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
            None,
            Some(&out),
            &mut list,
            &mut count,
        )
        .is_ok()
        {
            for i in 0..count as usize {
                if let Some(act) = (*list.add(i)).take() {
                    names.push(friendly_name(&act));
                }
            }
            windows::Win32::System::Com::CoTaskMemFree(Some(list.cast()));
        }
    }
    names
}

fn friendly_name(act: &IMFActivate) -> String {
    use windows::Win32::Media::MediaFoundation::MFT_FRIENDLY_NAME_Attribute;
    // SAFETY: GetAllocatedString allocates with CoTaskMem; we copy then free.
    unsafe {
        let mut p = windows::core::PWSTR::null();
        let mut len = 0u32;
        if act.GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut p, &mut len).is_ok() && !p.is_null() {
            let s = p.to_string().unwrap_or_default();
            windows::Win32::System::Com::CoTaskMemFree(Some(p.as_ptr().cast()));
            s
        } else {
            "unknown encoder".into()
        }
    }
}

impl MfEncoder {
    /// Open the first hardware encoder for `codec` on `device`. The device
    /// must be the one the capture textures live on.
    pub fn new(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        codec: HwCodec,
        res: Resolution,
        fps: u16,
        bitrate_kbps: u32,
    ) -> Result<Self> {
        // SAFETY: a long sequence of COM calls, each checked; pointers are
        // either stack out-params or COM objects kept alive by `self`.
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET).map_err(|e| win(e, "MFStartup"))?;

            // The MFT drives the device from its own threads.
            if let Ok(mt) = device.cast::<ID3D11Multithread>() {
                let _ = mt.SetMultithreadProtected(true);
            }

            // --- find a hardware encoder -----------------------------------
            let out = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: codec.subtype() };
            let mut list: *mut Option<IMFActivate> = std::ptr::null_mut();
            let mut count = 0u32;
            MFTEnumEx(
                MFT_CATEGORY_VIDEO_ENCODER,
                MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
                None,
                Some(&out),
                &mut list,
                &mut count,
            )
            .map_err(|e| win(e, "MFTEnumEx"))?;
            if count == 0 {
                windows::Win32::System::Com::CoTaskMemFree(Some(list.cast()));
                return Err(PlatformError::Unavailable(format!("no hardware {codec:?} encoder on this GPU")));
            }
            let act = (*list).take().ok_or_else(|| PlatformError::Unavailable("empty activate".into()))?;
            for i in 1..count as usize {
                drop((*list.add(i)).take());
            }
            windows::Win32::System::Com::CoTaskMemFree(Some(list.cast()));
            let name = friendly_name(&act);
            let mft: IMFTransform = act.ActivateObject().map_err(|e| win(e, "ActivateObject"))?;
            tracing::info!(%name, ?codec, "hardware encoder");

            // --- async unlock + D3D awareness -------------------------------
            let attrs = mft.GetAttributes().map_err(|e| win(e, "GetAttributes"))?;
            attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1).map_err(|e| win(e, "ASYNC_UNLOCK"))?;
            let d3d_aware = attrs.GetUINT32(&MF_SA_D3D11_AWARE).unwrap_or(0) != 0;
            if !d3d_aware {
                return Err(PlatformError::Unavailable(format!("{name} is not D3D11-aware")));
            }
            let _ = attrs.SetUINT32(&MF_LOW_LATENCY, 1);

            let mut reset_token = 0u32;
            let mut mgr: Option<IMFDXGIDeviceManager> = None;
            MFCreateDXGIDeviceManager(&mut reset_token, &mut mgr).map_err(|e| win(e, "MFCreateDXGIDeviceManager"))?;
            let mgr = mgr.ok_or_else(|| PlatformError::Backend(anyhow::anyhow!("no DXGI manager")))?;
            mgr.ResetDevice(device, reset_token).map_err(|e| win(e, "ResetDevice"))?;
            let mgr_ptr: *mut std::ffi::c_void = mgr.as_raw();
            mft.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, mgr_ptr as usize).map_err(|e| win(e, "SET_D3D_MANAGER"))?;

            // --- stream ids ---------------------------------------------------
            let (mut in_ids, mut out_ids) = ([0u32; 1], [0u32; 1]);
            let (input_id, output_id) = match mft.GetStreamIDs(&mut in_ids, &mut out_ids) {
                Ok(()) => (in_ids[0], out_ids[0]),
                Err(_) => (0, 0), // E_NOTIMPL means "ids are 0..n"
            };

            // --- output type first (encoders require this order) -------------
            let out_type = MFCreateMediaType().map_err(|e| win(e, "MFCreateMediaType"))?;
            out_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).map_err(|e| win(e, "major"))?;
            out_type.SetGUID(&MF_MT_SUBTYPE, &codec.subtype()).map_err(|e| win(e, "subtype"))?;
            out_type.SetUINT32(&MF_MT_AVG_BITRATE, bitrate_kbps.saturating_mul(1000)).map_err(|e| win(e, "bitrate"))?;
            out_type
                .SetUINT64(&MF_MT_FRAME_SIZE, (u64::from(res.width) << 32) | u64::from(res.height))
                .map_err(|e| win(e, "frame size"))?;
            out_type.SetUINT64(&MF_MT_FRAME_RATE, (u64::from(fps) << 32) | 1).map_err(|e| win(e, "frame rate"))?;
            out_type
                .SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
                .map_err(|e| win(e, "interlace"))?;
            let _ = out_type.SetUINT32(&MF_MT_ALL_SAMPLES_INDEPENDENT, 0);
            if codec == HwCodec::Hevc {
                // HEVC encoders want the profile stated up front; Main 4:2:0
                // 8-bit is what every hardware decoder plays.
                let _ = out_type.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH265VProfile_Main_420_8.0 as u32);
            }
            // Output is *video* range (black = 16, white = 235), the standard
            // for H.264/HEVC. Intel's encoder converts our BGRA to video-range
            // YUV internally whatever we say here; claiming full range made
            // the stream header lie, so the Mac decoded black 16 as grey.
            // Saying video range keeps header and pixels in agreement on
            // every decoder.
            let _ = out_type.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32);
            let _ = out_type.SetUINT32(&MF_MT_VIDEO_PRIMARIES, MFVideoPrimaries_BT709.0 as u32);
            let _ = out_type.SetUINT32(&MF_MT_TRANSFER_FUNCTION, MFVideoTransFunc_709.0 as u32);
            let _ = out_type.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32);
            mft.SetOutputType(output_id, &out_type, 0).map_err(|e| win(e, "SetOutputType"))?;

            // --- our input texture ------------------------------------------
            let desc = D3D11_TEXTURE2D_DESC {
                Width: res.width,
                Height: res.height,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32,
                CPUAccessFlags: 0,
                MiscFlags: 0,
            };
            let mut tex = None;
            device.CreateTexture2D(&desc, None, Some(&mut tex)).map_err(|e| win(e, "CreateTexture2D(input)"))?;
            let input_tex = tex.ok_or_else(|| PlatformError::Backend(anyhow::anyhow!("no input texture")))?;

            // Our own BGRA->NV12 conversion with explicit BT.709 video range
            // (see convert.rs). If the GPU can't, fall back to handing the
            // encoder BGRA and letting the driver convert.
            let converter = match super::convert::NvConverter::new(device, context, &input_tex, res, fps) {
                Ok(c) => Some(c),
                Err(e) => {
                    tracing::warn!("GPU colour converter unavailable ({e}); encoder will convert BGRA itself");
                    None
                }
            };

            // --- input type: NV12 from our converter, else BGRA ---

            let mut chosen_input = None;
            let candidates: &[GUID] =
                if converter.is_some() { &[MFVideoFormat_NV12, MFVideoFormat_ARGB32] } else { &[MFVideoFormat_ARGB32] };
            for &sub in candidates {
                let in_type = MFCreateMediaType().map_err(|e| win(e, "MFCreateMediaType"))?;
                in_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).map_err(|e| win(e, "major"))?;
                in_type.SetGUID(&MF_MT_SUBTYPE, &sub).map_err(|e| win(e, "subtype"))?;
                in_type
                    .SetUINT64(&MF_MT_FRAME_SIZE, (u64::from(res.width) << 32) | u64::from(res.height))
                    .map_err(|e| win(e, "frame size"))?;
                in_type.SetUINT64(&MF_MT_FRAME_RATE, (u64::from(fps) << 32) | 1).map_err(|e| win(e, "frame rate"))?;
                in_type
                    .SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
                    .map_err(|e| win(e, "interlace"))?;
                if sub == MFVideoFormat_NV12 {
                    // Our converter's output: BT.709, video range.
                    let _ = in_type.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32);
                    let _ = in_type.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32);
                } else {
                    // Desktop pixels are full-range sRGB.
                    let _ = in_type.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_0_255.0 as u32);
                }
                let _ = in_type.SetUINT32(&MF_MT_VIDEO_PRIMARIES, MFVideoPrimaries_BT709.0 as u32);
                let _ = in_type.SetUINT32(&MF_MT_TRANSFER_FUNCTION, MFVideoTransFunc_709.0 as u32);
                if mft.SetInputType(input_id, &in_type, 0).is_ok() {
                    chosen_input = Some(sub);
                    break;
                }
            }
            let input_fmt = chosen_input
                .ok_or_else(|| PlatformError::Unavailable(format!("{name} accepts neither BGRA nor NV12")))?;
            let converter = if input_fmt == MFVideoFormat_NV12 { converter } else { None };
            tracing::info!(
                encoder = %name,
                input = if converter.is_some() { "NV12 (our BT.709 video-range conversion)" } else { "BGRA (driver converts)" },
                "encoder colour path"
            );

            // --- codec tuning (best effort; not every driver supports every knob)
            let codec_api = mft.cast::<ICodecAPI>().ok();
            if let Some(api) = &codec_api {
                let set = |guid: &GUID, v: VARIANT| {
                    if let Err(e) = api.SetValue(guid, &v) {
                        tracing::debug!("codec api {guid:?}: {e}");
                    }
                };
                set(&CODECAPI_AVLowLatencyMode, variant_bool(true));
                set(&CODECAPI_AVEncCommonLowLatency, variant_bool(true));
                set(&CODECAPI_AVEncCommonRealTime, variant_bool(true));
                set(&CODECAPI_AVEncCommonRateControlMode, variant_u32(eAVEncCommonRateControlMode_CBR.0 as u32));
                set(&CODECAPI_AVEncCommonMeanBitRate, variant_u32(bitrate_kbps.saturating_mul(1000)));
                set(&CODECAPI_AVEncMPVDefaultBPictureCount, variant_u32(0)); // no B-frames: no reordering delay
                set(&CODECAPI_AVEncMPVGOPSize, variant_u32(u32::from(fps) * 10));
                // keyframe every 10 s; NACK forces sooner
            }

            let out_info = mft.GetOutputStreamInfo(output_id).map_err(|e| win(e, "GetOutputStreamInfo"))?;
            let output_provides_samples = out_info.dwFlags & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32) != 0;

            mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0).map_err(|e| win(e, "BEGIN_STREAMING"))?;
            mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0).map_err(|e| win(e, "START_OF_STREAM"))?;
            let events: IMFMediaEventGenerator = mft.cast().map_err(|e| win(e, "IMFMediaEventGenerator"))?;

            Ok(Self {
                mft,
                events,
                codec_api,
                _dxgi_manager: mgr,
                device: device.clone(),
                context: context.clone(),
                input_tex,
                converter,
                input_id,
                output_id,
                output_provides_samples,
                res,
                fps,
                codec,
                name,
                next_id: 0,
                started: Instant::now(),
                frame_duration_100ns: 10_000_000 / i64::from(fps.max(1)),
            })
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// `GetEvent` with a deadline. Hardware MFTs occasionally stop emitting
    /// events (driver resets, a keyframe request racing a frame in flight);
    /// blocking forever there would freeze the whole host, so we poll with
    /// short sleeps and give up after `timeout`.
    unsafe fn next_event_type(&self, timeout: Duration) -> Result<u32> {
        let deadline = Instant::now() + timeout;
        loop {
            // SAFETY: COM call on a live object.
            let r = unsafe { self.events.GetEvent(MF_EVENT_FLAG_NO_WAIT) };
            match r {
                Ok(ev) => return unsafe { ev.GetType() }.map_err(|e| win(e, "GetType")),
                Err(e) if e.code() == MF_E_NO_EVENTS_AVAILABLE => {
                    if Instant::now() >= deadline {
                        return Err(PlatformError::Backend(anyhow::anyhow!(
                            "encoder produced no event within {timeout:?}"
                        )));
                    }
                    std::thread::sleep(Duration::from_micros(200));
                }
                Err(e) => return Err(win(e, "GetEvent")),
            }
        }
    }

    /// Block until the MFT asks for input (async MFTs require this).
    unsafe fn wait_need_input(&self) -> Result<()> {
        // SAFETY: COM calls on live objects owned by self.
        unsafe {
            loop {
                let t = self.next_event_type(EVENT_TIMEOUT)?;
                if t == METransformNeedInput.0 as u32 {
                    return Ok(());
                }
                if t == METransformHaveOutput.0 as u32 {
                    // Output we didn't expect yet; the caller's drain picks it up
                    // because HaveOutput stays pending until ProcessOutput.
                    return Ok(());
                }
            }
        }
    }

    /// Pull one encoded sample. Blocks on the HaveOutput event.
    unsafe fn pull_output(&self) -> Result<Option<(Bytes, bool)>> {
        // SAFETY: COM calls on live objects; ManuallyDrop::take on fields initialised above; Lock's pointer is valid until Unlock.
        unsafe {
            loop {
                let t = self.next_event_type(EVENT_TIMEOUT)?;
                if t == METransformNeedInput.0 as u32 {
                    continue; // it wants more before it gives output; fine, caller feeds next frame
                }
                if t != METransformHaveOutput.0 as u32 {
                    continue;
                }
                let sample = if self.output_provides_samples {
                    None
                } else {
                    Some(MFCreateSample().map_err(|e| win(e, "MFCreateSample"))?)
                };
                let mut out = [MFT_OUTPUT_DATA_BUFFER {
                    dwStreamID: self.output_id,
                    pSample: std::mem::ManuallyDrop::new(sample),
                    dwStatus: 0,
                    pEvents: std::mem::ManuallyDrop::new(None),
                }];
                let mut status = 0u32;
                match self.mft.ProcessOutput(0, &mut out, &mut status) {
                    Ok(()) => {}
                    Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => {
                        drop(std::mem::ManuallyDrop::take(&mut out[0].pSample));
                        drop(std::mem::ManuallyDrop::take(&mut out[0].pEvents));
                        return Ok(None);
                    }
                    Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                        // The encoder finalised its output format (it now knows
                        // the SPS/PPS it will emit). Re-select the type it
                        // proposes and try again; the frame is still queued.
                        drop(std::mem::ManuallyDrop::take(&mut out[0].pSample));
                        drop(std::mem::ManuallyDrop::take(&mut out[0].pEvents));
                        let proposed = self
                            .mft
                            .GetOutputAvailableType(self.output_id, 0)
                            .map_err(|e| win(e, "GetOutputAvailableType"))?;
                        self.mft
                            .SetOutputType(self.output_id, &proposed, 0)
                            .map_err(|e| win(e, "SetOutputType(renegotiate)"))?;
                        tracing::debug!("encoder output type renegotiated");
                        // The HaveOutput event was consumed; the MFT re-queues
                        // one after renegotiation, so loop back to GetEvent.
                        continue;
                    }
                    Err(e) => {
                        drop(std::mem::ManuallyDrop::take(&mut out[0].pSample));
                        drop(std::mem::ManuallyDrop::take(&mut out[0].pEvents));
                        return Err(win(e, "ProcessOutput"));
                    }
                }
                let sample: Option<IMFSample> = std::mem::ManuallyDrop::take(&mut out[0].pSample);
                drop(std::mem::ManuallyDrop::take(&mut out[0].pEvents));
                let Some(sample) = sample else { return Ok(None) };
                let keyframe = sample.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) != 0;
                let buf = sample.ConvertToContiguousBuffer().map_err(|e| win(e, "ConvertToContiguousBuffer"))?;
                let mut ptr: *mut u8 = std::ptr::null_mut();
                let mut len = 0u32;
                buf.Lock(&mut ptr, None, Some(&mut len)).map_err(|e| win(e, "Lock"))?;
                let data = Bytes::copy_from_slice(std::slice::from_raw_parts(ptr, len as usize));
                buf.Unlock().map_err(|e| win(e, "Unlock"))?;
                return Ok(Some((data, keyframe)));
            }
        }
    }
}

impl VideoEncoder for MfEncoder {
    fn encode(&mut self, frame: &CapturedFrame, force_keyframe: bool) -> Result<EncodedFrame> {
        if frame.resolution != self.res {
            return Err(PlatformError::Backend(anyhow::anyhow!("resolution changed; encoder must be reopened")));
        }
        if frame.format != PixelFormat::Bgra8 {
            return Err(PlatformError::Backend(anyhow::anyhow!("MF encoder path expects BGRA capture")));
        }
        // SAFETY: COM calls on objects we own; the GPU handle is an
        // ID3D11Texture2D on *our* device by contract of the Windows capture.
        unsafe {
            match &frame.buffer {
                FrameBuffer::Gpu { api: GpuApi::D3D11, handle } => {
                    let raw: *mut std::ffi::c_void = *handle as *mut std::ffi::c_void;
                    let src = ID3D11Texture2D::from_raw_borrowed(&raw)
                        .ok_or_else(|| PlatformError::Backend(anyhow::anyhow!("null texture handle")))?;
                    self.context.CopyResource(&self.input_tex, src);
                }
                FrameBuffer::Cpu(_) => {
                    return Err(PlatformError::Backend(anyhow::anyhow!(
                        "MF encoder needs a GPU frame; use the software encoder for CPU frames"
                    )));
                }
                FrameBuffer::Gpu { api, .. } => {
                    return Err(PlatformError::Backend(anyhow::anyhow!("unsupported GPU api {api:?}")));
                }
            }

            if force_keyframe {
                if let Some(api) = &self.codec_api {
                    let _ = api.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &variant_u32(1));
                }
            }

            let encoder_input = match &self.converter {
                Some(c) => {
                    c.convert()?;
                    c.output()
                }
                None => &self.input_tex,
            };
            let buffer = MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, encoder_input, 0, false)
                .map_err(|e| win(e, "MFCreateDXGISurfaceBuffer"))?;
            let sample = MFCreateSample().map_err(|e| win(e, "MFCreateSample"))?;
            sample.AddBuffer(&buffer).map_err(|e| win(e, "AddBuffer"))?;
            let ts = (self.started.elapsed().as_nanos() / 100) as i64;
            sample.SetSampleTime(ts).map_err(|e| win(e, "SetSampleTime"))?;
            sample.SetSampleDuration(self.frame_duration_100ns).map_err(|e| win(e, "SetSampleDuration"))?;

            self.wait_need_input()?;
            self.mft.ProcessInput(self.input_id, &sample, 0).map_err(|e| win(e, "ProcessInput"))?;

            // Low-latency encoders return the frame right away; if this one
            // holds a frame of lookahead we get the previous frame's data,
            // which is still correct ordering for the wire.
            let (data, keyframe) = match self.pull_output()? {
                Some(x) => x,
                None => (Bytes::new(), false),
            };

            let id = self.next_id;
            self.next_id += 1;
            Ok(EncodedFrame {
                meta: EncodedFrameMeta {
                    frame_id: id,
                    capture_ts_us: frame.capture_ts_us,
                    is_keyframe: keyframe,
                    codec: self.codec.as_codec(),
                },
                data,
            })
        }
    }

    fn set_bitrate_kbps(&mut self, kbps: u32) -> Result<()> {
        if let Some(api) = &self.codec_api {
            // SAFETY: COM call with a valid VARIANT.
            unsafe { api.SetValue(&CODECAPI_AVEncCommonMeanBitRate, &variant_u32(kbps.saturating_mul(1000))) }
                .map_err(|e| win(e, "set bitrate"))?;
        }
        Ok(())
    }

    fn request_intra_refresh(&mut self) -> Result<()> {
        if let Some(api) = &self.codec_api {
            // SAFETY: as above.
            unsafe { api.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &variant_u32(1)) }
                .map_err(|e| win(e, "force keyframe"))?;
        }
        Ok(())
    }
}

impl Drop for MfEncoder {
    fn drop(&mut self) {
        // SAFETY: flushing a live MFT; errors on teardown are not actionable.
        unsafe {
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
        }
        let _ = &self.device;
        let _ = self.fps;
    }
}
