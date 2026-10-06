//! System-audio capture through WASAPI loopback: whatever the PC is playing
//! comes back to us as PCM, with no virtual cable or driver.
//!
//! We let Windows pick the mix format (always 32-bit float at the device
//! rate, typically 48 kHz stereo) and convert to the 48 kHz stereo i16 the
//! encoder wants. If the device runs at another rate (44.1 kHz is common)
//! we resample linearly; it is a 10 ms chunk of game audio, not mastering.

// FFI code: `unsafe` is the point here, each block carries a SAFETY note;
// the pedantic cast/pointer lints add noise, not safety.
#![allow(unsafe_code, clippy::pedantic)]

use std::collections::VecDeque;

use aa_core::audio::{CHANNELS, FRAME_SAMPLES, SAMPLE_RATE};
use windows::core::Interface;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Media::Audio::{
    eConsole, eRender, ActivateAudioInterfaceAsync, IActivateAudioInterfaceAsyncOperation,
    IActivateAudioInterfaceCompletionHandler, IActivateAudioInterfaceCompletionHandler_Impl, IAudioCaptureClient,
    IAudioClient, IMMDeviceEnumerator, MMDeviceEnumerator, AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED,
    AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_LOOPBACK, AUDIOCLIENT_ACTIVATION_PARAMS,
    AUDIOCLIENT_ACTIVATION_PARAMS_0, AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK, AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS,
    PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE, VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK, WAVEFORMATEX,
    WAVEFORMATEXTENSIBLE,
};
use windows::Win32::Media::KernelStreaming::WAVE_FORMAT_EXTENSIBLE;
use windows::Win32::Media::Multimedia::{KSDATAFORMAT_SUBTYPE_IEEE_FLOAT, WAVE_FORMAT_IEEE_FLOAT};
use windows::Win32::System::Com::StructuredStorage::{PROPVARIANT, PROPVARIANT_0, PROPVARIANT_0_0, PROPVARIANT_0_0_0};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, BLOB, CLSCTX_ALL, COINIT_MULTITHREADED,
};
use windows::Win32::System::Threading::{CreateEventW, GetCurrentProcessId, SetEvent, WaitForSingleObject};
use windows::Win32::System::Variant::VT_BLOB;

use crate::audio::{AudioCapture, FRAME_LEN_I16};
use crate::{PlatformError, Result};

fn win(e: windows::core::Error, what: &str) -> PlatformError {
    PlatformError::Backend(anyhow::anyhow!("{what}: {e}"))
}

pub struct WasapiLoopback {
    tap: Tap,
    client: IAudioClient,
    capture: IAudioCaptureClient,
    event: HANDLE,
    src_rate: u32,
    src_channels: u16,
    is_float: bool,
    bits: u16,
    /// Interleaved stereo i16 at 48 kHz, waiting to be cut into frames.
    pending: VecDeque<i16>,
    /// Resampler phase carried across chunks.
    resample_pos: f64,
    silent_frames: u32,
}

// SAFETY: used from the audio thread only.
unsafe impl Send for WasapiLoopback {}

impl std::fmt::Debug for WasapiLoopback {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WasapiLoopback")
            .field("tap", &self.tap)
            .field("src_rate", &self.src_rate)
            .field("src_channels", &self.src_channels)
            .finish_non_exhaustive()
    }
}

/// Signals an event when `ActivateAudioInterfaceAsync` finishes, so we can
/// wait for it synchronously (we have no message loop to be called back on).
#[windows::core::implement(IActivateAudioInterfaceCompletionHandler)]
struct ActivationDone(HANDLE);

impl IActivateAudioInterfaceCompletionHandler_Impl for ActivationDone_Impl {
    fn ActivateCompleted(
        &self,
        _op: windows::core::Ref<'_, IActivateAudioInterfaceAsyncOperation>,
    ) -> windows::core::Result<()> {
        // SAFETY: the event handle outlives the activation (we wait on it).
        unsafe { SetEvent(self.0) }
    }
}

/// Where the audio is tapped. Which one is in use decides whether muting
/// the PC's speakers also mutes the stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tap {
    /// Per-process loopback (Windows 10 2004+): every app's audio *before*
    /// the speaker volume/mute is applied. Muting the PC leaves the stream intact.
    Process,
    /// Classic endpoint loopback: the mix *as sent to the speakers*. On
    /// devices with software volume, muting the PC silences the stream too.
    Endpoint,
}

impl WasapiLoopback {
    /// Prefer the per-process tap; fall back to the endpoint tap on older
    /// Windows or if activation fails for any reason.
    pub fn new() -> Result<Self> {
        match Self::process_loopback() {
            Ok(s) => Ok(s),
            Err(e) => {
                tracing::warn!(
                    "process loopback unavailable ({e}); using endpoint loopback (muting the PC will mute the stream)"
                );
                Self::endpoint_loopback()
            }
        }
    }

    /// Everything-but-us process loopback: Windows mixes all other apps for
    /// us at the format we ask for, independent of the speaker device.
    pub fn process_loopback() -> Result<Self> {
        // SAFETY: the activation params and PROPVARIANT live on this stack
        // frame for the whole synchronous wait; handles come from successful calls.
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            let mut params = AUDIOCLIENT_ACTIVATION_PARAMS {
                ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
                Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
                    ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                        // "Everything except our own process tree": we never
                        // want to re-stream the viewer's own sound if it ever
                        // runs on the same machine.
                        TargetProcessId: GetCurrentProcessId(),
                        ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE,
                    },
                },
            };
            // PROPVARIANT's inner struct sits behind ManuallyDrop in a
            // union; build it whole rather than poking fields through it.
            let pv = PROPVARIANT {
                Anonymous: PROPVARIANT_0 {
                    Anonymous: std::mem::ManuallyDrop::new(PROPVARIANT_0_0 {
                        vt: VT_BLOB,
                        wReserved1: 0,
                        wReserved2: 0,
                        wReserved3: 0,
                        Anonymous: PROPVARIANT_0_0_0 {
                            blob: BLOB {
                                cbSize: std::mem::size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
                                pBlobData: std::ptr::addr_of_mut!(params).cast::<u8>(),
                            },
                        },
                    }),
                },
            };

            let done = CreateEventW(None, false, false, None).map_err(|e| win(e, "CreateEventW"))?;
            let handler: IActivateAudioInterfaceCompletionHandler = ActivationDone(done).into();
            let op = ActivateAudioInterfaceAsync(
                VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
                &IAudioClient::IID,
                Some(&raw const pv),
                &handler,
            )
            .map_err(|e| win(e, "ActivateAudioInterfaceAsync"))?;
            let _ = WaitForSingleObject(done, 2_000);
            let _ = windows::Win32::Foundation::CloseHandle(done);

            let mut hr = windows::core::HRESULT(0);
            let mut unknown: Option<windows::core::IUnknown> = None;
            op.GetActivateResult(&mut hr, &mut unknown).map_err(|e| win(e, "GetActivateResult"))?;
            hr.ok().map_err(|e| win(e, "process loopback activation"))?;
            let client: IAudioClient = unknown
                .ok_or_else(|| PlatformError::Backend(anyhow::anyhow!("activation returned no interface")))?
                .cast()
                .map_err(|e| win(e, "IAudioClient cast"))?;

            // Process loopback has no "mix format": we name the one we want,
            // which is exactly what the encoder wants, so no resampling.
            let fmt = WAVEFORMATEX {
                wFormatTag: WAVE_FORMAT_IEEE_FLOAT as u16,
                nChannels: CHANNELS,
                nSamplesPerSec: SAMPLE_RATE,
                nAvgBytesPerSec: SAMPLE_RATE * u32::from(CHANNELS) * 4,
                nBlockAlign: CHANNELS * 4,
                wBitsPerSample: 32,
                cbSize: 0,
            };
            Self::finish(client, &fmt, true, Tap::Process)
        }
    }

    /// Classic loopback on the default render device.
    pub fn endpoint_loopback() -> Result<Self> {
        // SAFETY: standard WASAPI setup sequence; every pointer comes from a
        // successful call just before it is used.
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            let enumerator: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).map_err(|e| win(e, "MMDeviceEnumerator"))?;
            let device =
                enumerator.GetDefaultAudioEndpoint(eRender, eConsole).map_err(|e| win(e, "default render device"))?;
            let client: IAudioClient = device.Activate(CLSCTX_ALL, None).map_err(|e| win(e, "IAudioClient"))?;

            let fmt_ptr = client.GetMixFormat().map_err(|e| win(e, "GetMixFormat"))?;
            let fmt: WAVEFORMATEX = *fmt_ptr;
            let mut is_float = fmt.wFormatTag == WAVE_FORMAT_IEEE_FLOAT as u16;
            if fmt.wFormatTag == WAVE_FORMAT_EXTENSIBLE as u16 {
                // Packed struct: copy the field out rather than borrow it.
                let ext: WAVEFORMATEXTENSIBLE = std::ptr::read_unaligned(fmt_ptr as *const WAVEFORMATEXTENSIBLE);
                let sub = ext.SubFormat;
                is_float = sub == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT;
            }
            let result = Self::finish(client, fmt_ptr.as_ref().expect("mix format"), is_float, Tap::Endpoint);
            CoTaskMemFree(Some(fmt_ptr.cast()));
            result
        }
    }

    /// Shared tail: initialise the client in event-driven loopback mode and
    /// start capturing.
    unsafe fn finish(client: IAudioClient, fmt: &WAVEFORMATEX, is_float: bool, tap: Tap) -> Result<Self> {
        // SAFETY: caller guarantees `fmt` is valid for the call; the rest are
        // live COM objects from this function.
        unsafe {
            let (src_rate, src_channels, bits) = (fmt.nSamplesPerSec, fmt.nChannels, fmt.wBitsPerSample);
            // 20 ms buffer; event-driven so we wake exactly when data lands.
            let hns_20ms = 200_000;
            client
                .Initialize(
                    AUDCLNT_SHAREMODE_SHARED,
                    AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
                    hns_20ms,
                    0,
                    std::ptr::from_ref(fmt),
                    None,
                )
                .map_err(|e| win(e, "IAudioClient::Initialize(loopback)"))?;

            let event = CreateEventW(None, false, false, None).map_err(|e| win(e, "CreateEventW"))?;
            client.SetEventHandle(event).map_err(|e| win(e, "SetEventHandle"))?;
            let capture: IAudioCaptureClient = client.GetService().map_err(|e| win(e, "IAudioCaptureClient"))?;
            client.Start().map_err(|e| win(e, "IAudioClient::Start"))?;
            tracing::info!(?tap, src_rate, src_channels, bits, is_float, "wasapi loopback capture ready");

            Ok(Self {
                tap,
                client,
                capture,
                event,
                src_rate,
                src_channels,
                is_float,
                bits,
                pending: VecDeque::with_capacity(FRAME_LEN_I16 * 8),
                resample_pos: 0.0,
                silent_frames: 0,
            })
        }
    }

    pub fn tap(&self) -> Tap {
        self.tap
    }

    /// Pull everything WASAPI has and append it to `pending` as 48 kHz stereo i16.
    fn drain(&mut self) -> Result<bool> {
        let mut any_sound = false;
        // SAFETY: `capture` is a live IAudioCaptureClient; the buffer pointer
        // and frame count come from GetBuffer and are valid until ReleaseBuffer.
        unsafe {
            loop {
                let frames = self.capture.GetNextPacketSize().map_err(|e| win(e, "GetNextPacketSize"))?;
                if frames == 0 {
                    break;
                }
                let mut data: *mut u8 = std::ptr::null_mut();
                let mut got = 0u32;
                let mut flags = 0u32;
                self.capture.GetBuffer(&mut data, &mut got, &mut flags, None, None).map_err(|e| win(e, "GetBuffer"))?;
                let silent = flags as i32 & AUDCLNT_BUFFERFLAGS_SILENT.0 != 0;
                let n = got as usize * self.src_channels as usize;

                // Convert to stereo f32 at the source rate first.
                let mut stereo: Vec<(f32, f32)> = Vec::with_capacity(got as usize);
                if silent || data.is_null() {
                    stereo.resize(got as usize, (0.0, 0.0));
                } else {
                    any_sound = true;
                    let ch = self.src_channels as usize;
                    if self.is_float && self.bits == 32 {
                        let s = std::slice::from_raw_parts(data as *const f32, n);
                        for f in s.chunks_exact(ch) {
                            stereo.push(downmix(f));
                        }
                    } else if self.bits == 16 {
                        let s = std::slice::from_raw_parts(data as *const i16, n);
                        for f in s.chunks_exact(ch) {
                            let v: Vec<f32> = f.iter().map(|&x| f32::from(x) / 32768.0).collect();
                            stereo.push(downmix(&v));
                        }
                    } else {
                        stereo.resize(got as usize, (0.0, 0.0));
                    }
                }
                self.capture.ReleaseBuffer(got).map_err(|e| win(e, "ReleaseBuffer"))?;

                // Resample to 48 kHz if needed, then to i16.
                if self.src_rate == SAMPLE_RATE {
                    for (l, r) in stereo {
                        self.pending.push_back(to_i16(l));
                        self.pending.push_back(to_i16(r));
                    }
                } else {
                    let step = f64::from(self.src_rate) / f64::from(SAMPLE_RATE);
                    let mut pos = self.resample_pos;
                    while (pos as usize) + 1 < stereo.len() {
                        let i = pos as usize;
                        let t = (pos - i as f64) as f32;
                        let (l0, r0) = stereo[i];
                        let (l1, r1) = stereo[i + 1];
                        self.pending.push_back(to_i16(l0 + (l1 - l0) * t));
                        self.pending.push_back(to_i16(r0 + (r1 - r0) * t));
                        pos += step;
                    }
                    self.resample_pos = pos - (stereo.len().saturating_sub(1)) as f64;
                    if self.resample_pos < 0.0 {
                        self.resample_pos = 0.0;
                    }
                }
            }
        }
        Ok(any_sound)
    }
}

fn downmix(frame: &[f32]) -> (f32, f32) {
    match frame.len() {
        0 => (0.0, 0.0),
        1 => (frame[0], frame[0]),
        2 => (frame[0], frame[1]),
        // 5.1/7.1: front L/R plus a bit of centre; surrounds dropped.
        _ => (frame[0] + frame[2] * 0.7, frame[1] + frame[2] * 0.7),
    }
}

fn to_i16(v: f32) -> i16 {
    (v.clamp(-1.0, 1.0) * 32767.0) as i16
}

impl AudioCapture for WasapiLoopback {
    fn next_frame(&mut self, pcm: &mut [i16]) -> Result<bool> {
        debug_assert_eq!(pcm.len(), FRAME_LEN_I16);
        // SAFETY: event handle and clients are live for self's lifetime.
        unsafe {
            while self.pending.len() < FRAME_LEN_I16 {
                // Wake when WASAPI has data; 100 ms cap so a stopped device
                // doesn't hang the audio thread.
                let _ = WaitForSingleObject(self.event, 100);
                let sound = self.drain()?;
                if sound {
                    self.silent_frames = 0;
                } else if self.pending.is_empty() {
                    // Nothing is playing. Report silence without blocking
                    // forever; the host sends nothing for silent frames.
                    self.silent_frames = self.silent_frames.saturating_add(1);
                    pcm.fill(0);
                    return Ok(false);
                }
            }
        }
        for s in pcm.iter_mut() {
            *s = self.pending.pop_front().unwrap_or(0);
        }
        Ok(true)
    }
}

impl Drop for WasapiLoopback {
    fn drop(&mut self) {
        // SAFETY: stopping a live client; teardown errors are not actionable.
        unsafe {
            let _ = self.client.Stop();
            let _ = windows::Win32::Foundation::CloseHandle(self.event);
        }
        let _ = FRAME_SAMPLES;
        let _ = CHANNELS;
    }
}

/// Mutes the default output device via `IAudioEndpointVolume`. Why the
/// endpoint rather than our own process: the sound we want to silence
/// belongs to the game or browser, not to us.
pub struct EndpointMute {
    volume: windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume,
    /// Mute state before we touched anything; restored on `restore`/drop.
    original: Option<bool>,
}

// SAFETY: COM interface used from the session task only; windows-rs
// interfaces are already Send, this mirrors `WasapiLoopback`.
unsafe impl Send for EndpointMute {}

impl std::fmt::Debug for EndpointMute {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EndpointMute").field("original", &self.original).finish_non_exhaustive()
    }
}

impl EndpointMute {
    pub fn new() -> Result<Self> {
        // SAFETY: standard MMDevice activation; pointers come from successful calls.
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            let enumerator: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).map_err(|e| win(e, "MMDeviceEnumerator"))?;
            let device =
                enumerator.GetDefaultAudioEndpoint(eRender, eConsole).map_err(|e| win(e, "default render device"))?;
            let volume = device.Activate(CLSCTX_ALL, None).map_err(|e| win(e, "IAudioEndpointVolume"))?;
            Ok(Self { volume, original: None })
        }
    }
}

impl crate::audio::SpeakerControl for EndpointMute {
    fn set_muted(&mut self, muted: bool) -> Result<()> {
        // SAFETY: live endpoint-volume interface; null event context is allowed.
        unsafe {
            if self.original.is_none() {
                let was = self.volume.GetMute().map_err(|e| win(e, "GetMute"))?;
                self.original = Some(was.as_bool());
            }
            self.volume.SetMute(muted, std::ptr::null()).map_err(|e| win(e, "SetMute"))?;
        }
        tracing::info!(muted, "host speakers");
        Ok(())
    }

    fn restore(&mut self) -> Result<()> {
        if let Some(was) = self.original.take() {
            // SAFETY: as above.
            unsafe {
                self.volume.SetMute(was, std::ptr::null()).map_err(|e| win(e, "SetMute(restore)"))?;
            }
            tracing::info!(muted = was, "host speakers restored");
        }
        Ok(())
    }
}

impl Drop for EndpointMute {
    fn drop(&mut self) {
        let _ = crate::audio::SpeakerControl::restore(self);
    }
}
