//! A Mac camera (built-in, USB, or an iPhone through Continuity Camera)
//! through AVFoundation, delivering 1280x720 NV12 pictures.
//!
//! The first use shows macOS's "Anywhere would like to access the camera"
//! question; the answer is kept in System Settings → Privacy & Security →
//! Camera.

#![allow(unsafe_code)]

use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender};
use std::time::Duration;

use block2::RcBlock;
use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Bool, ProtocolObject};
use objc2::{define_class, msg_send, AllocAnyThread, DefinedClass};
use objc2_av_foundation::{
    AVAuthorizationStatus, AVCaptureConnection, AVCaptureDevice, AVCaptureDeviceInput, AVCaptureOutput,
    AVCaptureSession, AVCaptureSessionPreset1280x720, AVCaptureVideoDataOutput,
    AVCaptureVideoDataOutputSampleBufferDelegate, AVMediaTypeVideo,
};
use objc2_core_foundation::CFString;
use objc2_core_media::CMSampleBuffer;
use objc2_core_video::{
    kCVPixelBufferHeightKey, kCVPixelBufferPixelFormatTypeKey, kCVPixelBufferWidthKey,
    kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange, CVPixelBufferGetBaseAddressOfPlane,
    CVPixelBufferGetBytesPerRowOfPlane, CVPixelBufferGetHeightOfPlane, CVPixelBufferGetWidthOfPlane,
    CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
};
use objc2_foundation::{NSDictionary, NSNumber, NSObject, NSObjectProtocol, NSString};

use crate::camera::{CameraFrame, CameraSource, HEIGHT, WIDTH};
use crate::{PlatformError, Result};

struct Ivars {
    frames: SyncSender<CameraFrame>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements and we don't impl Drop.
    #[unsafe(super(NSObject))]
    #[name = "AACameraOutput"]
    #[ivars = Ivars]
    struct CameraOutput;

    unsafe impl NSObjectProtocol for CameraOutput {}

    unsafe impl AVCaptureVideoDataOutputSampleBufferDelegate for CameraOutput {
        #[unsafe(method(captureOutput:didOutputSampleBuffer:fromConnection:))]
        fn did_output(&self, _output: &AVCaptureOutput, sample: &CMSampleBuffer, _connection: &AVCaptureConnection) {
            // SAFETY: `sample` is valid for this callback.
            let Some(image) = (unsafe { sample.image_buffer() }) else { return };
            if let Some(f) = copy_nv12(&image) {
                // Newest wins: a full queue means the encoder is behind.
                let _ = self.ivars().frames.try_send(f);
            }
        }
    }
);

impl CameraOutput {
    fn new(frames: SyncSender<CameraFrame>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(Ivars { frames });
        // SAFETY: NSObject's plain init.
        unsafe { msg_send![super(this), init] }
    }
}

/// Copy a locked NV12 pixel buffer into a packed `CameraFrame`.
fn copy_nv12(px: &objc2_core_video::CVPixelBuffer) -> Option<CameraFrame> {
    // SAFETY: lock for reading, read inside the plane bounds the buffer
    // reports, unlock.
    unsafe {
        if CVPixelBufferLockBaseAddress(px, CVPixelBufferLockFlags::ReadOnly) != 0 {
            return None;
        }
        let w = CVPixelBufferGetWidthOfPlane(px, 0) & !1;
        let h = CVPixelBufferGetHeightOfPlane(px, 0) & !1;
        let y = CVPixelBufferGetBaseAddressOfPlane(px, 0).cast::<u8>();
        let uv = CVPixelBufferGetBaseAddressOfPlane(px, 1).cast::<u8>();
        let (ys, uvs) = (CVPixelBufferGetBytesPerRowOfPlane(px, 0), CVPixelBufferGetBytesPerRowOfPlane(px, 1));
        let out = if y.is_null() || uv.is_null() || w < 2 || h < 2 {
            None
        } else {
            let mut nv12 = Vec::with_capacity(w * h * 3 / 2);
            for r in 0..h {
                nv12.extend_from_slice(std::slice::from_raw_parts(y.add(r * ys), w));
            }
            for r in 0..h / 2 {
                nv12.extend_from_slice(std::slice::from_raw_parts(uv.add(r * uvs), w));
            }
            Some(if (w, h) == (WIDTH as usize, HEIGHT as usize) {
                CameraFrame { nv12, width: WIDTH, height: HEIGHT }
            } else {
                crate::camera::fit_nv12(&nv12, w, h, w)
            })
        };
        CVPixelBufferUnlockBaseAddress(px, CVPixelBufferLockFlags::ReadOnly);
        out
    }
}

/// A Core Foundation string is the same object as a Foundation string
/// ("toll-free bridged"), so a reference can be reinterpreted.
fn ns(s: &CFString) -> &NSString {
    // SAFETY: CFString and NSString are toll-free bridged.
    unsafe { &*std::ptr::from_ref(s).cast::<NSString>() }
}

#[allow(deprecated)] // the discovery-session replacement needs per-OS device type lists
fn devices() -> Vec<Retained<AVCaptureDevice>> {
    // SAFETY: plain class method with a static media type.
    unsafe {
        let Some(video) = AVMediaTypeVideo else { return Vec::new() };
        AVCaptureDevice::devicesWithMediaType(video).iter().collect()
    }
}

fn name_of(d: &AVCaptureDevice) -> String {
    // SAFETY: plain property read.
    unsafe { d.localizedName() }.to_string()
}

pub fn list() -> Vec<String> {
    devices().iter().map(|d| name_of(d)).collect()
}

/// Ask for camera permission if macOS hasn't asked yet; `Ok` if granted.
fn permission() -> Result<()> {
    // SAFETY: class methods with a static media type; the block lives until called.
    unsafe {
        let Some(video) = AVMediaTypeVideo else { return Ok(()) };
        let status = AVCaptureDevice::authorizationStatusForMediaType(video);
        if status == AVAuthorizationStatus::Authorized {
            return Ok(());
        }
        if status == AVAuthorizationStatus::NotDetermined {
            let (tx, rx) = sync_channel(1);
            let block = RcBlock::new(move |granted: Bool| {
                let _ = tx.send(granted.as_bool());
            });
            AVCaptureDevice::requestAccessForMediaType_completionHandler(video, &block);
            if rx.recv_timeout(Duration::from_secs(120)).unwrap_or(false) {
                return Ok(());
            }
        }
    }
    Err(PlatformError::Unavailable(
        "Anywhere may not use the camera: allow it in System Settings → Privacy & Security → Camera".into(),
    ))
}

pub struct MacCamera {
    session: Retained<AVCaptureSession>,
    _output: Retained<AVCaptureVideoDataOutput>,
    _delegate: Retained<CameraOutput>,
    _queue: dispatch2::DispatchRetained<DispatchQueue>,
    frames: Receiver<CameraFrame>,
    name: String,
}

// SAFETY: AVCaptureSession may be started and stopped from any thread; the
// delegate runs on its own queue and only touches a channel.
unsafe impl Send for MacCamera {}

impl std::fmt::Debug for MacCamera {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MacCamera").field("name", &self.name).finish_non_exhaustive()
    }
}

impl MacCamera {
    pub fn open(prefer: Option<&str>) -> Result<Self> {
        permission()?;
        let devices = devices();
        let names: Vec<String> = devices.iter().map(|d| name_of(d)).collect();
        let ix = crate::camera::choose(&names, prefer)
            .ok_or_else(|| PlatformError::Unavailable("this Mac has no camera".into()))?;
        let device = &devices[ix];
        let (tx, frames) = sync_channel(2);
        // SAFETY: standard AVFoundation set-up on objects we own.
        unsafe {
            let session = AVCaptureSession::new();
            session.beginConfiguration();
            if session.canSetSessionPreset(AVCaptureSessionPreset1280x720) {
                session.setSessionPreset(AVCaptureSessionPreset1280x720);
            }
            let input = AVCaptureDeviceInput::deviceInputWithDevice_error(device).map_err(|e| {
                PlatformError::Unavailable(format!("could not open {}: {}", names[ix], e.localizedDescription()))
            })?;
            if !session.canAddInput(&input) {
                return Err(PlatformError::Unavailable(format!("{} is busy", names[ix])));
            }
            session.addInput(&input);

            let output = AVCaptureVideoDataOutput::new();
            let keys: [&NSString; 3] =
                [ns(kCVPixelBufferPixelFormatTypeKey), ns(kCVPixelBufferWidthKey), ns(kCVPixelBufferHeightKey)];
            let values: [Retained<NSNumber>; 3] = [
                NSNumber::new_u32(kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange),
                NSNumber::new_u32(WIDTH),
                NSNumber::new_u32(HEIGHT),
            ];
            let value_refs: [&AnyObject; 3] = [&values[0], &values[1], &values[2]];
            let settings = NSDictionary::<NSString, AnyObject>::from_slices(&keys, &value_refs);
            output.setVideoSettings(Some(&settings));
            output.setAlwaysDiscardsLateVideoFrames(true);
            let delegate = CameraOutput::new(tx);
            let queue = DispatchQueue::new("anywhere.camera", None);
            output.setSampleBufferDelegate_queue(Some(ProtocolObject::from_ref(&*delegate)), Some(&queue));
            if !session.canAddOutput(&output) {
                return Err(PlatformError::Unavailable("the camera won't give pictures".into()));
            }
            session.addOutput(&output);
            session.commitConfiguration();
            session.startRunning();
            tracing::info!(camera = names[ix], "camera on");
            Ok(Self { session, _output: output, _delegate: delegate, _queue: queue, frames, name: names[ix].clone() })
        }
    }
}

impl CameraSource for MacCamera {
    fn next_frame(&mut self, timeout: Duration) -> Result<Option<CameraFrame>> {
        match self.frames.recv_timeout(timeout) {
            Ok(f) => Ok(Some(f)),
            Err(RecvTimeoutError::Timeout) => Ok(None),
            Err(RecvTimeoutError::Disconnected) => Err(PlatformError::DeviceLost("camera stopped".into())),
        }
    }

    fn name(&self) -> String {
        self.name.clone()
    }
}

impl Drop for MacCamera {
    fn drop(&mut self) {
        // SAFETY: stopping our own session; turns the green light off.
        unsafe { self.session.stopRunning() };
        tracing::info!(camera = self.name, "camera off");
    }
}
