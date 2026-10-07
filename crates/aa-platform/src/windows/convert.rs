//! BGRA → NV12 colour conversion on the GPU, with the colour maths stated
//! explicitly.
//!
//! Why this exists: video codecs carry YUV, the desktop is RGB, so someone
//! has to convert. Handing BGRA to the hardware encoder lets the *driver*
//! pick the matrix (BT.601 or BT.709) and the range (0-255 or 16-235), and
//! drivers disagree and don't tell you. If the stream header then claims a
//! different matrix or range than the pixels really use, the Mac decodes
//! them wrong: blacks turn grey, the picture looks washed out, reds shift.
//!
//! The D3D11 video processor is the GPU's fixed-function converter (the
//! same block video players use). We tell it exactly "input is full-range
//! sRGB, output is BT.709 video range", and the encoder header says the same
//! thing, so the two can never disagree. It runs on the GPU in well under a
//! millisecond and costs no CPU.

#![allow(unsafe_code, clippy::pedantic)]

use aa_core::video::Resolution;
use windows::core::Interface;
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, ID3D11VideoContext1, ID3D11VideoDevice, ID3D11VideoProcessor,
    ID3D11VideoProcessorEnumerator, ID3D11VideoProcessorInputView, ID3D11VideoProcessorOutputView,
    D3D11_BIND_RENDER_TARGET, D3D11_TEX2D_VPIV, D3D11_TEX2D_VPOV, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
    D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE, D3D11_VIDEO_PROCESSOR_CONTENT_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC,
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC,
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_STREAM, D3D11_VIDEO_USAGE_OPTIMAL_SPEED,
    D3D11_VPIV_DIMENSION_TEXTURE2D, D3D11_VPOV_DIMENSION_TEXTURE2D,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709, DXGI_FORMAT_NV12,
    DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};

use crate::{PlatformError, Result};

fn win(e: windows::core::Error, what: &str) -> PlatformError {
    PlatformError::Backend(anyhow::anyhow!("{what}: {e}"))
}

/// Converts one fixed BGRA texture into one fixed NV12 texture.
pub struct NvConverter {
    vctx: ID3D11VideoContext1,
    processor: ID3D11VideoProcessor,
    input_view: ID3D11VideoProcessorInputView,
    output_view: ID3D11VideoProcessorOutputView,
    output: ID3D11Texture2D,
}

impl std::fmt::Debug for NvConverter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NvConverter").finish_non_exhaustive()
    }
}

impl NvConverter {
    /// `input` is the BGRA texture the capture is copied into; it must stay
    /// alive as long as the converter (the encoder owns both).
    pub fn new(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        input: &ID3D11Texture2D,
        res: Resolution,
        fps: u16,
    ) -> Result<Self> {
        // SAFETY: COM calls with valid descriptors on our own device; every
        // out-parameter is checked before use.
        unsafe {
            let vdev: ID3D11VideoDevice = device.cast().map_err(|e| win(e, "ID3D11VideoDevice"))?;
            let vctx: ID3D11VideoContext1 = context.cast().map_err(|e| win(e, "ID3D11VideoContext1"))?;
            let rate = DXGI_RATIONAL { Numerator: u32::from(fps.max(1)), Denominator: 1 };
            let content = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
                InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                InputFrameRate: rate,
                InputWidth: res.width,
                InputHeight: res.height,
                OutputFrameRate: rate,
                OutputWidth: res.width,
                OutputHeight: res.height,
                Usage: D3D11_VIDEO_USAGE_OPTIMAL_SPEED,
            };
            let enumerator: ID3D11VideoProcessorEnumerator =
                vdev.CreateVideoProcessorEnumerator(&content).map_err(|e| win(e, "VideoProcessorEnumerator"))?;
            let processor = vdev.CreateVideoProcessor(&enumerator, 0).map_err(|e| win(e, "CreateVideoProcessor"))?;

            let desc = D3D11_TEXTURE2D_DESC {
                Width: res.width,
                Height: res.height,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_NV12,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
                CPUAccessFlags: 0,
                MiscFlags: 0,
            };
            let mut tex = None;
            device.CreateTexture2D(&desc, None, Some(&mut tex)).map_err(|e| win(e, "CreateTexture2D(NV12)"))?;
            let output = tex.ok_or_else(|| PlatformError::Backend(anyhow::anyhow!("no NV12 texture")))?;

            let in_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                FourCC: 0,
                ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: 0 },
                },
            };
            let mut input_view = None;
            vdev.CreateVideoProcessorInputView(input, &enumerator, &in_desc, Some(&mut input_view))
                .map_err(|e| win(e, "VideoProcessorInputView"))?;
            let input_view = input_view.ok_or_else(|| PlatformError::Backend(anyhow::anyhow!("no input view")))?;

            let out_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 } },
            };
            let mut output_view = None;
            vdev.CreateVideoProcessorOutputView(&output, &enumerator, &out_desc, Some(&mut output_view))
                .map_err(|e| win(e, "VideoProcessorOutputView"))?;
            let output_view = output_view.ok_or_else(|| PlatformError::Backend(anyhow::anyhow!("no output view")))?;

            // The whole point: say exactly what goes in and what comes out.
            vctx.VideoProcessorSetStreamColorSpace1(&processor, 0, DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709);
            vctx.VideoProcessorSetOutputColorSpace1(&processor, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709);
            vctx.VideoProcessorSetStreamFrameFormat(&processor, 0, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE);
            // No "enhancements": drivers may sharpen or tweak contrast by
            // default, which is exactly the kind of drift we are removing.
            vctx.VideoProcessorSetStreamAutoProcessingMode(&processor, 0, false);
            let full = RECT { left: 0, top: 0, right: res.width as i32, bottom: res.height as i32 };
            vctx.VideoProcessorSetStreamSourceRect(&processor, 0, true, Some(&full));
            vctx.VideoProcessorSetStreamDestRect(&processor, 0, true, Some(&full));
            vctx.VideoProcessorSetOutputTargetRect(&processor, true, Some(&full));

            Ok(Self { vctx, processor, input_view, output_view, output })
        }
    }

    /// Convert the current contents of the input texture.
    pub fn convert(&self) -> Result<()> {
        let stream = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: true.into(),
            OutputIndex: 0,
            InputFrameOrField: 0,
            PastFrames: 0,
            FutureFrames: 0,
            ppPastSurfaces: std::ptr::null_mut(),
            pInputSurface: std::mem::ManuallyDrop::new(Some(self.input_view.clone())),
            ppFutureSurfaces: std::ptr::null_mut(),
            ppPastSurfacesRight: std::ptr::null_mut(),
            pInputSurfaceRight: std::mem::ManuallyDrop::new(None),
            ppFutureSurfacesRight: std::ptr::null_mut(),
        };
        // SAFETY: views and processor are live; `stream` outlives the call.
        let mut streams = [stream];
        let result = unsafe { self.vctx.VideoProcessorBlt(&self.processor, &self.output_view, 0, &streams) };
        // The struct holds a ManuallyDrop clone of the view: release it
        // ourselves or every frame leaks a reference.
        // SAFETY: dropped exactly once, never used again.
        unsafe { std::mem::ManuallyDrop::drop(&mut streams[0].pInputSurface) };
        result.map_err(|e| win(e, "VideoProcessorBlt"))
    }

    /// The NV12 texture the encoder should read.
    pub fn output(&self) -> &ID3D11Texture2D {
        &self.output
    }
}
