//! The viewer window: shows frames with `wgpu`, forwards keyboard and mouse.
//!
//! Runs on the main thread because every OS requires the UI there. The
//! network session runs on a tokio thread and talks to us through
//! [`crate::link`]. Presentation is vsync-locked (`PresentMode::Fifo`): a
//! frame is drawn on the next display refresh and never torn, which is the
//! "locked to the viewer's refresh rate" rule from the architecture doc.
//!
//! This commit uploads CPU frames (`FrameBuffer::Cpu`) through
//! `queue.write_texture`. The zero-copy GPU path for real decoders replaces
//! that one function; nothing else here changes.

use std::sync::Arc;

use aa_core::input::{InputEvent, MouseButton};
use aa_core::video::{PixelFormat, Resolution};
use aa_platform::{DecodedFrame, FrameBuffer};
use tokio::sync::mpsc;
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::{ElementState, MouseButton as WinitButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::keyboard::PhysicalKey;
use winit::window::{Fullscreen, Window, WindowId};

use crate::keymap::hid_usage;
use crate::link::{FrameSlot, ViewerCommand};

/// The only user event: "a new frame is in the slot, redraw".
#[derive(Debug, Clone, Copy)]
pub struct Wake;

pub fn build_event_loop() -> anyhow::Result<EventLoop<Wake>> {
    Ok(EventLoop::<Wake>::with_user_event().build()?)
}

/// Where the video lands inside the window, in physical pixels (aspect-fit).
#[derive(Debug, Clone, Copy, Default)]
struct VideoRect {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
}

impl VideoRect {
    fn fit(video: Resolution, window: PhysicalSize<u32>) -> Self {
        let (ww, wh) = (window.width.max(1) as f32, window.height.max(1) as f32);
        let (vw, vh) = (video.width.max(1) as f32, video.height.max(1) as f32);
        let scale = (ww / vw).min(wh / vh);
        let w = vw * scale;
        let h = vh * scale;
        Self { x: (ww - w) * 0.5, y: (wh - h) * 0.5, w, h }
    }

    /// Window position → 0..=65535 across the video, or `None` if outside.
    fn normalise(&self, px: f64, py: f64) -> Option<(u16, u16)> {
        let nx = (px as f32 - self.x) / self.w;
        let ny = (py as f32 - self.y) / self.h;
        if !(0.0..=1.0).contains(&nx) || !(0.0..=1.0).contains(&ny) {
            return None;
        }
        Some(((nx * 65535.0) as u16, (ny * 65535.0) as u16))
    }
}

struct Gpu {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    pipeline: wgpu::RenderPipeline,
    bind_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    /// Current frame texture and its bind group; rebuilt on resolution change.
    frame: Option<(wgpu::Texture, wgpu::BindGroup, Resolution)>,
}

impl Gpu {
    fn new(window: &Arc<Window>) -> anyhow::Result<Self> {
        // On X11/Wayland wgpu wants the display handle; harmless elsewhere.
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_with_display_handle(Box::new(window.clone())));
        let surface = instance.create_surface(window.clone())?;
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
            apply_limit_buckets: false,
        }))?;
        tracing::info!(adapter = ?adapter.get_info().name, backend = ?adapter.get_info().backend, "gpu");
        let (device, queue) = pollster::block_on(
            adapter.request_device(&wgpu::DeviceDescriptor { label: Some("aa-viewer"), ..Default::default() }),
        )?;

        let size = window.inner_size();
        let mut config = surface
            .get_default_config(&adapter, size.width.max(1), size.height.max(1))
            .ok_or_else(|| anyhow::anyhow!("surface not supported by adapter"))?;
        // Fifo = vsync: one frame per refresh, never torn, locked to the display.
        config.present_mode = wgpu::PresentMode::Fifo;
        // Latency: don't let the swapchain queue frames behind our back.
        config.desired_maximum_frame_latency = 1;
        surface.configure(&device, &config);

        let shader = device.create_shader_module(wgpu::include_wgsl!("shader.wgsl"));
        let bind_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("frame"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("frame"),
            bind_group_layouts: &[Some(&bind_layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("frame"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: config.format,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("frame"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        Ok(Self { surface, device, queue, config, pipeline, bind_layout, sampler, frame: None })
    }

    fn resize(&mut self, size: PhysicalSize<u32>) {
        if size.width == 0 || size.height == 0 {
            return;
        }
        self.config.width = size.width;
        self.config.height = size.height;
        self.surface.configure(&self.device, &self.config);
    }

    /// Upload a decoded CPU frame into the frame texture.
    fn upload(&mut self, frame: &DecodedFrame) {
        let FrameBuffer::Cpu(px) = &frame.buffer else {
            tracing::warn!("GPU frame buffers not wired to the presenter yet");
            return;
        };
        if frame.format != PixelFormat::Bgra8 {
            tracing::warn!(?frame.format, "presenter only handles BGRA8 CPU frames for now");
            return;
        }
        let res = frame.resolution;
        if !self.frame.as_ref().is_some_and(|(_, _, r)| *r == res) {
            let texture = self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("frame"),
                size: wgpu::Extent3d { width: res.width, height: res.height, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Bgra8Unorm,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });
            let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
            let bind = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("frame"),
                layout: &self.bind_layout,
                entries: &[
                    wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&view) },
                    wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(&self.sampler) },
                ],
            });
            tracing::info!(?res, "frame texture created");
            self.frame = Some((texture, bind, res));
        }
        let (texture, _, _) = self.frame.as_ref().expect("just ensured");
        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            px,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(res.width * 4),
                rows_per_image: Some(res.height),
            },
            wgpu::Extent3d { width: res.width, height: res.height, depth_or_array_layers: 1 },
        );
    }

    fn render(&mut self, rect: VideoRect, window: &Window) {
        use wgpu::CurrentSurfaceTexture as Cst;
        let output = match self.surface.get_current_texture() {
            Cst::Success(t) | Cst::Suboptimal(t) => t,
            Cst::Lost | Cst::Outdated => {
                self.surface.configure(&self.device, &self.config);
                return;
            }
            Cst::Timeout | Cst::Occluded => return,
            Cst::Validation => {
                tracing::error!("surface validation error; see wgpu log");
                return;
            }
        };
        let view = output.texture.create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder =
            self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("present") });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("present"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            if let Some((_, bind, _)) = &self.frame {
                pass.set_viewport(rect.x, rect.y, rect.w.max(1.0), rect.h.max(1.0), 0.0, 1.0);
                pass.set_pipeline(&self.pipeline);
                pass.set_bind_group(0, bind, &[]);
                pass.draw(0..3, 0..1);
            }
        }
        self.queue.submit(Some(encoder.finish()));
        window.pre_present_notify();
        self.queue.present(output);
    }
}

pub struct App {
    title: String,
    fullscreen: bool,
    frames: FrameSlot,
    commands: mpsc::Sender<ViewerCommand>,
    window: Option<Arc<Window>>,
    gpu: Option<Gpu>,
    video_res: Option<Resolution>,
    rect: VideoRect,
    presented: u64,
}

impl std::fmt::Debug for App {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("App").field("presented", &self.presented).finish_non_exhaustive()
    }
}

impl App {
    pub fn new(title: String, fullscreen: bool, frames: FrameSlot, commands: mpsc::Sender<ViewerCommand>) -> Self {
        Self {
            title,
            fullscreen,
            frames,
            commands,
            window: None,
            gpu: None,
            video_res: None,
            rect: VideoRect::default(),
            presented: 0,
        }
    }

    fn send(&self, ev: InputEvent) {
        if self.commands.try_send(ViewerCommand::Input(ev)).is_err() {
            tracing::warn!("input channel full; event dropped");
        }
    }

    fn refit(&mut self) {
        if let (Some(res), Some(w)) = (self.video_res, &self.window) {
            self.rect = VideoRect::fit(res, w.inner_size());
        }
    }
}

impl ApplicationHandler<Wake> for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let mut attrs =
            Window::default_attributes().with_title(&self.title).with_inner_size(PhysicalSize::new(1280, 720));
        if self.fullscreen {
            attrs = attrs.with_fullscreen(Some(Fullscreen::Borderless(None)));
        }
        let window = match el.create_window(attrs) {
            Ok(w) => Arc::new(w),
            Err(e) => {
                tracing::error!("cannot create window: {e}");
                el.exit();
                return;
            }
        };
        match Gpu::new(&window) {
            Ok(g) => self.gpu = Some(g),
            Err(e) => {
                tracing::error!("cannot initialise GPU: {e:#}");
                el.exit();
                return;
            }
        }
        self.window = Some(window);
    }

    fn user_event(&mut self, _el: &ActiveEventLoop, _: Wake) {
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => {
                let _ = self.commands.try_send(ViewerCommand::Quit);
                el.exit();
            }
            WindowEvent::Resized(size) => {
                if let Some(g) = &mut self.gpu {
                    g.resize(size);
                }
                self.refit();
            }
            WindowEvent::RedrawRequested => {
                let Some(gpu) = &mut self.gpu else { return };
                let Some(window) = &self.window else { return };
                if let Some(frame) = self.frames.take() {
                    if self.video_res != Some(frame.resolution) {
                        self.video_res = Some(frame.resolution);
                        self.rect = VideoRect::fit(frame.resolution, window.inner_size());
                    }
                    gpu.upload(&frame);
                    self.presented += 1;
                }
                gpu.render(self.rect, window);
            }
            WindowEvent::CursorMoved { position, .. } => {
                if let Some((x, y)) = self.rect.normalise(position.x, position.y) {
                    self.send(InputEvent::MouseMoveAbs { x, y });
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                let button = match button {
                    WinitButton::Left => MouseButton::Left,
                    WinitButton::Right => MouseButton::Right,
                    WinitButton::Middle => MouseButton::Middle,
                    WinitButton::Back => MouseButton::Back,
                    WinitButton::Forward => MouseButton::Forward,
                    WinitButton::Other(_) => return,
                };
                self.send(InputEvent::MouseButton { button, pressed: state == ElementState::Pressed });
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let (dx, dy) = match delta {
                    MouseScrollDelta::LineDelta(x, y) => ((x * 120.0) as i16, (y * 120.0) as i16),
                    MouseScrollDelta::PixelDelta(p) => (p.x as i16, p.y as i16),
                };
                if dx != 0 || dy != 0 {
                    self.send(InputEvent::MouseScroll { dx, dy });
                }
            }
            WindowEvent::KeyboardInput { event, .. } => {
                if event.repeat {
                    return; // the host OS generates its own repeats
                }
                if let PhysicalKey::Code(code) = event.physical_key {
                    if let Some(hid) = hid_usage(code) {
                        self.send(InputEvent::Key { hid_usage: hid, pressed: event.state == ElementState::Pressed });
                    }
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letterbox_fits_wide_video_in_tall_window() {
        let r = VideoRect::fit(Resolution::new(1920, 1080), PhysicalSize::new(1000, 1000));
        assert!((r.w - 1000.0).abs() < 0.01);
        assert!((r.h - 562.5).abs() < 0.01);
        assert!((r.y - 218.75).abs() < 0.01);
        assert!(r.x.abs() < f32::EPSILON);
    }

    #[test]
    fn normalise_maps_corners_and_rejects_bars() {
        let r = VideoRect::fit(Resolution::new(1920, 1080), PhysicalSize::new(1000, 1000));
        assert_eq!(r.normalise(0.0, 218.75), Some((0, 0)));
        assert_eq!(r.normalise(1000.0, 781.25), Some((65535, 65535)));
        assert_eq!(r.normalise(500.0, 10.0), None);
    }
}
