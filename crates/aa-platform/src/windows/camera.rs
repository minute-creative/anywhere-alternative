//! A PC camera (the laptop's built-in one, or a USB webcam) through Media
//! Foundation's source reader, delivering 1280x720 NV12 pictures.
//!
//! The source reader's "advanced video processing" does the conversions a
//! camera may need (MJPG → pictures, other sizes → 1280x720) on the graphics
//! chip where it can. Windows asks nothing the first time, but its switch
//! Settings → Privacy & security → Camera → "Let desktop apps access your
//! camera" must be on.

#![allow(unsafe_code)]

use std::time::Duration;

use windows::core::PWSTR;
use windows::Win32::Media::MediaFoundation::{
    IMFActivate, IMFAttributes, IMFMediaSource, IMFSourceReader, MFCreateAttributes, MFCreateMediaType,
    MFCreateSourceReaderFromMediaSource, MFEnumDeviceSources, MFMediaType_Video, MFStartup, MFVideoFormat_NV12,
    MFSTARTUP_NOSOCKET, MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME, MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE,
    MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_MAJOR_TYPE,
    MF_MT_SUBTYPE, MF_SOURCE_READERF_ENDOFSTREAM, MF_SOURCE_READERF_ERROR,
    MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING, MF_SOURCE_READER_FIRST_VIDEO_STREAM, MF_VERSION,
};
use windows::Win32::System::Com::{CoInitializeEx, CoTaskMemFree, COINIT_MULTITHREADED};

use crate::camera::{CameraFrame, CameraSource, FPS, HEIGHT, WIDTH};
use crate::{PlatformError, Result};

#[allow(clippy::needless_pass_by_value)] // used as `.map_err(|e| win(e, ...))`
fn win(e: windows::core::Error, what: &str) -> PlatformError {
    PlatformError::Backend(anyhow::anyhow!("{what}: {e}"))
}

fn startup() {
    // SAFETY: per-thread COM/MF start; repeated calls are harmless.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let _ = MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET);
    }
}

/// Every camera Windows knows, with its name.
fn devices() -> Vec<(IMFActivate, String)> {
    startup();
    let mut out = Vec::new();
    // SAFETY: COM calls with valid out-pointers; the array MF hands back is
    // freed with CoTaskMemFree after we took (moved) each element out.
    unsafe {
        let mut attrs: Option<IMFAttributes> = None;
        if MFCreateAttributes(&mut attrs, 1).is_err() {
            return out;
        }
        let Some(attrs) = attrs else { return out };
        if attrs.SetGUID(&MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE, &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID).is_err()
        {
            return out;
        }
        let mut array: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut count = 0u32;
        if MFEnumDeviceSources(&attrs, &mut array, &mut count).is_err() || array.is_null() {
            return out;
        }
        for i in 0..count as usize {
            if let Some(a) = std::ptr::read(array.add(i)) {
                let mut name = PWSTR::null();
                let mut len = 0u32;
                let label = if a.GetAllocatedString(&MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME, &mut name, &mut len).is_ok()
                {
                    let s = name.to_string().unwrap_or_default();
                    CoTaskMemFree(Some(name.0 as *const _));
                    s
                } else {
                    format!("Camera {}", i + 1)
                };
                out.push((a, label));
            }
        }
        CoTaskMemFree(Some(array as *const _));
    }
    out
}

pub fn list() -> Vec<String> {
    devices().into_iter().map(|(_, n)| n).collect()
}

pub struct MfCamera {
    reader: IMFSourceReader,
    source: IMFMediaSource,
    name: String,
}

// SAFETY: the reader and source are free-threaded COM objects (MTA); one
// thread uses them at a time.
unsafe impl Send for MfCamera {}

impl std::fmt::Debug for MfCamera {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MfCamera").field("name", &self.name).finish_non_exhaustive()
    }
}

impl MfCamera {
    pub fn open(prefer: Option<&str>) -> Result<Self> {
        let devs = devices();
        let names: Vec<String> = devs.iter().map(|(_, n)| n.clone()).collect();
        let ix = crate::camera::choose(&names, prefer).ok_or_else(|| {
            PlatformError::Unavailable(
                "this PC has no camera (or desktop apps may not use it: Settings → Privacy & security → Camera)".into(),
            )
        })?;
        let (activate, name) = &devs[ix];
        // SAFETY: COM calls on objects we own; out-pointers are locals.
        unsafe {
            let source: IMFMediaSource = activate.ActivateObject().map_err(|e| {
                PlatformError::Unavailable(format!(
                    "could not open {name} ({e}). Another app may be using it, or Settings → Privacy & security → \
                     Camera → \"Let desktop apps access your camera\" is off"
                ))
            })?;
            let mut attrs: Option<IMFAttributes> = None;
            MFCreateAttributes(&mut attrs, 1).map_err(|e| win(e, "attributes"))?;
            let attrs = attrs.ok_or_else(|| PlatformError::Backend(anyhow::anyhow!("no attributes")))?;
            attrs.SetUINT32(&MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING, 1).map_err(|e| win(e, "processing"))?;
            let reader = MFCreateSourceReaderFromMediaSource(&source, &attrs).map_err(|e| win(e, "source reader"))?;
            let t = MFCreateMediaType().map_err(|e| win(e, "media type"))?;
            t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).map_err(|e| win(e, "major type"))?;
            t.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12).map_err(|e| win(e, "subtype"))?;
            t.SetUINT64(&MF_MT_FRAME_SIZE, (u64::from(WIDTH) << 32) | u64::from(HEIGHT)).map_err(|e| win(e, "size"))?;
            t.SetUINT64(&MF_MT_FRAME_RATE, (u64::from(FPS) << 32) | 1).map_err(|e| win(e, "rate"))?;
            let stream = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
            reader.SetCurrentMediaType(stream, None, &t).map_err(|e| win(e, "camera picture format"))?;
            tracing::info!(camera = name, "camera on");
            Ok(Self { reader, source, name: name.clone() })
        }
    }
}

impl CameraSource for MfCamera {
    fn next_frame(&mut self, _timeout: Duration) -> Result<Option<CameraFrame>> {
        // ReadSample waits for the next picture (about 33 ms at 30 fps).
        let stream = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
        let mut flags = 0u32;
        let mut sample = None;
        // SAFETY: synchronous read with valid out-pointers.
        unsafe {
            self.reader
                .ReadSample(stream, 0, None, Some(&mut flags), None, Some(&mut sample))
                .map_err(|e| PlatformError::DeviceLost(format!("camera read: {e}")))?;
        }
        if flags & (MF_SOURCE_READERF_ERROR.0 as u32 | MF_SOURCE_READERF_ENDOFSTREAM.0 as u32) != 0 {
            return Err(PlatformError::DeviceLost("the camera stopped (unplugged, or another app took it)".into()));
        }
        let Some(sample) = sample else { return Ok(None) };
        // SAFETY: lock the contiguous buffer, copy, unlock.
        unsafe {
            let buf = sample.ConvertToContiguousBuffer().map_err(|e| win(e, "camera buffer"))?;
            let mut p = std::ptr::null_mut();
            let mut len = 0u32;
            buf.Lock(&mut p, None, Some(&mut len)).map_err(|e| win(e, "camera lock"))?;
            let need = (WIDTH * HEIGHT * 3 / 2) as usize;
            let frame = (len as usize >= need && !p.is_null()).then(|| CameraFrame {
                nv12: std::slice::from_raw_parts(p, need).to_vec(),
                width: WIDTH,
                height: HEIGHT,
            });
            let _ = buf.Unlock();
            Ok(frame)
        }
    }

    fn name(&self) -> String {
        self.name.clone()
    }
}

impl Drop for MfCamera {
    fn drop(&mut self) {
        // SAFETY: shutting down our own source turns the camera light off.
        unsafe {
            let _ = self.source.Shutdown();
        }
        tracing::info!(camera = self.name, "camera off");
    }
}
