//! `aa-platform`: the only crate allowed to know which OS it is on.
//!
//! It exposes five traits. The host and viewer are written against the
//! traits only; each OS (and each GPU vendor) supplies implementations.
//!
//! | Trait             | macOS backend                 | Windows backend(s)                      |
//! |-------------------|-------------------------------|-----------------------------------------|
//! | [`ScreenCapture`] | `ScreenCaptureKit`              | DXGI Desktop Duplication                |
//! | [`VideoEncoder`]  | `VideoToolbox`                  | NVENC / AMF / `QuickSync` / Media Foundation |
//! | [`VideoDecoder`]  | `VideoToolbox`                  | D3D11 Video / Media Foundation          |
//! | [`InputInjector`] | `CGEvent`                       | `SendInput`                               |
//! | [`VirtualGamepad`]| `IOKit` user-space HID (spike)  | `ViGEm`                                   |
//!
//! The [`mock`] module implements every trait with no hardware (test pattern
//! in, bytes out) so the complete pipeline runs in CI on Linux and so either
//! side can be developed before the other's backend exists.

#![warn(unsafe_code)]

use std::time::Duration;

use aa_core::capability::Capabilities;
use aa_core::input::{InputEvent, Rumble};
use aa_core::video::{Codec, EncodedFrameMeta, PixelFormat, Resolution};
use bytes::Bytes;

pub mod audio;
pub mod clipboard;
pub mod discover;
pub mod ds5dev;
pub mod hid_mac;
pub mod hid_scancode;
pub mod lan;
pub mod mock;
pub mod padhub;
pub mod padmap;
pub mod playout;
pub mod sw;
pub mod usbip;

#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(target_os = "windows")]
pub mod windows;

/// Where a captured picture lives. The fast path keeps it on the GPU and
/// hands a handle straight to the encoder; the CPU variant exists for the
/// mock backend and for debugging.
#[derive(Debug)]
pub enum FrameBuffer {
    /// Tightly packed rows of `PixelFormat`.
    Cpu(Bytes),
    /// An opaque GPU texture handle. The meaning of `handle` is defined by
    /// the backend pair that shares it (e.g. an `IOSurfaceRef` on macOS, an
    /// `ID3D11Texture2D*` on Windows). Encoders check `api` and refuse
    /// handles they do not understand.
    Gpu { api: GpuApi, handle: usize },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GpuApi {
    Metal,
    D3D11,
}

/// One captured picture.
#[derive(Debug)]
pub struct CapturedFrame {
    pub buffer: FrameBuffer,
    pub format: PixelFormat,
    pub resolution: Resolution,
    /// Microseconds on the host's monotonic clock at the moment the
    /// compositor produced this picture.
    pub capture_ts_us: u64,
}

/// One compressed picture ready for the wire.
#[derive(Debug)]
pub struct EncodedFrame {
    pub meta: EncodedFrameMeta,
    pub data: Bytes,
}

/// One decoded picture ready to present. Same storage story as capture.
/// CPU `Nv12` is tightly packed: `w*h` luma bytes, then `w*h/2` of
/// interleaved U,V.
#[derive(Debug)]
pub struct DecodedFrame {
    pub buffer: FrameBuffer,
    pub format: PixelFormat,
    pub resolution: Resolution,
    pub frame_id: u32,
}

impl DecodedFrame {
    /// Bytes in a CPU frame (0 for GPU frames).
    pub fn buffer_len(&self) -> usize {
        match &self.buffer {
            FrameBuffer::Cpu(b) => b.len(),
            FrameBuffer::Gpu { .. } => 0,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PlatformError {
    #[error("permission denied: {0}")]
    Permission(String),
    #[error("backend not available on this machine: {0}")]
    Unavailable(String),
    #[error("not implemented yet: {0}")]
    NotImplemented(&'static str),
    /// The GPU or display went away under us (driver reset, sleep/resume,
    /// monitor unplugged): the whole capture/encode pipeline must be rebuilt.
    #[error("device lost: {0}")]
    DeviceLost(String),
    #[error("{0}")]
    Backend(#[from] anyhow::Error),
}

pub type Result<T> = std::result::Result<T, PlatformError>;

/// Produces frames from a display, as fast as the display refreshes.
pub trait ScreenCapture: Send {
    /// Block until the next frame is available or `timeout` elapses.
    /// Returns `Ok(None)` on timeout (nothing changed on screen), which the
    /// host treats as "re-send nothing"; static desktops cost zero bandwidth.
    fn next_frame(&mut self, timeout: Duration) -> Result<Option<CapturedFrame>>;
    fn resolution(&self) -> Resolution;
    fn refresh_rate_hz(&self) -> u16;
    /// Why no frames are coming, when the cause is known and outside our
    /// control: e.g. "PC is locked" (Windows hides the lock screen and UAC
    /// prompts from normal programs). Shown to the viewer.
    fn unavailable_reason(&self) -> Option<&'static str> {
        None
    }
}

/// Turns captured frames into a codec bitstream.
pub trait VideoEncoder: Send {
    fn encode(&mut self, frame: &CapturedFrame, force_keyframe: bool) -> Result<EncodedFrame>;
    /// Change target bitrate without restarting. Called by congestion control.
    fn set_bitrate_kbps(&mut self, kbps: u32) -> Result<()>;
    /// Ask the encoder to begin an intra-refresh cycle so a viewer that lost
    /// a frame recovers within a few frames without a full keyframe spike.
    fn request_intra_refresh(&mut self) -> Result<()>;
}

/// Turns a codec bitstream back into pictures.
pub trait VideoDecoder: Send {
    fn decode(&mut self, frame_id: u32, data: &Bytes) -> Result<Option<DecodedFrame>>;
}

/// Applies viewer input to the host OS.
pub trait InputInjector: Send {
    fn inject(&mut self, event: InputEvent) -> Result<()>;
}

/// Presents a virtual game controller to the host OS.
pub trait VirtualGamepad: Send {
    /// A controller appeared on the viewer: plug in a matching virtual one.
    fn attach(&mut self, _slot: u8, _kind: aa_core::input::GamepadKind) -> Result<()> {
        Ok(())
    }
    /// The viewer's controller went away: unplug its virtual twin.
    fn detach(&mut self, _slot: u8) -> Result<()> {
        Ok(())
    }
    fn update(&mut self, slot: u8, state: aa_core::input::GamepadState) -> Result<()>;
    /// Poll for rumble the game sent to the virtual pad, to forward to the viewer.
    fn poll_rumble(&mut self) -> Result<Option<Rumble>>;
}

/// Everything a host needs, wired together for the current OS.
/// Builds an encoder for the codec a viewer negotiated. The codec is only
/// known once a viewer says Hello, so the host cannot pick its encoder at
/// startup; it asks this instead.
pub type EncoderFactory = Box<dyn FnMut(Codec, Resolution, u16) -> Result<Box<dyn VideoEncoder>> + Send>;
/// Rebuilds capture from scratch (new GPU device, re-picked display) plus an
/// encoder factory bound to it. Used when the old pipeline is beyond repair:
/// GPU reset, sleep/resume, the captured monitor unplugged.
pub type PipelineFactory = Box<dyn FnMut() -> Result<(Box<dyn ScreenCapture>, EncoderFactory)> + Send>;

/// Keep this PC awake with its display on while someone is streaming it
/// (the person isn't touching it, so Windows would otherwise dim, lock and
/// sleep). Call from a long-lived thread; `false` restores normal power.
pub fn keep_awake(on: bool) {
    #[cfg(target_os = "windows")]
    windows::keep_awake(on);
    #[cfg(target_os = "macos")]
    macos::keep_awake(on);
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let _ = on;
}
/// Same idea on the viewer: build the decoder for the codec the host chose.
pub type DecoderFactory = Box<dyn FnMut(Codec) -> Result<Box<dyn VideoDecoder>> + Send>;

pub struct HostBackends {
    pub capture: Box<dyn ScreenCapture>,
    /// Encoder for `capabilities.codecs[0]`, ready to go. Must really be that
    /// codec: the session treats `codecs[0]` as "what is loaded" and only
    /// rebuilds when a viewer negotiates something else.
    pub encoder: Box<dyn VideoEncoder>,
    /// Builds an encoder for any codec in `capabilities.codecs`, at any size.
    pub encoder_factory: Option<EncoderFactory>,
    /// Starts over from nothing when capture is broken for good.
    pub rebuild: Option<PipelineFactory>,
    pub input: Box<dyn InputInjector>,
    pub gamepad: Option<Box<dyn VirtualGamepad>>,
    /// System-audio source; `None` on platforms without one yet.
    pub audio: Option<Box<dyn audio::AudioCapture>>,
    /// Host speaker mute; `None` where not implemented.
    pub speaker: Option<Box<dyn audio::SpeakerControl>>,
    /// Shared copy/paste; `None` where there is no clipboard.
    pub clipboard: Option<Box<dyn clipboard::SystemClipboard>>,
    /// Plugs a virtual USB device (the `DualSense` mirror) into this
    /// computer; `None` where that is not possible (`DualSense`s then work as
    /// generic controllers).
    pub pad_attach: Option<padhub::Attach>,
    pub capabilities: Capabilities,
}

impl std::fmt::Debug for HostBackends {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostBackends").field("capabilities", &self.capabilities).finish_non_exhaustive()
    }
}

/// Everything a viewer needs, wired together for the current OS.
pub struct ViewerBackends {
    /// Builds the decoder once the codec is negotiated; must succeed for
    /// every codec in `capabilities.codecs`.
    pub decoder: DecoderFactory,
    /// Shared copy/paste; `None` where there is no clipboard.
    pub clipboard: Option<Box<dyn clipboard::SystemClipboard>>,
    pub capabilities: Capabilities,
}

impl std::fmt::Debug for ViewerBackends {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ViewerBackends").field("capabilities", &self.capabilities).finish_non_exhaustive()
    }
}

/// Build the real backends for this OS. Returns `NotImplemented` on
/// platforms whose backend is not written yet, so callers fall back to
/// [`mock`] with an explicit log line rather than silently.
pub fn host_backends() -> Result<HostBackends> {
    #[cfg(target_os = "macos")]
    {
        macos::host_backends()
    }
    #[cfg(target_os = "windows")]
    {
        windows::host_backends()
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        Err(PlatformError::Unavailable("no real host backend for this OS; use --mock".into()))
    }
}

pub fn viewer_backends() -> Result<ViewerBackends> {
    #[cfg(target_os = "macos")]
    {
        macos::viewer_backends()
    }
    #[cfg(target_os = "windows")]
    {
        windows::viewer_backends()
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        Err(PlatformError::Unavailable("no real viewer backend for this OS; use --mock".into()))
    }
}
