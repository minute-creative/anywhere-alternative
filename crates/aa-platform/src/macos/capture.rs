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

use aa_core::share::ShareTarget;
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
    SCContentFilter, SCDisplay, SCRunningApplication, SCShareableContent, SCStream, SCStreamConfiguration,
    SCStreamOutput, SCStreamOutputType, SCWindow,
};

use crate::{CapturedFrame, FrameBuffer, GpuApi, PlatformError, Result, ScreenCapture};

/// Our own app, never offered in the share lists.
const OWN_BUNDLE: &str = "com.minutecreative.anywhere";

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
    /// The whole screen's size in pixels (after the 4K cap).
    screen: (usize, usize),
    /// What is shared, and the size of that in pixels before the viewer's
    /// Resolution setting.
    share: ShareTarget,
    source: (usize, usize),
    /// The viewer's Resolution setting.
    max: Option<Resolution>,
    display: Retained<SCDisplay>,
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
pub(crate) fn main_display_mode() -> (usize, usize, u16) {
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

/// What could be captured (displays, apps, windows), asked asynchronously.
/// `on_screen`: only windows that are visible now (for the share lists).
pub(super) fn content(on_screen: bool) -> Result<Retained<SCShareableContent>> {
    Ok(wait(|tx| {
        let block = RcBlock::new(move |content: *mut SCShareableContent, error: *mut NSError| {
            let r = match NonNull::new(content) {
                // SAFETY: a non-null content object is valid here; retain it to keep it.
                Some(c) => Ok(Handoff(unsafe { Retained::retain(c.as_ptr()) }.expect("non-null"))),
                None => Err(ns_err("listing what can be shared", error)),
            };
            let _ = tx.send(r);
        });
        // SAFETY: the block lives until it is called (RcBlock copies to the heap).
        unsafe {
            if on_screen {
                SCShareableContent::getShareableContentExcludingDesktopWindows_onScreenWindowsOnly_completionHandler(
                    true, true, &block,
                );
            } else {
                SCShareableContent::getShareableContentWithCompletionHandler(&block);
            }
        }
    })
    .ok_or_else(|| PlatformError::Unavailable("ScreenCaptureKit did not answer".into()))??
    .0)
}

/// Windows a person would pick: ordinary (layer 0), a real size, not ours.
fn pickable(w: &SCWindow) -> Option<(Retained<SCRunningApplication>, String)> {
    // SAFETY: property reads on a live object.
    unsafe {
        let app = w.owningApplication()?;
        let bundle = app.bundleIdentifier().to_string();
        let f = w.frame();
        if w.windowLayer() != 0 || f.size.width < 120.0 || f.size.height < 80.0 || bundle == OWN_BUNDLE {
            return None;
        }
        Some((app, bundle))
    }
}

/// The apps and windows open on this Mac, for the share lists.
pub fn shareable() -> Result<Vec<ShareTarget>> {
    let content = content(true)?;
    let mut out = Vec::new();
    // SAFETY: property reads on live objects.
    unsafe {
        for w in content.windows().iter() {
            let Some((app, bundle)) = pickable(&w) else { continue };
            let name = app.applicationName().to_string();
            if name.is_empty() {
                continue;
            }
            out.push(ShareTarget::App { id: bundle, name: name.clone() });
            let title = w.title().map(|t| t.to_string()).unwrap_or_default();
            out.push(ShareTarget::Window { id: u64::from(w.windowID()), app: name, title });
        }
    }
    Ok(out)
}

/// The app's processes, for keeping the viewer's clicks inside it.
pub(super) fn app_pids(bundle: &str) -> Vec<i32> {
    let Ok(content) = content(false) else { return Vec::new() };
    // SAFETY: property reads on live objects.
    unsafe {
        content
            .applications()
            .iter()
            .filter(|a| a.bundleIdentifier().to_string() == bundle)
            .map(|a| a.processID())
            .collect()
    }
}

/// Points → pixels on the main display (2 on Retina screens).
fn backing_scale(display: &SCDisplay) -> f64 {
    let (w, _, _) = main_display_mode();
    // SAFETY: plain property read.
    let points = unsafe { display.width() }.max(1) as f64;
    (w as f64 / points).clamp(1.0, 4.0)
}

/// The filter for `target` and its size in pixels (`None`: the screen's).
fn filter_for(
    target: &ShareTarget,
    display: &SCDisplay,
) -> Result<(Retained<SCContentFilter>, Option<(usize, usize)>)> {
    // SAFETY: standard ScreenCaptureKit object setup on live objects.
    unsafe {
        match target {
            ShareTarget::Screen => Ok((
                SCContentFilter::initWithDisplay_excludingWindows(SCContentFilter::alloc(), display, &NSArray::new()),
                None,
            )),
            ShareTarget::App { id, name } => {
                let content = content(false)?;
                let apps: Vec<Retained<SCRunningApplication>> =
                    content.applications().iter().filter(|a| a.bundleIdentifier().to_string() == *id).collect();
                if apps.is_empty() {
                    return Err(PlatformError::Unavailable(format!("{name} is not open")));
                }
                let apps = NSArray::from_retained_slice(&apps);
                Ok((
                    SCContentFilter::initWithDisplay_includingApplications_exceptingWindows(
                        SCContentFilter::alloc(),
                        display,
                        &apps,
                        &NSArray::new(),
                    ),
                    None,
                ))
            }
            ShareTarget::Window { id, title, .. } => {
                let content = content(false)?;
                let window = content
                    .windows()
                    .iter()
                    .find(|w| u64::from(w.windowID()) == *id)
                    .ok_or_else(|| PlatformError::Unavailable(format!("the window \"{title}\" was closed")))?;
                let f = window.frame();
                let scale = backing_scale(display);
                let size = ((f.size.width * scale) as usize & !1, (f.size.height * scale) as usize & !1);
                Ok((SCContentFilter::initWithDesktopIndependentWindow(SCContentFilter::alloc(), &window), Some(size)))
            }
        }
    }
}

/// `src` shrunk to fit `max` (keeping its shape), even sizes.
fn fit(src: (usize, usize), max: Option<Resolution>) -> (usize, usize) {
    let r = Resolution::new(src.0 as u32, src.1 as u32);
    let r = max.map_or(r, |m| aa_core::capability::fit_within(r, m));
    ((r.width as usize & !1).max(2), (r.height as usize & !1).max(2))
}

/// The main display as ScreenCaptureKit knows it (asked asynchronously).
pub(super) fn main_display() -> Result<Retained<SCDisplay>> {
    let content = content(false)?;
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
        // Capture as fast as the screen refreshes (up to 240). How many of
        // those frames are sent is the frame-rate ladder's call (it starts
        // at 60 and climbs only while the connection carries every frame
        // sharp); 0.4.6 capped this at 60 for everyone because 200 frames
        // a second at a fixed bitrate made each one blurry and froze Wi-Fi.
        let max_fps: u16 = std::env::var("AA_MAC_MAX_FPS").ok().and_then(|v| v.parse().ok()).unwrap_or(240);
        let refresh_hz = refresh_hz.min(max_fps.max(1));
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
            // No cursor in the picture: the viewer shows its own pointer,
            // which never lags. A drawn-in cursor trailed it on every fast
            // move ("I see two cursors").
            config.setShowsCursor(false);
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
            screen: (w, h),
            share: ShareTarget::Screen,
            source: (w, h),
            max: None,
            display,
        })
    }

    /// Change the stream's picture size (what the viewer gets).
    fn resize(&mut self) -> Result<()> {
        let (w, h) = fit(self.source, self.max);
        // SAFETY: configuring our own live stream.
        unsafe {
            let config = SCStreamConfiguration::new();
            config.setWidth(w);
            config.setHeight(h);
            config.setPixelFormat(kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange);
            config.setColorMatrix(kCVImageBufferYCbCrMatrix_ITU_R_709_2);
            config.setMinimumFrameInterval(CMTime::new(1, i32::from(self.refresh_hz)));
            config.setShowsCursor(false);
            config.setQueueDepth(4);
            config.setScalesToFit(true);
            // One window: no drop shadow around it (it would be sent too).
            config.setIgnoreShadowsSingleWindow(true);
            let done = wait(|tx| {
                let block = RcBlock::new(move |error: *mut NSError| {
                    let _ = tx.send(if error.is_null() { Ok(()) } else { Err(ns_err("resizing the capture", error)) });
                });
                self.stream.updateConfiguration_completionHandler(&config, Some(&block));
            })
            .ok_or_else(|| PlatformError::Unavailable("screen capture did not answer".into()))?;
            done?;
        }
        self.resolution = Resolution::new(w as u32, h as u32);
        tracing::info!(width = w, height = h, "capture size");
        Ok(())
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

    fn set_share(&mut self, target: &ShareTarget) -> Result<()> {
        let (filter, size) = filter_for(target, &self.display)?;
        // SAFETY: updating our own live stream.
        let done = wait(|tx| {
            let block = RcBlock::new(move |error: *mut NSError| {
                let _ = tx.send(if error.is_null() { Ok(()) } else { Err(ns_err("changing what is shared", error)) });
            });
            unsafe { self.stream.updateContentFilter_completionHandler(&filter, Some(&block)) };
        })
        .ok_or_else(|| PlatformError::Unavailable("screen capture did not answer".into()))?;
        done?;
        self.share = target.clone();
        self.source = size.unwrap_or(self.screen);
        self.resize()
    }

    fn set_max_size(&mut self, max: Option<Resolution>) {
        if max == self.max {
            return;
        }
        self.max = max;
        if let Err(e) = self.resize() {
            tracing::warn!("could not change the picture size: {e}");
        }
    }

    fn scales_itself(&self) -> bool {
        true
    }
}
