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
use windows::Win32::Media::Audio::{
    eConsole, eRender, IAudioCaptureClient, IAudioClient, IMMDeviceEnumerator, MMDeviceEnumerator,
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
    AUDCLNT_STREAMFLAGS_LOOPBACK, WAVEFORMATEX, WAVEFORMATEXTENSIBLE,
};
use windows::Win32::Media::KernelStreaming::WAVE_FORMAT_EXTENSIBLE;
use windows::Win32::Media::Multimedia::{KSDATAFORMAT_SUBTYPE_IEEE_FLOAT, WAVE_FORMAT_IEEE_FLOAT};
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_ALL, COINIT_MULTITHREADED};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

use crate::audio::{AudioCapture, FRAME_LEN_I16};
use crate::{PlatformError, Result};

fn win(e: windows::core::Error, what: &str) -> PlatformError {
    PlatformError::Backend(anyhow::anyhow!("{what}: {e}"))
}

pub struct WasapiLoopback {
    client: IAudioClient,
    capture: IAudioCaptureClient,
    event: windows::Win32::Foundation::HANDLE,
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
            .field("src_rate", &self.src_rate)
            .field("src_channels", &self.src_channels)
            .finish_non_exhaustive()
    }
}

impl WasapiLoopback {
    pub fn new() -> Result<Self> {
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
            let (src_rate, src_channels, bits) = (fmt.nSamplesPerSec, fmt.nChannels, fmt.wBitsPerSample);

            // 20 ms buffer; event-driven so we wake exactly when data lands.
            let hns_20ms = 200_000;
            client
                .Initialize(
                    AUDCLNT_SHAREMODE_SHARED,
                    AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
                    hns_20ms,
                    0,
                    fmt_ptr,
                    None,
                )
                .map_err(|e| win(e, "IAudioClient::Initialize(loopback)"))?;
            CoTaskMemFree(Some(fmt_ptr.cast()));

            let event = CreateEventW(None, false, false, None).map_err(|e| win(e, "CreateEventW"))?;
            client.SetEventHandle(event).map_err(|e| win(e, "SetEventHandle"))?;
            let capture: IAudioCaptureClient = client.GetService().map_err(|e| win(e, "IAudioCaptureClient"))?;
            client.Start().map_err(|e| win(e, "IAudioClient::Start"))?;
            tracing::info!(src_rate, src_channels, bits, is_float, "wasapi loopback capture ready");

            Ok(Self {
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

    /// Pull everything WASAPI has and append it to `pending` as 48 kHz stereo i16.
    unsafe fn drain(&mut self) -> Result<bool> {
        let mut any_sound = false;
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
