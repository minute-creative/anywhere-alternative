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

// FFI code: `unsafe` is the point here, each block carries a SAFETY note;
// the pedantic cast/pointer lints add noise, not safety.
#![allow(unsafe_code, clippy::pedantic)]

use std::time::{Duration, Instant};

use aa_core::video::{PixelFormat, Resolution};
use bytes::BytesMut;
use windows::core::Interface;
use windows::Win32::Foundation::{E_ACCESSDENIED, HMODULE, RECT};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_BIND_SHADER_RESOURCE,
    D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
    D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
    D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory1, IDXGIOutput, IDXGIOutput1, IDXGIOutputDuplication, IDXGIResource,
    DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_DEVICE_REMOVED, DXGI_ERROR_DEVICE_RESET, DXGI_ERROR_WAIT_TIMEOUT,
    DXGI_OUTDUPL_FRAME_INFO,
};

use crate::{CapturedFrame, FrameBuffer, GpuApi, PlatformError, Result, ScreenCapture};

/// Where captured frames should end up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Output {
    /// Read pixels back to the CPU (for the software encoder). Slow at 4K.
    Cpu,
    /// Copy into a GPU texture we own and hand out its pointer; the hardware
    /// encoder copies from it. Zero CPU involvement.
    Gpu,
}

pub struct DxgiCapture {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    /// GPU-mode destination; alive as long as `self`, so the raw pointer we
    /// hand out in `FrameBuffer::Gpu` stays valid until the next frame.
    gpu_tex: Option<ID3D11Texture2D>,
    adapter: IDXGIAdapter1,
    output_index: u32,
    dup: Option<IDXGIOutputDuplication>,
    staging: ID3D11Texture2D,
    res: Resolution,
    refresh_hz: u16,
    desktop_rect: RECT,
    started: Instant,
    /// Windows refused to show us the screen: lock screen, UAC prompt or
    /// Ctrl+Alt+Del (the "secure desktop", off limits to normal programs).
    locked: bool,
    /// No display is on (laptop lid closed with nothing else attached,
    /// monitor powered off).
    no_display: bool,
    /// When re-grabbing the screen started failing for reasons other than
    /// the two above; past a limit we ask for a full rebuild.
    failing_since: Option<Instant>,
}

/// Where the captured screen sits on the virtual desktop, shared with the
/// input injector so clicks land right after a resolution change.
static OUTPUT_RECT: std::sync::Mutex<Option<RECT>> = std::sync::Mutex::new(None);

pub fn current_output_rect() -> Option<RECT> {
    *OUTPUT_RECT.lock().expect("output rect")
}

fn publish_rect(r: RECT) {
    *OUTPUT_RECT.lock().expect("output rect") = Some(r);
}

/// Re-grabbing that fails this long (not locked, a display is on) means
/// this GPU device is beyond saving: ask for a full rebuild.
const GIVE_UP_AFTER: Duration = Duration::from_secs(15);

/// The output to capture: `preferred` if it shows a desktop, else the first
/// that does (lid closed onto an external monitor, monitor swapped).
///
/// SAFETY: plain COM enumeration on a live adapter.
unsafe fn pick_output(adapter: &IDXGIAdapter1, preferred: u32) -> Option<(u32, IDXGIOutput)> {
    let order = std::iter::once(preferred).chain((0..8).filter(|&i| i != preferred));
    for i in order {
        // SAFETY: see function contract.
        let Ok(o) = (unsafe { adapter.EnumOutputs(i) }) else { continue };
        // SAFETY: as above.
        if unsafe { o.GetDesc() }.is_ok_and(|d| d.AttachedToDesktop.as_bool()) {
            return Some((i, o));
        }
    }
    None
}

fn refresh_of(desc: &windows::Win32::Graphics::Dxgi::DXGI_OUTDUPL_DESC) -> u16 {
    let rr = desc.ModeDesc.RefreshRate;
    // Rounded, so 59.94 Hz (60000/1001) reads as 60, and 143.9 as 144.
    (rr.Numerator + rr.Denominator / 2).checked_div(rr.Denominator).map_or(60, |hz| hz.clamp(1, 1000) as u16)
}

fn is_device_gone(code: windows::core::HRESULT) -> bool {
    code == DXGI_ERROR_DEVICE_REMOVED || code == DXGI_ERROR_DEVICE_RESET
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
    pub fn new(output_index: u32, output_mode: Output) -> Result<Self> {
        // SAFETY: plain COM creation calls with valid out-pointers.
        unsafe {
            let factory: IDXGIFactory1 = CreateDXGIFactory1().map_err(|e| win(e, "CreateDXGIFactory1"))?;
            let adapter = factory.EnumAdapters1(0).map_err(|e| win(e, "EnumAdapters1(0)"))?;
            let (output_index, output) = pick_output(&adapter, output_index)
                .ok_or_else(|| PlatformError::DeviceLost("no display is on (lid closed or monitor off?)".into()))?;
            let output1: IDXGIOutput1 = output.cast().map_err(|e| win(e, "IDXGIOutput1"))?;
            let out_desc = output.GetDesc().map_err(|e| win(e, "output GetDesc"))?;
            publish_rect(out_desc.DesktopCoordinates);

            // VIDEO_SUPPORT unlocks the GPU's video processor, which does our
            // colour conversion (convert.rs). Retry without it on GPUs that
            // refuse, and the encoder falls back to converting BGRA itself.
            let mut device = None;
            let mut context = None;
            let mut created: windows::core::Result<()> = Ok(());
            for flags in
                [D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_CREATE_DEVICE_BGRA_SUPPORT]
            {
                created = D3D11CreateDevice(
                    &adapter,
                    D3D_DRIVER_TYPE_UNKNOWN,
                    HMODULE::default(),
                    flags,
                    None,
                    D3D11_SDK_VERSION,
                    Some(&mut device),
                    None,
                    Some(&mut context),
                );
                if created.is_ok() {
                    break;
                }
            }
            created.map_err(|e| win(e, "D3D11CreateDevice"))?;
            let device = device.ok_or_else(|| PlatformError::Unavailable("no D3D11 device".into()))?;
            let context = context.ok_or_else(|| PlatformError::Unavailable("no D3D11 context".into()))?;

            // Started while the PC is locked: carry on without a picture
            // and pick the screen up the moment it unlocks, rather than
            // refusing to start.
            let (dup, res, refresh_hz, locked) = match output1.DuplicateOutput(&device) {
                Ok(dup) => {
                    let desc = dup.GetDesc();
                    (Some(dup), Resolution::new(desc.ModeDesc.Width, desc.ModeDesc.Height), refresh_of(&desc), false)
                }
                Err(e) if e.code() == E_ACCESSDENIED => {
                    let r = out_desc.DesktopCoordinates;
                    tracing::info!("screen is locked; will start capturing once it is unlocked");
                    let res = Resolution::new((r.right - r.left) as u32, (r.bottom - r.top) as u32);
                    (None, res, 60, true)
                }
                Err(e) if is_device_gone(e.code()) => return Err(PlatformError::DeviceLost(format!("{e}"))),
                Err(e) => return Err(win(e, "DuplicateOutput")),
            };

            let staging = Self::make_staging(&device, res)?;
            let gpu_tex = if output_mode == Output::Gpu { Some(Self::make_gpu_tex(&device, res)?) } else { None };
            tracing::info!(?res, refresh_hz, output_index, ?output_mode, "desktop duplication ready");
            Ok(Self {
                device,
                context,
                gpu_tex,
                adapter,
                output_index,
                dup,
                staging,
                res,
                refresh_hz,
                desktop_rect: out_desc.DesktopCoordinates,
                started: Instant::now(),
                locked,
                no_display: false,
                failing_since: None,
            })
        }
    }

    /// Where this output sits on the virtual desktop; the input injector
    /// needs it to turn stream coordinates into screen coordinates.
    pub fn desktop_rect(&self) -> RECT {
        self.desktop_rect
    }

    /// The D3D11 device the frames live on. A hardware encoder must be
    /// created on this same device to read them without a copy.
    pub fn device(&self) -> (&ID3D11Device, &ID3D11DeviceContext) {
        (&self.device, &self.context)
    }

    fn make_gpu_tex(device: &ID3D11Device, res: Resolution) -> Result<ID3D11Texture2D> {
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
        let mut tex = None;
        // SAFETY: desc is fully initialised; out-pointer is valid.
        unsafe { device.CreateTexture2D(&desc, None, Some(&mut tex)) }.map_err(|e| win(e, "CreateTexture2D(gpu)"))?;
        tex.ok_or_else(|| PlatformError::Backend(anyhow::anyhow!("CreateTexture2D returned nothing")))
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

    /// Recreate the duplication after it was lost (mode change, lock
    /// screen, UAC prompt, display off). Re-reads size and refresh rate and
    /// re-picks the display if ours went away.
    fn reacquire(&mut self) -> Result<()> {
        self.dup = None;
        // SAFETY: COM calls on live objects.
        unsafe {
            let Some((index, output)) = pick_output(&self.adapter, self.output_index) else {
                self.no_display = true;
                return Err(PlatformError::Unavailable("no display is on".into()));
            };
            self.no_display = false;
            if index != self.output_index {
                tracing::info!(display = index, "the captured display went away; now capturing another");
                self.output_index = index;
            }
            let output1: IDXGIOutput1 = output.cast().map_err(|e| win(e, "IDXGIOutput1"))?;
            self.desktop_rect = output.GetDesc().map_err(|e| win(e, "output GetDesc"))?.DesktopCoordinates;
            publish_rect(self.desktop_rect);
            let dup = match output1.DuplicateOutput(&self.device) {
                Ok(d) => d,
                Err(e) if e.code() == E_ACCESSDENIED => {
                    self.locked = true;
                    return Err(win(e, "DuplicateOutput (locked)"));
                }
                Err(e) if is_device_gone(e.code()) => return Err(PlatformError::DeviceLost(format!("{e}"))),
                Err(e) => return Err(win(e, "DuplicateOutput")),
            };
            self.locked = false;
            let desc = dup.GetDesc();
            let res = Resolution::new(desc.ModeDesc.Width, desc.ModeDesc.Height);
            let hz = refresh_of(&desc);
            if res != self.res || hz != self.refresh_hz {
                tracing::info!(old = ?self.res, new = ?res, refresh_hz = hz, "display mode changed");
                if res != self.res {
                    self.res = res;
                    self.staging = Self::make_staging(&self.device, res)?;
                    if self.gpu_tex.is_some() {
                        self.gpu_tex = Some(Self::make_gpu_tex(&self.device, res)?);
                    }
                }
                self.refresh_hz = hz;
            }
            self.dup = Some(dup);
        }
        Ok(())
    }
}

impl ScreenCapture for DxgiCapture {
    fn unavailable_reason(&self) -> Option<&'static str> {
        if self.locked {
            Some("the PC is locked or showing a security prompt; unlock it at the PC")
        } else if self.no_display {
            Some("no screen is on at the PC (lid closed or monitor off)")
        } else {
            None
        }
    }

    fn next_frame(&mut self, timeout: Duration) -> Result<Option<CapturedFrame>> {
        if self.dup.is_none() {
            match self.reacquire() {
                Ok(()) => self.failing_since = None,
                Err(PlatformError::DeviceLost(m)) => return Err(PlatformError::DeviceLost(m)),
                Err(e) => {
                    // Locked or no display: wait as long as it takes, the
                    // viewer is told why. Anything else gets a time limit.
                    if self.locked || self.no_display {
                        self.failing_since = None;
                    } else if self.failing_since.get_or_insert_with(Instant::now).elapsed() > GIVE_UP_AFTER {
                        self.failing_since = None;
                        return Err(PlatformError::DeviceLost(format!("screen capture won't come back: {e}")));
                    }
                    tracing::debug!("reacquire failed: {e}");
                    std::thread::sleep(Duration::from_millis(250));
                }
            }
            return Ok(None);
        }
        let Some(dup) = self.dup.as_ref() else { return Ok(None) };

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
            Err(e) if is_device_gone(e.code()) => return Err(PlatformError::DeviceLost(format!("{e}"))),
            Err(e) => {
                // Unknown trouble: start over with a fresh duplication next call.
                tracing::debug!("AcquireNextFrame: {e}; reacquiring");
                self.dup = None;
                return Ok(None);
            }
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

        if let Some(gpu_tex) = &self.gpu_tex {
            // SAFETY: GPU→GPU copy into a texture we own, then release the
            // duplication frame. The pointer handed out is valid until the
            // next call, which is the contract the encoder relies on.
            unsafe {
                let tex: ID3D11Texture2D = resource.cast().map_err(|e| win(e, "ID3D11Texture2D"))?;
                self.context.CopyResource(gpu_tex, &tex);
                drop(tex);
                drop(resource);
                dup.ReleaseFrame().map_err(|e| win(e, "ReleaseFrame"))?;
            }
            return Ok(Some(CapturedFrame {
                buffer: FrameBuffer::Gpu { api: GpuApi::D3D11, handle: gpu_tex.as_raw() as usize },
                format: PixelFormat::Bgra8,
                resolution: res,
                capture_ts_us,
            }));
        }

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
