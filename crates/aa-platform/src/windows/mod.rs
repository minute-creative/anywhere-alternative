//! Windows backends.
//!
//! Host:
//! * [`capture`] — DXGI Desktop Duplication. Hands out the desktop as a
//!   D3D11 texture (GPU mode, for hardware encoders) or as CPU pixels
//!   (for the software fallback).
//! * [`encoder`] — Media Foundation hardware encoder (Intel `QuickSync`,
//!   NVIDIA NVENC, AMD AMF behind one API), fed the capture texture with no
//!   CPU copy. Falls back to the software H.264 encoder if no hardware
//!   encoder opens.
//! * [`input`] — `SendInput` with scan codes for keys and absolute
//!   virtual-desktop coordinates for the mouse.
//! * `gamepad` — stage 2: `ViGEm` bus, virtual `DualShock` 4 or Xbox 360.
//!
//! Viewer: software decoder until the Media Foundation / D3D11 decoder
//! lands.

pub mod audio;
pub mod capture;
pub mod convert;
pub mod encoder;
pub mod input;

use aa_core::capability::Capabilities;
use aa_core::config::StreamConfig;
use aa_core::video::{Codec, ColorRange, Resolution};

use crate::{HostBackends, Result, ScreenCapture, VideoEncoder, ViewerBackends};

/// Which encoder the host should use. `Auto` tries hardware, then software.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EncoderChoice {
    #[default]
    Auto,
    Hardware,
    Software,
}

pub fn host_backends() -> Result<HostBackends> {
    host_backends_with(EncoderChoice::Auto)
}

pub fn host_backends_with(choice: EncoderChoice) -> Result<HostBackends> {
    // Real pixels everywhere, before DXGI or any UI call sees a scaled value.
    // SAFETY: plain Win32 call with a constant argument.
    #[allow(unsafe_code)]
    unsafe {
        let _ = windows::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(
            windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        );
    }
    // Probe the display first so we know the resolution the encoder must handle.
    let probe = capture::DxgiCapture::new(0, capture::Output::Cpu)?;
    let res = probe.resolution();
    let fps = probe.refresh_rate_hz();
    let desktop_rect = probe.desktop_rect();
    drop(probe);
    let kbps = StreamConfig::suggested_bitrate_kbps(res, fps);

    let mut codecs = vec![Codec::H264];
    let mut encoder_factory: Option<crate::EncoderFactory> = None;
    let (cap, encoder): (capture::DxgiCapture, Box<dyn VideoEncoder>) = match choice {
        EncoderChoice::Software => {
            (capture::DxgiCapture::new(0, capture::Output::Cpu)?, Box::new(crate::sw::SwEncoder::new(res, fps, kbps)?))
        }
        EncoderChoice::Hardware | EncoderChoice::Auto => {
            let cap = capture::DxgiCapture::new(0, capture::Output::Gpu)?;
            let (device, context) = cap.device();
            match open_hardware_encoder(device, context, res, fps, kbps) {
                Ok((offered, enc, factory)) => {
                    codecs = offered;
                    encoder_factory = Some(factory);
                    (cap, Box::new(enc))
                }
                Err(e) if choice == EncoderChoice::Auto => {
                    tracing::warn!("hardware encoder unavailable ({e}); falling back to software H.264");
                    drop(cap);
                    (
                        capture::DxgiCapture::new(0, capture::Output::Cpu)?,
                        Box::new(crate::sw::SwEncoder::new(res, fps, kbps)?),
                    )
                }
                Err(e) => return Err(e),
            }
        }
    };
    codecs.dedup();

    let input = input::SendInputInjector::new(desktop_rect)?;
    let audio: Option<Box<dyn crate::audio::AudioCapture>> = match audio::WasapiLoopback::new() {
        Ok(a) => {
            if a.tap() == audio::Tap::Endpoint {
                tracing::warn!("audio tap is the speaker endpoint: muting the PC's speakers will also mute the stream");
            }
            Some(Box::new(a))
        }
        Err(e) => {
            tracing::warn!("system audio capture unavailable ({e}); streaming without sound");
            None
        }
    };
    let speaker: Option<Box<dyn crate::audio::SpeakerControl>> = match audio::EndpointMute::new() {
        Ok(s) => Some(Box::new(s)),
        Err(e) => {
            tracing::warn!("speaker mute control unavailable ({e})");
            None
        }
    };
    Ok(HostBackends {
        capture: Box::new(cap),
        encoder,
        encoder_factory,
        input: Box::new(input),
        gamepad: None,
        audio,
        clipboard: crate::clipboard::system(),
        speaker,
        capabilities: Capabilities {
            codecs,
            max_resolution: res,
            max_fps: fps,
            color_ranges: vec![ColorRange::Sdr],
            has_gamepad: false,
            can_emulate_gamepad: false,
        },
    })
}

/// Opens the hardware encoder the host starts with, HEVC when it works,
/// else H.264. Returns the codecs to offer (the loaded one first), the
/// encoder, and a factory for the other codec.
///
/// HEVC: same picture at roughly half the bits, which matters most on
/// Wi-Fi. It is opened *here*, not at negotiation, so a GPU whose HEVC
/// encoder is listed but broken is caught before HEVC is offered. The
/// loaded encoder must match `codecs[0]`: the session takes that as "what is
/// running", and a mismatch means sending H.264 labelled HEVC (black screen).
fn open_hardware_encoder(
    device: &windows::Win32::Graphics::Direct3D11::ID3D11Device,
    context: &windows::Win32::Graphics::Direct3D11::ID3D11DeviceContext,
    res: Resolution,
    fps: u16,
    kbps: u32,
) -> Result<(Vec<Codec>, encoder::MfEncoder, crate::EncoderFactory)> {
    let hevc = if encoder::list_hardware_encoders(encoder::HwCodec::Hevc).is_empty() {
        None
    } else {
        encoder::MfEncoder::new(device, context, encoder::HwCodec::Hevc, res, fps, kbps)
            .inspect_err(|e| tracing::warn!("HEVC encoder is listed but would not open ({e}); offering H.264 only"))
            .ok()
    };
    let (codec, enc) = match hevc {
        Some(enc) => (Codec::Hevc, enc),
        None => (Codec::H264, encoder::MfEncoder::new(device, context, encoder::HwCodec::H264, res, fps, kbps)?),
    };
    let codecs = if codec == Codec::Hevc { vec![Codec::Hevc, Codec::H264] } else { vec![Codec::H264] };
    tracing::info!(
        ?res,
        fps,
        kbps,
        ?codec,
        offered = ?codecs,
        encoder = enc.name(),
        "windows host: DXGI capture -> hardware encoder (zero-copy)"
    );
    let (dev, ctx) = (device.clone(), context.clone());
    let factory: crate::EncoderFactory = Box::new(move |codec| {
        let hw = match codec {
            Codec::Hevc => encoder::HwCodec::Hevc,
            Codec::H264 => encoder::HwCodec::H264,
            Codec::Av1 => return Err(crate::PlatformError::Unavailable("no AV1 encoder yet".into())),
        };
        let enc = encoder::MfEncoder::new(&dev, &ctx, hw, res, fps, kbps)?;
        tracing::info!(encoder = enc.name(), ?codec, "switched hardware encoder");
        Ok(Box::new(enc) as Box<dyn VideoEncoder>)
    });
    Ok((codecs, enc, factory))
}

pub fn viewer_backends() -> Result<ViewerBackends> {
    Ok(ViewerBackends {
        decoder: Box::new(|_codec| Ok(Box::new(crate::sw::SwDecoder::new()?) as Box<dyn crate::VideoDecoder>)),
        clipboard: crate::clipboard::system(),
        capabilities: Capabilities {
            codecs: vec![Codec::H264],
            max_resolution: Resolution::new(3840, 2160),
            max_fps: aa_core::capability::MAX_FPS,
            color_ranges: vec![ColorRange::Sdr],
            has_gamepad: false,
            can_emulate_gamepad: false,
        },
    })
}

/// Hardware encoder throughput at `res`: frames per second sustained on a
/// synthetic moving picture. Returns the encoder's name with the number.
/// Resolution of the main screen, for the bench.
pub fn screen_resolution() -> Result<Resolution> {
    Ok(capture::DxgiCapture::new(0, capture::Output::Cpu)?.resolution())
}

pub fn bench_hardware(codec: Codec, res: Resolution, frames: u32) -> Result<(String, f64)> {
    use std::time::Instant;
    let hw = match codec {
        Codec::Hevc => encoder::HwCodec::Hevc,
        Codec::H264 => encoder::HwCodec::H264,
        Codec::Av1 => return Err(crate::PlatformError::Unavailable("no AV1 encoder yet".into())),
    };
    // Any D3D11 device will do for a bench; reuse the capture's.
    let cap = capture::DxgiCapture::new(0, capture::Output::Gpu)?;
    let (device, context) = cap.device();
    let mut enc = encoder::MfEncoder::new(device, context, hw, res, 120, 40_000)?;
    let name = enc.name().to_string();
    // Two different pictures, alternated: every frame changes completely,
    // which is the worst case (fast camera pan), not a still desktop.
    let a = bench_frame(device, context, res, 0)?;
    let b = bench_frame(device, context, res, 97)?;
    for i in 0..10 {
        enc.encode(if i % 2 == 0 { &a } else { &b }, i == 0)?; // warm-up
    }
    let start = Instant::now();
    for i in 0..frames {
        enc.encode(if i % 2 == 0 { &a } else { &b }, false)?;
    }
    Ok((name, f64::from(frames) / start.elapsed().as_secs_f64()))
}

/// A GPU texture filled with a gradient, wrapped as a captured frame.
fn bench_frame(
    device: &windows::Win32::Graphics::Direct3D11::ID3D11Device,
    _context: &windows::Win32::Graphics::Direct3D11::ID3D11DeviceContext,
    res: Resolution,
    shift: usize,
) -> Result<crate::CapturedFrame> {
    use windows::core::Interface;
    use windows::Win32::Graphics::Direct3D11::{
        D3D11_BIND_SHADER_RESOURCE, D3D11_SUBRESOURCE_DATA, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
    };
    use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};

    let width = res.width as usize;
    let height = res.height as usize;
    let mut px = vec![0u8; width * height * 4];
    for row in 0..height {
        for col in 0..width {
            let at = (row * width + col) * 4;
            let (gx, gy) = (col + shift, row + shift * 3);
            px[at] = gx as u8;
            px[at + 1] = gy as u8;
            px[at + 2] = (gx ^ gy) as u8;
            px[at + 3] = 255;
        }
    }
    let desc = D3D11_TEXTURE2D_DESC {
        Width: res.width,
        Height: res.height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let init =
        D3D11_SUBRESOURCE_DATA { pSysMem: px.as_ptr().cast(), SysMemPitch: (width * 4) as u32, SysMemSlicePitch: 0 };
    let mut tex = None;
    // SAFETY: desc and init data are valid for the call's duration.
    #[allow(unsafe_code)]
    unsafe { device.CreateTexture2D(&desc, Some(&init), Some(&mut tex)) }
        .map_err(|e| crate::PlatformError::Backend(anyhow::anyhow!("CreateTexture2D(bench): {e}")))?;
    let tex = tex.ok_or_else(|| crate::PlatformError::Backend(anyhow::anyhow!("no bench texture")))?;
    // Leak the texture on purpose: the frame's raw handle must stay valid for
    // the bench's lifetime, and the bench exits right after.
    let handle = tex.as_raw() as usize;
    std::mem::forget(tex);
    Ok(crate::CapturedFrame {
        buffer: crate::FrameBuffer::Gpu { api: crate::GpuApi::D3D11, handle },
        format: aa_core::video::PixelFormat::Bgra8,
        resolution: res,
        capture_ts_us: 0,
    })
}
