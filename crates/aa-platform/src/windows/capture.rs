//! DXGI Desktop Duplication capture.
//!
//! Lifecycle: create a D3D11 device on the adapter that owns the output,
//! `DuplicateOutput` it, then loop `AcquireNextFrame` → copy to a staging
//! texture → `ReleaseFrame` (as early as possible, so the compositor isn't
//! blocked) → `Map` and read the pixels.
//!
//! Desktop Duplication only delivers a frame when the desktop changed, so
//! a static screen costs nothing. It also stops delivering frames
//! (`DXGI_ERROR_ACCESS_LOST`) on mode changes, UAC prompts and lock screens;
//! we recreate the duplication and carry on.

// FFI code: the pedantic cast/pointer lints add noise here, not safety.
#![allow(clippy::pedantic)]

use std::time::{Duration, Instant};

use aa_core::video::{PixelFormat, Resolution};
use bytes::BytesMut;
use windows::core::Interface;
use windows::Win32::Foundation::{HMODULE, RECT};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_CPU_ACCESS_READ,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_SDK_VERSION,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory1, IDXGIOutput1, IDXGIOutputDuplication, IDXGIResource,
    DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO,
};

use crate::{CapturedFrame, FrameBuffer, PlatformError, Result, ScreenCapture};

pub struct DxgiCapture {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    adapter: IDXGIAdapter1,
    output_index: u32,
    dup: Option<IDXGIOutputDuplication>,
    staging: ID3D11Texture2D,
    res: Resolution,
    refresh_hz: u16,
    desktop_rect: RECT,
    started: Instant,
}

// SAFETY: the D3D11 device is created single-threaded-safe by default and
// every COM object here is only touched from the capture thread.
unsafe impl Send for DxgiCapture {}

impl std::fmt::Debug for DxgiCapture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DxgiCapture")
            .field("res", &self.res)
            .field("refresh_hz", &self.refresh_hz)
            .finish_non_exhaustive()
    }
}

fn win(e: windows::core::Error, what: &str) -> PlatformError {
    PlatformError::Backend(anyhow::anyhow!("{what}: {e}"))
}

impl DxgiCapture {
    /// Capture output `output_index` of the first adapter (0 = primary display).
    pub fn new(output_index: u32) -> Result<Self> {
        // SAFETY: plain COM creation calls with valid out-pointers.
        unsafe {
            let factory: IDXGIFactory1 = CreateDXGIFactory1().map_err(|e| win(e, "CreateDXGIFactory1"))?;
            let adapter = factory.EnumAdapters1(0).map_err(|e| win(e, "EnumAdapters1(0)"))?;
            let output = adapter.EnumOutputs(output_index).map_err(|e| win(e, "EnumOutputs"))?;
            let output1: IDXGIOutput1 = output.cast().map_err(|e| win(e, "IDXGIOutput1"))?;
            let out_desc = output.GetDesc().map_err(|e| win(e, "output GetDesc"))?;

            let mut device = None;
            let mut context = None;
            D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
            .map_err(|e| win(e, "D3D11CreateDevice"))?;
            let device = device.ok_or_else(|| PlatformError::Unavailable("no D3D11 device".into()))?;
            let context = context.ok_or_else(|| PlatformError::Unavailable("no D3D11 context".into()))?;

            let dup = output1.DuplicateOutput(&device).map_err(|e| {
                if e.code().0 as u32 == 0x8007_0005 {
                    PlatformError::Permission(
                        "desktop duplication denied (another app owns it, or a secure desktop is up)".into(),
                    )
                } else {
                    win(e, "DuplicateOutput")
                }
            })?;
            let desc = dup.GetDesc();
            let res = Resolution::new(desc.ModeDesc.Width, desc.ModeDesc.Height);
            let rr = desc.ModeDesc.RefreshRate;
            let refresh_hz =
                if rr.Denominator == 0 { 60 } else { (rr.Numerator / rr.Denominator).clamp(1, 1000) as u16 };

            let staging = Self::make_staging(&device, res)?;
            tracing::info!(?res, refresh_hz, "desktop duplication ready");
            Ok(Self {
                device,
                context,
                adapter,
                output_index,
                dup: Some(dup),
                staging,
                res,
                refresh_hz,
                desktop_rect: out_desc.DesktopCoordinates,
                started: Instant::now(),
            })
        }
    }

    /// Where this output sits on the virtual desktop; the input injector
    /// needs it to turn stream coordinates into screen coordinates.
    pub fn desktop_rect(&self) -> RECT {
        self.desktop_rect
    }

    fn make_staging(device: &ID3D11Device, res: Resolution) -> Result<ID3D11Texture2D> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: res.width,
            Height: res.height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };
        let mut tex = None;
        // SAFETY: desc is fully initialised; out-pointer is valid.
        unsafe { device.CreateTexture2D(&desc, None, Some(&mut tex)) }.map_err(|e| win(e, "CreateTexture2D"))?;
        tex.ok_or_else(|| PlatformError::Backend(anyhow::anyhow!("CreateTexture2D returned nothing")))
    }

    /// Recreate the duplication after `DXGI_ERROR_ACCESS_LOST`. The desktop
    /// mode may have changed, so resolution is re-read too.
    fn reacquire(&mut self) -> Result<()> {
        self.dup = None;
        // SAFETY: COM calls on live objects.
        unsafe {
            let output = self.adapter.EnumOutputs(self.output_index).map_err(|e| win(e, "EnumOutputs"))?;
            let output1: IDXGIOutput1 = output.cast().map_err(|e| win(e, "IDXGIOutput1"))?;
            self.desktop_rect = output.GetDesc().map_err(|e| win(e, "output GetDesc"))?.DesktopCoordinates;
            let dup = output1.DuplicateOutput(&self.device).map_err(|e| win(e, "DuplicateOutput"))?;
            let desc = dup.GetDesc();
            let res = Resolution::new(desc.ModeDesc.Width, desc.ModeDesc.Height);
            if res != self.res {
                tracing::info!(old = ?self.res, new = ?res, "display mode changed");
                self.res = res;
                self.staging = Self::make_staging(&self.device, res)?;
            }
            self.dup = Some(dup);
        }
        Ok(())
    }
}

impl ScreenCapture for DxgiCapture {
    fn next_frame(&mut self, timeout: Duration) -> Result<Option<CapturedFrame>> {
        let Some(dup) = self.dup.as_ref() else {
            // Lost earlier; try to come back, and give the caller a beat if we can't.
            if let Err(e) = self.reacquire() {
                tracing::debug!("reacquire failed: {e}");
                std::thread::sleep(Duration::from_millis(250));
            }
            return Ok(None);
        };

        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;
        // SAFETY: valid out-pointers; `dup` is a live duplication.
        let acquired = unsafe { dup.AcquireNextFrame(timeout.as_millis() as u32, &mut info, &mut resource) };
        match acquired {
            Ok(()) => {}
            Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(None),
            Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST => {
                tracing::info!("desktop duplication access lost; reacquiring");
                self.dup = None;
                return Ok(None);
            }
            Err(e) => return Err(win(e, "AcquireNextFrame")),
        }

        // A frame with no new desktop image means only the mouse moved.
        if info.LastPresentTime == 0 {
            // SAFETY: we acquired above and must release exactly once.
            unsafe { dup.ReleaseFrame() }.map_err(|e| win(e, "ReleaseFrame"))?;
            return Ok(None);
        }

        let Some(resource) = resource else {
            // SAFETY: as above.
            unsafe { dup.ReleaseFrame() }.map_err(|e| win(e, "ReleaseFrame"))?;
            return Ok(None);
        };

        let capture_ts_us = self.started.elapsed().as_micros() as u64;
        let res = self.res;

        // SAFETY: the resource is a 2D texture by contract of Desktop
        // Duplication; copy on the GPU, release the frame, then map the
        // staging copy which we own.
        let result: Result<BytesMut> = unsafe {
            let tex: ID3D11Texture2D = resource.cast().map_err(|e| win(e, "ID3D11Texture2D"))?;
            self.context.CopyResource(&self.staging, &tex);
            drop(tex);
            drop(resource);
            dup.ReleaseFrame().map_err(|e| win(e, "ReleaseFrame"))?;

            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.context.Map(&self.staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped)).map_err(|e| win(e, "Map"))?;
            let row_bytes = res.width as usize * 4;
            let mut px = BytesMut::with_capacity(row_bytes * res.height as usize);
            let base = mapped.pData.cast::<u8>();
            for y in 0..res.height as usize {
                let row = std::slice::from_raw_parts(base.add(y * mapped.RowPitch as usize), row_bytes);
                px.extend_from_slice(row);
            }
            self.context.Unmap(&self.staging, 0);
            Ok(px)
        };
        let px = result?;

        Ok(Some(CapturedFrame {
            buffer: FrameBuffer::Cpu(px.freeze()),
            format: PixelFormat::Bgra8,
            resolution: res,
            capture_ts_us,
        }))
    }

    fn resolution(&self) -> Resolution {
        self.res
    }

    fn refresh_rate_hz(&self) -> u16 {
        self.refresh_hz
    }
}
