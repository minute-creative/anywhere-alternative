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
use aa_core::share::ShareTarget;
use aa_core::video::{Codec, EncodedFrameMeta, PixelFormat, Resolution};
use bytes::Bytes;

pub mod apps;
pub mod audio;
pub mod camera;
pub mod clipboard;
pub mod discover;
pub mod ds5dev;
pub mod hid_mac;
pub mod hid_scancode;
pub mod hidmouse;
pub mod lan;
pub mod mock;
pub mod padhub;
pub mod padmap;
pub mod pairing;
pub mod playout;
pub mod portmap;
pub mod share;
pub mod sw;
pub mod tailscale;
pub mod telemetry;
pub mod trust;
pub mod update;
pub mod usbip;

#[cfg(target_os = "ios")]
pub mod ios;
#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(target_os = "windows")]
pub mod windows;

/// Where a captured picture lives. The fast path keeps it on the GPU and
/// hands a handle straight to the encoder; the CPU variant exists for the
/// mock backend and for debugging.
#[derive(Debug, Clone)]
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
#[derive(Debug, Clone)]
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
    /// This frame is a long-term reference: once the viewer confirms it
    /// has it, hand this token back through [`VideoEncoder::acknowledge`]
    /// so a later loss can be repaired from it (Mac only).
    pub ltr_token: Option<i64>,
    /// This frame repairs a loss from a confirmed long-term reference
    /// (the answer to [`VideoEncoder::request_recovery`]).
    pub recovery: bool,
}

impl EncodedFrame {
    /// A plain frame (no long-term reference business).
    pub fn new(meta: EncodedFrameMeta, data: Bytes) -> Self {
        Self { meta, data, ltr_token: None, recovery: false }
    }
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
    /// The encoder chose not to output this frame (its rate control was over
    /// budget). Nothing is wrong: skip it and carry on. Treating this as a
    /// failure forced a fresh full picture, which the same rate control then
    /// had to squeeze small, so the screen went blurry (v0.4.3 on a Mac).
    #[error("encoder skipped this frame")]
    FrameSkipped,
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
    /// Show only this app or window (or everything again). Picture size may
    /// change; the pipeline follows `CapturedFrame::resolution`.
    fn set_share(&mut self, target: &ShareTarget) -> Result<()> {
        if target.is_screen() {
            Ok(())
        } else {
            Err(PlatformError::NotImplemented("sharing one app or window"))
        }
    }
    /// The largest picture the viewer wants (its Resolution setting), or
    /// `None` for full size. Backends that can scale while capturing (Mac)
    /// do it here; others leave it to the encoder.
    fn set_max_size(&mut self, _max: Option<Resolution>) {}
    /// Whether the picture comes out already scaled by [`Self::set_max_size`].
    fn scales_itself(&self) -> bool {
        false
    }
    /// Make the next [`Self::next_frame`] return a whole picture even if
    /// nothing changed. Screens only hand out pictures when something
    /// changes, so before 0.5.8 a viewer that joined a still screen saw
    /// nothing (the owner's PC sat at 0 fps watching an idle Mac) until
    /// something moved there. The host calls this when a viewer arrives and
    /// when it needs a fresh picture but has none to repeat.
    fn refresh(&mut self) {}
}

/// Turns captured frames into a codec bitstream.
pub trait VideoEncoder: Send {
    fn encode(&mut self, frame: &CapturedFrame, force_keyframe: bool) -> Result<EncodedFrame>;
    /// Change target bitrate without restarting. Called by congestion control.
    fn set_bitrate_kbps(&mut self, kbps: u32) -> Result<()>;
    /// Ask the encoder to begin an intra-refresh cycle so a viewer that lost
    /// a frame recovers within a few frames without a full keyframe spike.
    fn request_intra_refresh(&mut self) -> Result<()>;
    /// Change the frame rate the encoder plans for. `true`: done without a
    /// restart; `false`: the caller must build a new encoder at that rate.
    fn set_frame_rate(&mut self, _fps: u16) -> bool {
        false
    }
    /// Can this encoder repair a loss from a long-term reference?
    fn supports_recovery(&self) -> bool {
        false
    }
    /// The viewer confirmed it has these long-term references.
    fn acknowledge(&mut self, _tokens: &[i64]) {}
    /// Make the next frame a repair built on a confirmed long-term
    /// reference. `false`: none is confirmed yet; ask for a keyframe instead.
    fn request_recovery(&mut self) -> bool {
        false
    }
}

/// Turns a codec bitstream back into pictures.
pub trait VideoDecoder: Send {
    fn decode(&mut self, frame_id: u32, data: &Bytes) -> Result<Option<DecodedFrame>>;
}

/// Applies viewer input to the host OS.
pub trait InputInjector: Send {
    fn inject(&mut self, event: InputEvent) -> Result<()>;
    /// Keep the viewer inside one app or window (clicks elsewhere and
    /// app-switching keys are dropped; mouse positions map onto the shared
    /// window), or `None` for the whole screen.
    fn set_limit(&mut self, _limit: Option<&ShareTarget>) {}
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

/// Something on this computer that stops the other computer's mouse or
/// keys from working, in plain words for the viewer's corner display. Today:
/// Riot Vanguard (Valorant's anti-cheat) on a PC, which swallows clicks.
pub fn input_blocker() -> Option<&'static str> {
    // With Vanguard running, clicks go through a virtual USB mouse
    // (`windows::vmouse`), which needs usbip-win2. Only say something when
    // that can't work.
    #[cfg(target_os = "windows")]
    {
        (windows::vmouse::status() == windows::vmouse::NO_DRIVER && windows::vanguard_running()).then_some(
            "Valorant's anti-cheat (Riot Vanguard) is running on the PC and blocks normal clicks. To click while it \
             runs, install usbip-win2 on the PC (Anywhere app > Extras), or close Valorant and the Riot Client.",
        )
    }
    #[cfg(not(target_os = "windows"))]
    {
        None
    }
}

/// Has a game taken the mouse pointer here (hidden and locked to its
/// window)? Then the viewer switches to game mouse by itself, because a
/// game like that reads mouse movement, not pointer positions (owner,
/// 0.5.9: aiming in Valorant did nothing until game mouse, which they
/// didn't know to turn on). Cheap; asked once a second. On a Mac it was
/// always false until 2026-10-20 (Elden Ring on the phone: the stick
/// stayed a pointer and the controls never switched to the game's keys);
/// see `macos::pointer` for how a Mac tells.
pub fn cursor_captured() -> bool {
    #[cfg(target_os = "windows")]
    {
        windows::cursor_captured()
    }
    #[cfg(target_os = "macos")]
    {
        macos::pointer::captured()
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        false
    }
}

/// Phone mode (Mac host): while a phone watches, give this computer a
/// screen of the phone's shape (`points`, logical size) and make it the
/// main one, so the stream fills the phone with readable text. False where
/// that isn't possible; the stream then shows the normal screen. Takes
/// about a second (screens are rearranged): call it off the network loop.
pub fn phone_display_begin(points: (u16, u16)) -> bool {
    #[cfg(target_os = "macos")]
    {
        macos::vdisplay::begin(points)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = points;
        false
    }
}

/// The phone left: give the main screen back and remove the phone screen.
pub fn phone_display_end() {
    #[cfg(target_os = "macos")]
    macos::vdisplay::end();
}

/// The text field here that has the keyboard focus, if any: where it is
/// (fractions of the main display: x, y, width, height) and whether it is
/// a password field. The phone opens its keyboard and zooms to it (owner,
/// 0.6.0: clicking into a field left nothing to type with). Mac only for
/// now; may take up to ~100 ms when an app is busy, so call it off the
/// network loop.
pub fn focused_text_field() -> Option<([f32; 4], bool)> {
    #[cfg(target_os = "macos")]
    {
        macos::textfocus::focused_text_field()
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

/// Black out this computer's own monitor while the viewer still sees it
/// (the viewer's "screen mask" switch). Returns false where it isn't
/// available yet (a Mac host).
pub fn screen_mask(on: bool) -> bool {
    #[cfg(target_os = "windows")]
    {
        windows::privacy::set(on);
        true
    }
    #[cfg(not(target_os = "windows"))]
    {
        if on {
            tracing::info!("screen mask asked for, not available on this computer yet");
        }
        false
    }
}

/// Is the app already open? Then bring its window forward and say so, so a
/// second start (Start menu, desktop icon) doesn't open a second copy. A
/// Mac does this by itself for .app bundles.
pub fn app_already_open(title: &str) -> bool {
    #[cfg(target_os = "windows")]
    {
        windows::app_already_open(title)
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = title;
        false
    }
}

/// The main screen's refresh rate (frames per second it can show). The
/// viewer tells the host, so it never sends frames the screen can't show.
pub fn display_refresh_hz() -> Option<u16> {
    #[cfg(target_os = "macos")]
    {
        Some(macos::capture::main_display_mode().2)
    }
    #[cfg(target_os = "windows")]
    {
        windows::display_refresh_hz()
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        None
    }
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
    #[cfg(target_os = "ios")]
    {
        ios::viewer_backends()
    }
    #[cfg(target_os = "windows")]
    {
        windows::viewer_backends()
    }
    #[cfg(not(any(target_os = "macos", target_os = "ios", target_os = "windows")))]
    {
        Err(PlatformError::Unavailable("no real viewer backend for this OS; use --mock".into()))
    }
}
