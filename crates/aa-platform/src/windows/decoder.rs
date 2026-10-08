//! Video decoding on a Windows viewer through Media Foundation.
//!
//! Windows ships H.264 (and, with the free "HEVC Video Extensions from
//! Device Manufacturer" / Store add-on, HEVC) decoders that use the
//! graphics card's video engine (DXVA) when given a D3D11 device. That
//! takes 4K decoding off the processor entirely; the software path
//! (OpenH264) costs one CPU core per ~1440p60.
//!
//! Output is NV12 (the codec's own YUV layout) in CPU memory; the window
//! converts it to RGB in a shader, so the processor never does per-pixel
//! colour maths either. Readback from the GPU is one copy per frame.
//!
//! If the GPU path can't be set up, the same Windows decoder runs on the
//! processor (still multi-threaded and much faster than OpenH264); if even
//! that is missing, the caller falls back to OpenH264.

#![allow(unsafe_code, clippy::pedantic)]

use aa_core::video::{Codec, PixelFormat, Resolution};
use bytes::Bytes;
use windows::core::{Interface, GUID};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Multithread, ID3D11Texture2D, D3D11_CPU_ACCESS_READ,
    D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_SDK_VERSION,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC};
use windows::Win32::Media::MediaFoundation::{
    IMF2DBuffer, IMFActivate, IMFDXGIBuffer, IMFDXGIDeviceManager, IMFMediaType, IMFSample, IMFTransform,
    MFCreateDXGIDeviceManager, MFCreateMediaType, MFCreateMemoryBuffer, MFCreateSample, MFMediaType_Video, MFStartup,
    MFTEnumEx, MFVideoFormat_H264, MFVideoFormat_HEVC, MFVideoFormat_NV12, MFSTARTUP_NOSOCKET,
    MFT_CATEGORY_VIDEO_DECODER, MFT_ENUM_FLAG_SORTANDFILTER, MFT_ENUM_FLAG_SYNCMFT, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING,
    MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_MESSAGE_SET_D3D_MANAGER, MFT_OUTPUT_DATA_BUFFER,
    MFT_OUTPUT_STREAM_PROVIDES_SAMPLES, MFT_REGISTER_TYPE_INFO, MF_E_NOTACCEPTING, MF_E_TRANSFORM_NEED_MORE_INPUT,
    MF_E_TRANSFORM_STREAM_CHANGE, MF_LOW_LATENCY, MF_MT_FRAME_SIZE, MF_MT_MAJOR_TYPE, MF_MT_MINIMUM_DISPLAY_APERTURE,
    MF_MT_SUBTYPE, MF_SA_D3D11_AWARE, MF_VERSION,
};
use windows::Win32::System::Com::{CoInitializeEx, CoTaskMemFree, COINIT_MULTITHREADED};

use crate::{DecodedFrame, FrameBuffer, PlatformError, Result, VideoDecoder};

fn win(e: windows::core::Error, what: &str) -> PlatformError {
    PlatformError::Backend(anyhow::anyhow!("{what}: {e}"))
}

/// The GPU side, when hardware decoding is on.
struct Gpu {
    _manager: IMFDXGIDeviceManager,
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    /// CPU-readable copy target, recreated when the size changes.
    staging: Option<(ID3D11Texture2D, u32, u32)>,
}

pub struct MfDecoder {
    mft: IMFTransform,
    gpu: Option<Gpu>,
    codec: Codec,
    provides_samples: bool,
    out_size: u32,
    /// Allocated (coded) size, and the visible picture inside it.
    coded: (u32, u32),
    visible: (u32, u32),
    ts: i64,
    nv12: Vec<u8>,
}

// SAFETY: the MFT and D3D objects are used only from the decode thread
// that owns this decoder; the device is multithread-protected for the
// decoder's own worker threads.
unsafe impl Send for MfDecoder {}

impl std::fmt::Debug for MfDecoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MfDecoder").field("codec", &self.codec).field("gpu", &self.gpu.is_some()).finish()
    }
}

fn subtype(codec: Codec) -> Result<GUID> {
    match codec {
        Codec::H264 => Ok(MFVideoFormat_H264),
        Codec::Hevc => Ok(MFVideoFormat_HEVC),
        Codec::Av1 => Err(PlatformError::Unavailable("no AV1 decoder".into())),
    }
}

/// The first Windows decoder that takes `codec`.
unsafe fn find_decoder(codec: Codec) -> Result<IMFTransform> {
    // SAFETY: standard MF enumeration; the array is CoTaskMem that we free.
    unsafe {
        let input = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: subtype(codec)? };
        let mut list: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut count = 0u32;
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_DECODER,
            MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER,
            Some(&input),
            None,
            &mut list,
            &mut count,
        )
        .map_err(|e| win(e, "MFTEnumEx"))?;
        let mut acts = Vec::new();
        for i in 0..count as usize {
            if let Some(a) = (*list.add(i)).take() {
                acts.push(a);
            }
        }
        CoTaskMemFree(Some(list.cast()));
        let act = acts.into_iter().next().ok_or_else(|| {
            PlatformError::Unavailable(match codec {
                Codec::Hevc => "no HEVC decoder in Windows (install \"HEVC Video Extensions\" from the Microsoft \
                                Store to enable it)"
                    .into(),
                _ => format!("no {codec:?} decoder in Windows"),
            })
        })?;
        act.ActivateObject().map_err(|e| win(e, "ActivateObject"))
    }
}

/// A D3D11 device that can decode video, handed to the decoder.
unsafe fn attach_gpu(mft: &IMFTransform) -> Result<Gpu> {
    // SAFETY: COM calls on live objects; out-params are stack locals.
    unsafe {
        let attrs = mft.GetAttributes().map_err(|e| win(e, "GetAttributes"))?;
        if attrs.GetUINT32(&MF_SA_D3D11_AWARE).unwrap_or(0) == 0 {
            return Err(PlatformError::Unavailable("decoder is not D3D11-aware".into()));
        }
        let (mut device, mut context) = (None, None);
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )
        .map_err(|e| win(e, "D3D11CreateDevice"))?;
        let device: ID3D11Device = device.ok_or_else(|| PlatformError::Unavailable("no D3D11 device".into()))?;
        let context = context.ok_or_else(|| PlatformError::Unavailable("no D3D11 context".into()))?;
        if let Ok(mt) = device.cast::<ID3D11Multithread>() {
            let _ = mt.SetMultithreadProtected(true);
        }
        let mut token = 0u32;
        let mut manager: Option<IMFDXGIDeviceManager> = None;
        MFCreateDXGIDeviceManager(&mut token, &mut manager).map_err(|e| win(e, "MFCreateDXGIDeviceManager"))?;
        let manager = manager.ok_or_else(|| PlatformError::Unavailable("no DXGI manager".into()))?;
        manager.ResetDevice(&device, token).map_err(|e| win(e, "ResetDevice"))?;
        mft.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize)
            .map_err(|e| win(e, "SET_D3D_MANAGER"))?;
        Ok(Gpu { _manager: manager, device, context, staging: None })
    }
}

fn unpack_size(v: u64) -> (u32, u32) {
    ((v >> 32) as u32, v as u32)
}

impl MfDecoder {
    pub fn new(codec: Codec, hardware: bool) -> Result<Self> {
        // SAFETY: a sequence of checked COM calls on objects we keep.
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET).map_err(|e| win(e, "MFStartup"))?;
            let mft = find_decoder(codec)?;
            if let Ok(attrs) = mft.GetAttributes() {
                // Without this the decoder holds frames back for reordering.
                let _ = attrs.SetUINT32(&MF_LOW_LATENCY, 1);
            }
            let gpu = if hardware {
                match attach_gpu(&mft) {
                    Ok(g) => Some(g),
                    Err(e) => {
                        tracing::warn!("graphics-card video decoding unavailable ({e}); decoding on the processor");
                        None
                    }
                }
            } else {
                None
            };
            let in_type: IMFMediaType = MFCreateMediaType().map_err(|e| win(e, "MFCreateMediaType"))?;
            in_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).map_err(|e| win(e, "major"))?;
            in_type.SetGUID(&MF_MT_SUBTYPE, &subtype(codec)?).map_err(|e| win(e, "subtype"))?;
            mft.SetInputType(0, &in_type, 0).map_err(|e| win(e, "SetInputType"))?;
            let mut d = Self {
                mft,
                gpu,
                codec,
                provides_samples: false,
                out_size: 0,
                coded: (0, 0),
                visible: (0, 0),
                ts: 0,
                nv12: Vec::new(),
            };
            d.pick_output()?;
            d.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0).map_err(|e| win(e, "BEGIN_STREAMING"))?;
            d.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0).map_err(|e| win(e, "START_OF_STREAM"))?;
            tracing::info!(?codec, hardware = d.gpu.is_some(), "viewer decoder: Media Foundation");
            Ok(d)
        }
    }

    /// Choose NV12 output (at start and whenever the stream changes size).
    unsafe fn pick_output(&mut self) -> Result<()> {
        // SAFETY: COM calls on our MFT.
        unsafe {
            let mut i = 0;
            loop {
                let t = match self.mft.GetOutputAvailableType(0, i) {
                    Ok(t) => t,
                    Err(_) => return Err(PlatformError::Unavailable("decoder offers no NV12 output".into())),
                };
                if t.GetGUID(&MF_MT_SUBTYPE).ok() == Some(MFVideoFormat_NV12) {
                    self.mft.SetOutputType(0, &t, 0).map_err(|e| win(e, "SetOutputType"))?;
                    self.coded = t.GetUINT64(&MF_MT_FRAME_SIZE).map(unpack_size).unwrap_or((0, 0));
                    // The visible picture (1080 of a 1088-row coded frame).
                    let mut area = [0u8; 16];
                    let mut got = 0u32;
                    self.visible =
                        if t.GetBlob(&MF_MT_MINIMUM_DISPLAY_APERTURE, &mut area, Some(&mut got)).is_ok() && got >= 16 {
                            let cx = i32::from_le_bytes([area[8], area[9], area[10], area[11]]);
                            let cy = i32::from_le_bytes([area[12], area[13], area[14], area[15]]);
                            (cx.max(0) as u32, cy.max(0) as u32)
                        } else {
                            self.coded
                        };
                    if self.visible.0 == 0 || self.visible.1 == 0 {
                        self.visible = self.coded;
                    }
                    let info = self.mft.GetOutputStreamInfo(0).map_err(|e| win(e, "GetOutputStreamInfo"))?;
                    self.provides_samples = info.dwFlags & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32) != 0;
                    self.out_size = info.cbSize;
                    return Ok(());
                }
                i += 1;
            }
        }
    }

    /// Copy the visible NV12 picture out of a decoded sample into `self.nv12`.
    unsafe fn read_sample(&mut self, sample: &IMFSample) -> Result<()> {
        let (w, h) = (self.visible.0 as usize & !1, self.visible.1 as usize & !1);
        self.nv12.resize(w * h * 3 / 2, 0);
        // SAFETY: COM calls; mapped/locked pointers are read only within
        // their lock and within the sizes the API reports.
        unsafe {
            let buf = sample.GetBufferByIndex(0).map_err(|e| win(e, "GetBufferByIndex"))?;
            if let (Some(gpu), Ok(dx)) = (self.gpu.as_mut(), buf.cast::<IMFDXGIBuffer>()) {
                let mut raw: *mut std::ffi::c_void = std::ptr::null_mut();
                dx.GetResource(&ID3D11Texture2D::IID, &mut raw).map_err(|e| win(e, "GetResource"))?;
                if raw.is_null() {
                    return Err(PlatformError::Backend(anyhow::anyhow!("decoder gave no texture")));
                }
                // SAFETY: GetResource returned an owned reference to this interface.
                let tex = ID3D11Texture2D::from_raw(raw);
                let index = dx.GetSubresourceIndex().map_err(|e| win(e, "GetSubresourceIndex"))?;
                let mut desc = D3D11_TEXTURE2D_DESC::default();
                tex.GetDesc(&mut desc);
                if !gpu.staging.as_ref().is_some_and(|(_, sw, sh)| *sw == desc.Width && *sh == desc.Height) {
                    let sd = D3D11_TEXTURE2D_DESC {
                        Width: desc.Width,
                        Height: desc.Height,
                        MipLevels: 1,
                        ArraySize: 1,
                        Format: DXGI_FORMAT_NV12,
                        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                        Usage: D3D11_USAGE_STAGING,
                        BindFlags: 0,
                        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                        MiscFlags: 0,
                    };
                    let mut st = None;
                    gpu.device.CreateTexture2D(&sd, None, Some(&mut st)).map_err(|e| win(e, "staging texture"))?;
                    let st = st.ok_or_else(|| PlatformError::Backend(anyhow::anyhow!("no staging texture")))?;
                    gpu.staging = Some((st, desc.Width, desc.Height));
                }
                let (staging, _, sh) = gpu.staging.as_ref().expect("just made");
                gpu.context.CopySubresourceRegion(staging, 0, 0, 0, 0, &tex, index, None);
                let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
                gpu.context.Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped)).map_err(|e| win(e, "Map"))?;
                let pitch = mapped.RowPitch as usize;
                let base = mapped.pData.cast::<u8>();
                let rows = (h.min(*sh as usize), h / 2);
                copy_nv12(&mut self.nv12, base, pitch, *sh as usize, w, rows);
                gpu.context.Unmap(staging, 0);
                return Ok(());
            }
            // Processor path: a 2-D buffer with its own pitch.
            let b2d: IMF2DBuffer = buf.cast().map_err(|e| win(e, "IMF2DBuffer"))?;
            let mut scan0: *mut u8 = std::ptr::null_mut();
            let mut pitch = 0i32;
            b2d.Lock2D(&mut scan0, &mut pitch).map_err(|e| win(e, "Lock2D"))?;
            let coded_h = (self.coded.1 as usize).max(h);
            copy_nv12(&mut self.nv12, scan0, pitch.unsigned_abs() as usize, coded_h, w, (h, h / 2));
            let _ = b2d.Unlock2D();
            Ok(())
        }
    }
}

/// Copy a `w`-wide NV12 picture from a pitched surface whose chroma plane
/// starts `plane_rows` rows after the luma plane.
unsafe fn copy_nv12(out: &mut [u8], base: *const u8, pitch: usize, plane_rows: usize, w: usize, rows: (usize, usize)) {
    let (y_rows, uv_rows) = rows;
    for r in 0..y_rows {
        // SAFETY: row `r` < plane height lies within the mapped surface.
        let src = unsafe { std::slice::from_raw_parts(base.add(r * pitch), w) };
        out[r * w..(r + 1) * w].copy_from_slice(src);
    }
    let uv_out = y_rows.max(1) * w;
    for r in 0..uv_rows {
        // SAFETY: the chroma plane follows the luma plane in the same mapping.
        let src = unsafe { std::slice::from_raw_parts(base.add((plane_rows + r) * pitch), w) };
        let at = uv_out + r * w;
        if at + w <= out.len() {
            out[at..at + w].copy_from_slice(src);
        }
    }
}

impl VideoDecoder for MfDecoder {
    fn decode(&mut self, frame_id: u32, data: &Bytes) -> Result<Option<DecodedFrame>> {
        // SAFETY: checked COM calls on objects we own; the input buffer is
        // filled within its lock.
        unsafe {
            let len =
                u32::try_from(data.len()).map_err(|_| PlatformError::Backend(anyhow::anyhow!("frame too big")))?;
            let buffer = MFCreateMemoryBuffer(len.max(1)).map_err(|e| win(e, "MFCreateMemoryBuffer"))?;
            let mut p: *mut u8 = std::ptr::null_mut();
            buffer.Lock(&mut p, None, None).map_err(|e| win(e, "Lock"))?;
            std::ptr::copy_nonoverlapping(data.as_ptr(), p, data.len());
            buffer.Unlock().map_err(|e| win(e, "Unlock"))?;
            buffer.SetCurrentLength(len).map_err(|e| win(e, "SetCurrentLength"))?;
            let sample = MFCreateSample().map_err(|e| win(e, "MFCreateSample"))?;
            sample.AddBuffer(&buffer).map_err(|e| win(e, "AddBuffer"))?;
            self.ts += 166_666;
            let _ = sample.SetSampleTime(self.ts);

            let mut got = false;
            loop {
                match self.mft.ProcessInput(0, &sample, 0) {
                    Ok(()) => break,
                    // Full: take what's ready, then feed again.
                    Err(e) if e.code() == MF_E_NOTACCEPTING => {
                        if !self.drain(&mut got)? {
                            return Err(win(e, "ProcessInput"));
                        }
                    }
                    Err(e) => return Err(win(e, "ProcessInput")),
                }
            }
            self.drain(&mut got)?;
            if !got {
                return Ok(None);
            }
            let (w, h) = (self.visible.0 & !1, self.visible.1 & !1);
            Ok(Some(DecodedFrame {
                buffer: FrameBuffer::Cpu(Bytes::copy_from_slice(&self.nv12)),
                format: PixelFormat::Nv12,
                resolution: Resolution::new(w, h),
                frame_id,
            }))
        }
    }
}

impl MfDecoder {
    /// Pull every finished picture; the newest ends up in `self.nv12`.
    /// Returns whether anything came out.
    fn drain(&mut self, got: &mut bool) -> Result<bool> {
        let mut any = false;
        // SAFETY: COM calls; ManuallyDrop fields are taken exactly once.
        unsafe {
            loop {
                let sample = if self.provides_samples {
                    None
                } else {
                    let s = MFCreateSample().map_err(|e| win(e, "MFCreateSample"))?;
                    let b = MFCreateMemoryBuffer(self.out_size.max(1)).map_err(|e| win(e, "MFCreateMemoryBuffer"))?;
                    s.AddBuffer(&b).map_err(|e| win(e, "AddBuffer"))?;
                    Some(s)
                };
                let mut out = [MFT_OUTPUT_DATA_BUFFER {
                    dwStreamID: 0,
                    pSample: std::mem::ManuallyDrop::new(sample),
                    dwStatus: 0,
                    pEvents: std::mem::ManuallyDrop::new(None),
                }];
                let mut status = 0u32;
                let r = self.mft.ProcessOutput(0, &mut out, &mut status);
                let sample: Option<IMFSample> = std::mem::ManuallyDrop::take(&mut out[0].pSample);
                drop(std::mem::ManuallyDrop::take(&mut out[0].pEvents));
                match r {
                    Ok(()) => {
                        if let Some(s) = sample {
                            self.read_sample(&s)?;
                            *got = true;
                            any = true;
                        }
                    }
                    Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(any),
                    Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                        self.pick_output()?;
                        tracing::info!(coded = ?self.coded, visible = ?self.visible, "decoder picture size");
                    }
                    Err(e) => return Err(win(e, "ProcessOutput")),
                }
            }
        }
    }
}

/// Which codecs this PC can decode in hardware-assisted Media Foundation.
pub fn available_codecs() -> Vec<Codec> {
    [Codec::Hevc, Codec::H264].into_iter().filter(|c| MfDecoder::new(*c, true).is_ok()).collect()
}
