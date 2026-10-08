//! Screen capture on macOS through `ScreenCaptureKit`.
//!
//! `ScreenCaptureKit` (macOS 12.3+) is the modern, supported way to capture
//! the screen; the older APIs are deprecated or gone in macOS 15. It hands
//! us each new picture as a GPU-backed `CVPixelBuffer` (an `IOSurface`), in
//! the exact format the hardware encoder wants (NV12, BT.709 video range),
//! so nothing is copied or converted on the CPU. It only sends a frame when
//! something on screen changed, so a still desktop costs nothing.
//!
//! macOS asks the user once for Screen Recording permission (System
//! Settings → Privacy & Security → Screen & System Audio Recording) for the
//! app that runs `aa-host`, normally Terminal. Without it we get an error
//! here, which we turn into plain instructions.

#![allow(unsafe_code, clippy::pedantic)]

use std::ptr::NonNull;
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender};
use std::time::{Duration, Instant};

use aa_core::video::{PixelFormat, Resolution};
use block2::RcBlock;
use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{define_class, msg_send, AllocAnyThread, DefinedClass};
use objc2_core_foundation::CFRetained;
use objc2_core_graphics::{
    CGDisplayCopyDisplayMode, CGDisplayMode, CGMainDisplayID, CGPreflightScreenCaptureAccess,
    CGRequestScreenCaptureAccess,
};
use objc2_core_media::{CMSampleBuffer, CMTime};
use objc2_core_video::{
    kCVImageBufferYCbCrMatrix_ITU_R_709_2, kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange, CVPixelBuffer,
    CVPixelBufferGetHeight, CVPixelBufferGetWidth,
};
use objc2_foundation::{NSArray, NSError, NSObject, NSObjectProtocol};
use objc2_screen_capture_kit::{
    SCContentFilter, SCDisplay, SCShareableContent, SCStream, SCStreamConfiguration, SCStreamOutput, SCStreamOutputType,
};

use crate::{CapturedFrame, FrameBuffer, GpuApi, PlatformError, Result, ScreenCapture};

/// Largest width we capture at. A 5K or 6K display is scaled down to this:
/// beyond 4K the stream gets no sharper on the viewer's screen, only
/// heavier to encode and send. `AA_MAC_MAX_WIDTH` overrides it.
const DEFAULT_MAX_WIDTH: usize = 3840;

/// A captured picture, held so the GPU memory stays valid until encoded.
struct Pixels(CFRetained<CVPixelBuffer>);
// SAFETY: a CVPixelBuffer is a reference-counted, thread-safe CF object;
// we only read it after ScreenCaptureKit has finished writing it.
unsafe impl Send for Pixels {}

/// What the capture callback needs.
struct OutputIvars {
    frames: SyncSender<(Pixels, Instant)>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements and we don't impl Drop.
    #[unsafe(super(NSObject))]
    #[name = "AAScreenOutput"]
    #[ivars = OutputIvars]
    struct ScreenOutput;

    unsafe impl NSObjectProtocol for ScreenOutput {}

    unsafe impl SCStreamOutput for ScreenOutput {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        fn did_output(&self, _stream: &SCStream, sample: &CMSampleBuffer, kind: SCStreamOutputType) {
            if kind != SCStreamOutputType::Screen {
                return;
            }
            // Frames that only say "nothing changed" carry no picture.
            // SAFETY: `sample` is valid for the duration of this callback.
            let Some(image) = (unsafe { sample.image_buffer() }) else { return };
            // Newest wins: if the encoder is behind, drop this one rather
            // than queue up latency.
            let _ = self.ivars().frames.try_send((Pixels(image), Instant::now()));
        }
    }
);

impl ScreenOutput {
    fn new(frames: SyncSender<(Pixels, Instant)>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(OutputIvars { frames });
        // SAFETY: NSObject's plain init.
        unsafe { msg_send![super(this), init] }
    }
}

pub struct SckCapture {
    stream: Retained<SCStream>,
    _output: Retained<ScreenOutput>,
    _queue: dispatch2::DispatchRetained<DispatchQueue>,
    frames: Receiver<(Pixels, Instant)>,
    /// The frame being encoded; its handle is only valid while held here.
    current: Option<Pixels>,
    resolution: Resolution,
    refresh_hz: u16,
    started: Instant,
}

// SAFETY: the SCStream and our output object are thread-safe Objective-C
// objects (ScreenCaptureKit calls us on its own queue); the struct is used
// by one capture thread at a time.
unsafe impl Send for SckCapture {}

impl std::fmt::Debug for SckCapture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SckCapture").field("resolution", &self.resolution).finish_non_exhaustive()
    }
}

/// Run an asynchronous ScreenCaptureKit call and wait for its answer.
pub(super) fn wait<T: Send + 'static>(start: impl FnOnce(SyncSender<T>)) -> Option<T> {
    let (tx, rx) = sync_channel(1);
    start(tx);
    rx.recv_timeout(Duration::from_secs(10)).ok()
}

pub(super) fn ns_err(what: &str, e: *mut NSError) -> PlatformError {
    // SAFETY: a non-null NSError handed to a completion handler is valid
    // for the duration of the handler; we only format it there.
    let text = NonNull::new(e).map_or_else(String::new, |e| unsafe { e.as_ref() }.localizedDescription().to_string());
    PlatformError::Backend(anyhow::anyhow!("{what}: {text}"))
}

/// The main display's size in real pixels and its refresh rate.
fn main_display_mode() -> (usize, usize, u16) {
    let mode = CGDisplayCopyDisplayMode(CGMainDisplayID());
    let w = CGDisplayMode::pixel_width(mode.as_deref());
    let h = CGDisplayMode::pixel_height(mode.as_deref());
    let hz = CGDisplayMode::refresh_rate(mode.as_deref());
    // Built-in panels can report 0; ProMotion reports its maximum.
    let hz = if hz >= 1.0 { hz.round() as u16 } else { 60 };
    (w, h, hz)
}

/// Carries an Objective-C object across the completion handler's thread.
struct Handoff<T>(T);
// SAFETY: the object is retained and only touched again by the receiving
// thread after the sender has let go of it.
unsafe impl<T> Send for Handoff<T> {}

/// The main display as ScreenCaptureKit knows it (asked asynchronously).
pub(super) fn main_display() -> Result<Retained<SCDisplay>> {
    let content = wait(|tx| {
        let block = RcBlock::new(move |content: *mut SCShareableContent, error: *mut NSError| {
            let r = match NonNull::new(content) {
                // SAFETY: a non-null content object is valid here; retain it to keep it.
                Some(c) => Ok(Handoff(unsafe { Retained::retain(c.as_ptr()) }.expect("non-null"))),
                None => Err(ns_err("listing displays", error)),
            };
            let _ = tx.send(r);
        });
        // SAFETY: the block lives until it is called (RcBlock copies to the heap).
        unsafe { SCShareableContent::getShareableContentWithCompletionHandler(&block) };
    })
    .ok_or_else(|| PlatformError::Unavailable("ScreenCaptureKit did not answer".into()))??
    .0;
    let main_id = CGMainDisplayID();
    // SAFETY: plain property reads on live objects.
    let displays = unsafe { content.displays() };
    let found = displays.iter().find(|d| unsafe { d.displayID() } == main_id).or_else(|| displays.iter().next());
    found.ok_or_else(|| PlatformError::Unavailable("no display to capture".into()))
}

/// Ask macOS for Screen Recording permission if we don't have it.
pub(super) fn check_permission() -> Result<()> {
    if CGPreflightScreenCaptureAccess() {
        return Ok(());
    }
    let _ = CGRequestScreenCaptureAccess();
    Err(PlatformError::Permission(
        "macOS has not allowed this app to record the screen yet. Allow it in System Settings → Privacy \
         & Security → Screen & System Audio Recording (turn on Terminal, or whichever app runs \
         aa-host), then quit that app completely and start aa-host again"
            .into(),
    ))
}

impl SckCapture {
    pub fn new() -> Result<Self> {
        check_permission()?;
        let display = main_display()?;

        let (mut w, mut h, refresh_hz) = main_display_mode();
        let max_w = std::env::var("AA_MAC_MAX_WIDTH").ok().and_then(|v| v.parse().ok()).unwrap_or(DEFAULT_MAX_WIDTH);
        if w > max_w {
            h = h * max_w / w;
            w = max_w;
        }
        // Encoders want even sizes.
        let (w, h) = (w & !1, h & !1);

        // SAFETY: standard ScreenCaptureKit object setup on live objects.
        let stream = unsafe {
            let filter =
                SCContentFilter::initWithDisplay_excludingWindows(SCContentFilter::alloc(), &display, &NSArray::new());
            let config = SCStreamConfiguration::new();
            config.setWidth(w);
            config.setHeight(h);
            config.setPixelFormat(kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange);
            config.setColorMatrix(kCVImageBufferYCbCrMatrix_ITU_R_709_2);
            config.setMinimumFrameInterval(CMTime::new(1, i32::from(refresh_hz)));
            config.setShowsCursor(true);
            config.setQueueDepth(4);
            config.setScalesToFit(true);
            SCStream::initWithFilter_configuration_delegate(SCStream::alloc(), &filter, &config, None)
        };

        let (tx, frames) = sync_channel(2);
        let output = ScreenOutput::new(tx);
        let queue = DispatchQueue::new("aa.screen-capture", None);
        // SAFETY: the output object and queue outlive the stream (both kept in Self).
        unsafe {
            stream.addStreamOutput_type_sampleHandlerQueue_error(
                ProtocolObject::from_ref(&*output),
                SCStreamOutputType::Screen,
                Some(&queue),
            )
        }
        .map_err(|e| {
            PlatformError::Backend(anyhow::anyhow!("adding the capture output: {}", e.localizedDescription()))
        })?;

        let started = wait(|tx| {
            let block = RcBlock::new(move |error: *mut NSError| {
                let _ = tx.send(if error.is_null() { Ok(()) } else { Err(ns_err("starting screen capture", error)) });
            });
            // SAFETY: as above.
            unsafe { stream.startCaptureWithCompletionHandler(Some(&block)) };
        })
        .ok_or_else(|| PlatformError::Unavailable("screen capture did not start".into()))?;
        started?;

        tracing::info!(width = w, height = h, refresh_hz, "screen capture: ScreenCaptureKit");
        Ok(Self {
            stream,
            _output: output,
            _queue: queue,
            frames,
            current: None,
            resolution: Resolution::new(w as u32, h as u32),
            refresh_hz,
            started: Instant::now(),
        })
    }
}

impl Drop for SckCapture {
    fn drop(&mut self) {
        // SAFETY: stopping a live stream; we don't wait for the answer.
        unsafe { self.stream.stopCaptureWithCompletionHandler(None) };
    }
}

impl ScreenCapture for SckCapture {
    fn next_frame(&mut self, timeout: Duration) -> Result<Option<CapturedFrame>> {
        let (pixels, at) = match self.frames.recv_timeout(timeout) {
            Ok(f) => f,
            Err(RecvTimeoutError::Timeout) => return Ok(None),
            Err(RecvTimeoutError::Disconnected) => {
                return Err(PlatformError::DeviceLost("screen capture stopped".into()));
            }
        };
        let width = CVPixelBufferGetWidth(&pixels.0) as u32;
        let height = CVPixelBufferGetHeight(&pixels.0) as u32;
        let handle = CFRetained::as_ptr(&pixels.0).as_ptr() as usize;
        self.current = Some(pixels);
        Ok(Some(CapturedFrame {
            buffer: FrameBuffer::Gpu { api: GpuApi::Metal, handle },
            format: PixelFormat::Nv12,
            resolution: Resolution::new(width, height),
            capture_ts_us: at.saturating_duration_since(self.started).as_micros() as u64,
        }))
    }

    fn resolution(&self) -> Resolution {
        self.resolution
    }

    fn refresh_rate_hz(&self) -> u16 {
        self.refresh_hz
    }
}
