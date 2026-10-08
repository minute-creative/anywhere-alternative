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
use crate::overlay::{LiveStats, Overlay, Settings};

/// Events the session thread sends to the window.
#[derive(Debug, Clone)]
pub enum Wake {
    /// A new frame is in the slot; redraw.
    Frame,
    /// The session ended (host gone, error); close the window.
    SessionEnded(String),
    /// The host went quiet; we are trying to get it back.
    Reconnecting,
    /// The host accepted us (first time or after a reconnect).
    Connected,
    /// Why the host's picture is paused, or `None` when it is back.
    HostStatus(Option<String>),
}

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
    /// Fill the window, ignoring aspect ratio (the picture distorts).
    fn stretch(window: PhysicalSize<u32>) -> Self {
        Self { x: 0.0, y: 0.0, w: window.width.max(1) as f32, h: window.height.max(1) as f32 }
    }

    fn fit(video: Resolution, window: PhysicalSize<u32>) -> Self {
        let (ww, wh) = (window.width.max(1) as f32, window.height.max(1) as f32);
        let (vw, vh) = (video.width.max(1) as f32, video.height.max(1) as f32);
        let scale = (ww / vw).min(wh / vh);
        let w = vw * scale;
        let h = vh * scale;
        Self { x: (ww - w) * 0.5, y: (wh - h) * 0.5, w, h }
    }

    /// Window position → 0..=65535 across the video, or `None` if the
    /// cursor is on the letterbox bars. Half a pixel of tolerance at the
    /// edges absorbs float rounding in the fit, then the value is clamped.
    fn normalise(&self, px: f64, py: f64) -> Option<(u16, u16)> {
        let nx = (px as f32 - self.x) / self.w;
        let ny = (py as f32 - self.y) / self.h;
        let (tx, ty) = (0.5 / self.w, 0.5 / self.h);
        if nx < -tx || nx > 1.0 + tx || ny < -ty || ny > 1.0 + ty {
            return None;
        }
        Some(((nx.clamp(0.0, 1.0) * 65535.0) as u16, (ny.clamp(0.0, 1.0) * 65535.0) as u16))
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
        // Render in sRGB: the frame bytes are sRGB-encoded, so sample them as
        // sRGB and write to an sRGB target. Treating them as linear washes
        // the picture out (greys lift, colours flatten).
        let caps = surface.get_capabilities(&adapter);
        if let Some(f) = caps.formats.iter().copied().find(wgpu::TextureFormat::is_srgb) {
            config.format = f;
        }
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
        let tex_format = match frame.format {
            PixelFormat::Bgra8 => wgpu::TextureFormat::Bgra8UnormSrgb,
            PixelFormat::Rgba8 => wgpu::TextureFormat::Rgba8UnormSrgb,
            other => {
                tracing::warn!(?other, "presenter only handles 8-bit RGBA/BGRA CPU frames for now");
                return;
            }
        };
        let res = frame.resolution;
        if !self.frame.as_ref().is_some_and(|(t, _, r)| *r == res && t.format() == tex_format) {
            let texture = self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("frame"),
                size: wgpu::Extent3d { width: res.width, height: res.height, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: tex_format,
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

    fn render(
        &mut self,
        rect: VideoRect,
        window: &Window,
        overlay: impl FnOnce(&wgpu::Device, &wgpu::Queue, &mut wgpu::CommandEncoder, &wgpu::TextureView, [u32; 2]),
    ) {
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
        overlay(&self.device, &self.queue, &mut encoder, &view, [self.config.width, self.config.height]);
        self.queue.submit(Some(encoder.finish()));
        window.pre_present_notify();
        self.queue.present(output);
    }

    fn surface_format(&self) -> wgpu::TextureFormat {
        self.config.format
    }
}

#[allow(clippy::struct_excessive_bools)] // window state flags
pub struct App {
    title: String,
    fullscreen: bool,
    stretch: bool,
    frames: FrameSlot,
    commands: mpsc::Sender<ViewerCommand>,
    window: Option<Arc<Window>>,
    gpu: Option<Gpu>,
    video_res: Option<Resolution>,
    rect: VideoRect,
    presented: u64,
    /// HID usages currently held down, so we can release them all when the
    /// window loses focus. Otherwise a Cmd+Tab away leaves Ctrl stuck on the
    /// host and every later click becomes Ctrl+click.
    held_keys: Vec<u16>,
    /// Treat the Mac Command key as Control on the host, so Cmd+C / Cmd+V do
    /// what a Mac user expects on a Windows host.
    cmd_as_ctrl: bool,
    /// Cmd is down on the Mac keyboard (see the key handler for why it matters).
    cmd_held: bool,
    /// Between a lost host and the next successful connection.
    reconnecting: bool,
    overlay: Option<Overlay>,
    settings: Settings,
    applied: Settings,
    stats: LiveStats,
    stats_rx: Option<std::sync::mpsc::Receiver<LiveStats>>,
    modifiers: winit::keyboard::ModifiersState,
}

impl std::fmt::Debug for App {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("App").field("presented", &self.presented).finish_non_exhaustive()
    }
}

impl App {
    /// The session was started with the mic on: show the box ticked.
    pub fn set_mic_shown(&mut self, on: bool) {
        self.settings.send_mic = on;
        self.applied.send_mic = on;
    }

    pub fn new(
        title: String,
        fullscreen: bool,
        stretch: bool,
        frames: FrameSlot,
        commands: mpsc::Sender<ViewerCommand>,
    ) -> Self {
        Self {
            title,
            fullscreen,
            stretch,
            frames,
            commands,
            window: None,
            gpu: None,
            video_res: None,
            rect: VideoRect::default(),
            presented: 0,
            held_keys: Vec::new(),
            cmd_as_ctrl: cfg!(target_os = "macos"),
            cmd_held: false,
            reconnecting: false,
            overlay: None,
            settings: Settings::new(fullscreen, stretch),
            applied: Settings::new(fullscreen, stretch),
            stats: LiveStats::default(),
            stats_rx: None,
            modifiers: winit::keyboard::ModifiersState::empty(),
        }
    }

    /// Where the session delivers once-a-second stats for the overlay.
    pub fn set_stats_receiver(&mut self, rx: std::sync::mpsc::Receiver<LiveStats>) {
        self.stats_rx = Some(rx);
    }

    /// Push changed settings to the window and host.
    fn apply_settings(&mut self) {
        if self.settings == self.applied {
            return;
        }
        if let Some(w) = &self.window {
            if self.settings.fullscreen != self.applied.fullscreen {
                w.set_fullscreen(self.settings.fullscreen.then_some(Fullscreen::Borderless(None)));
            }
        }
        if self.settings.stretch != self.applied.stretch {
            self.stretch = self.settings.stretch;
            self.refit();
        }
        if (self.settings.max_mbps - self.applied.max_mbps).abs() > 0.01 {
            let kbps = (self.settings.max_mbps * 1000.0) as u32;
            let _ = self.commands.try_send(ViewerCommand::SetMaxBitrate(kbps));
        }
        if self.settings.send_mic != self.applied.send_mic {
            let _ = self.commands.try_send(ViewerCommand::SetMic(self.settings.send_mic));
        }
        if self.settings.mute_host != self.applied.mute_host {
            let _ = self.commands.try_send(ViewerCommand::SetHostMute(self.settings.mute_host));
        }
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        if (self.settings.volume_boost_db - self.applied.volume_boost_db).abs() > 0.01 {
            aa_platform::audio::set_boost_db(self.settings.volume_boost_db);
        }
        self.applied = self.settings;
    }

    fn send(&self, ev: InputEvent) {
        if self.commands.try_send(ViewerCommand::Input(ev)).is_err() {
            // Rate-limit: this only happens when the session is gone or stalled.
            if self.presented == 0 {
                tracing::trace!("input dropped; session not consuming");
            } else {
                tracing::warn!("input channel full; event dropped");
            }
        }
    }

    fn layout(&self, res: Resolution, size: PhysicalSize<u32>) -> VideoRect {
        if self.stretch {
            VideoRect::stretch(size)
        } else {
            VideoRect::fit(res, size)
        }
    }

    fn refit(&mut self) {
        if let (Some(res), Some(w)) = (self.video_res, &self.window) {
            self.rect = self.layout(res, w.inner_size());
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
            Ok(g) => {
                self.overlay = Some(Overlay::new(&window, &g.device, g.surface_format()));
                self.gpu = Some(g);
            }
            Err(e) => {
                tracing::error!("cannot initialise GPU: {e:#}");
                el.exit();
                return;
            }
        }
        self.window = Some(window);
    }

    fn user_event(&mut self, el: &ActiveEventLoop, ev: Wake) {
        match ev {
            Wake::Frame => {
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            Wake::SessionEnded(reason) => {
                tracing::info!("closing window: {reason}");
                el.exit();
            }
            Wake::Reconnecting => {
                self.reconnecting = true;
                if let Some(w) = &self.window {
                    w.set_title(&format!("{} — reconnecting…", self.title));
                }
            }
            Wake::HostStatus(msg) => {
                if let Some(w) = &self.window {
                    match msg {
                        Some(m) => w.set_title(&format!("{} — paused: {m}", self.title)),
                        None => w.set_title(&self.title),
                    }
                }
            }
            Wake::Connected => {
                if let Some(w) = &self.window {
                    w.set_title(&self.title);
                }
                if std::mem::take(&mut self.reconnecting) {
                    // A new session starts with defaults: send the user's
                    // choices again.
                    let s = self.settings;
                    let _ = self.commands.try_send(ViewerCommand::SetMaxBitrate((s.max_mbps * 1000.0) as u32));
                    let _ = self.commands.try_send(ViewerCommand::SetHostMute(s.mute_host));
                    let _ = self.commands.try_send(ViewerCommand::SetMic(s.send_mic));
                }
            }
        }
    }

    #[allow(clippy::too_many_lines)] // one match arm per event kind
    fn window_event(&mut self, el: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        if let WindowEvent::ModifiersChanged(m) = &event {
            self.modifiers = m.state();
        }
        // Settings hotkey: Ctrl/Cmd + Shift + S.
        if let WindowEvent::KeyboardInput { event: k, .. } = &event {
            let primary =
                if cfg!(target_os = "macos") { self.modifiers.super_key() } else { self.modifiers.control_key() };
            if k.state == ElementState::Pressed
                && !k.repeat
                && primary
                && self.modifiers.shift_key()
                && k.physical_key == PhysicalKey::Code(winit::keyboard::KeyCode::KeyS)
            {
                if let Some(o) = &mut self.overlay {
                    o.toggle();
                    // The host must not be left thinking Ctrl/Shift are down.
                    for hid in std::mem::take(&mut self.held_keys) {
                        self.send(InputEvent::Key { hid_usage: hid, pressed: false });
                    }
                    self.send(InputEvent::ReleaseAll);
                    if let Some(w) = &self.window {
                        w.request_redraw();
                    }
                }
                return;
            }
        }
        // While the overlay is open it owns input; nothing goes to the host.
        if let (Some(o), Some(w)) = (&mut self.overlay, &self.window) {
            let consumed = o.on_event(w, &event);
            if o.open {
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
                if consumed
                    || matches!(
                        event,
                        WindowEvent::KeyboardInput { .. }
                            | WindowEvent::MouseInput { .. }
                            | WindowEvent::CursorMoved { .. }
                            | WindowEvent::MouseWheel { .. }
                    )
                {
                    return;
                }
            }
        }
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
                let Some(window) = self.window.clone() else { return };
                let window = window.as_ref();
                if let Some(frame) = self.frames.take() {
                    if self.video_res != Some(frame.resolution) {
                        self.video_res = Some(frame.resolution);
                        self.rect = if self.stretch {
                            VideoRect::stretch(window.inner_size())
                        } else {
                            VideoRect::fit(frame.resolution, window.inner_size())
                        };
                    }
                    gpu.upload(&frame);
                    self.presented += 1;
                }
                if let Some(rx) = &self.stats_rx {
                    while let Ok(s) = rx.try_recv() {
                        self.stats = s;
                    }
                }
                let rect = self.rect;
                let title = self.title.clone();
                let mut settings = self.settings;
                let stats = self.stats;
                let overlay = &mut self.overlay;
                gpu.render(rect, window, |device, queue, encoder, view, size| {
                    if let Some(o) = overlay {
                        o.draw(window, device, queue, encoder, view, size, &mut settings, stats, &title);
                    }
                });
                self.settings = settings;
                self.apply_settings();
                // Keep repainting while the overlay or hint is visible.
                if self.overlay.as_ref().is_some_and(|o| o.open) {
                    window.request_redraw();
                }
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
                        let hid = shortcut_key(hid, self.cmd_as_ctrl, crate::link::host_is_mac());
                        let pressed = event.state == ElementState::Pressed;
                        let is_modifier = (0xE0..=0xE7).contains(&hid);
                        if pressed && !is_modifier && self.cmd_as_ctrl && self.cmd_held {
                            // macOS never reports the key-up of a key pressed
                            // while Cmd is down. Sent as a plain press, Cmd+C
                            // would leave C held on the PC (and repeating).
                            // So send the whole tap now: shortcuts only need
                            // the press anyway.
                            self.send(InputEvent::Key { hid_usage: hid, pressed: true });
                            self.send(InputEvent::Key { hid_usage: hid, pressed: false });
                            return;
                        }
                        if matches!(code, winit::keyboard::KeyCode::SuperLeft | winit::keyboard::KeyCode::SuperRight) {
                            self.cmd_held = pressed;
                        }
                        if pressed {
                            if !self.held_keys.contains(&hid) {
                                self.held_keys.push(hid);
                            }
                        } else {
                            self.held_keys.retain(|k| *k != hid);
                        }
                        self.send(InputEvent::Key { hid_usage: hid, pressed });
                    }
                }
            }
            WindowEvent::Focused(false) => {
                self.cmd_held = false;
                // macOS does not deliver key-up for keys released while another
                // app has focus (Cmd during Cmd+Tab is the classic). Release
                // everything we think is down so nothing sticks on the host.
                for hid in std::mem::take(&mut self.held_keys) {
                    self.send(InputEvent::Key { hid_usage: hid, pressed: false });
                }
                self.send(InputEvent::ReleaseAll);
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
        assert!(r.x.abs() < 0.01);
    }

    #[test]
    fn normalise_maps_corners_and_rejects_bars() {
        let r = VideoRect::fit(Resolution::new(1920, 1080), PhysicalSize::new(1000, 1000));
        assert_eq!(r.normalise(0.0, 218.75), Some((0, 0)));
        assert_eq!(r.normalise(1000.0, 781.25), Some((65535, 65535)));
        assert_eq!(r.normalise(500.0, 10.0), None);
    }
}

/// Shortcuts mean the same on both machines. `mac_viewer`: this keyboard
/// is a Mac's (`cmd_as_ctrl`); `mac_host`: the far end is a Mac.
/// - Mac → PC: Cmd becomes Ctrl (Cmd+C copies on the PC).
/// - PC → Mac: Ctrl becomes Cmd (Ctrl+C copies on the Mac).
/// - Same kind on both ends: keys go through unchanged.
fn shortcut_key(hid: u16, mac_viewer: bool, mac_host: bool) -> u16 {
    match (mac_viewer, mac_host, hid) {
        (true, false, 0xE3) => 0xE0, // Left Cmd -> Left Ctrl
        (true, false, 0xE7) => 0xE4, // Right Cmd -> Right Ctrl
        (false, true, 0xE0) => 0xE3, // Left Ctrl -> Left Cmd
        (false, true, 0xE4) => 0xE7, // Right Ctrl -> Right Cmd
        _ => hid,
    }
}

#[cfg(test)]
mod shortcut_tests {
    use super::shortcut_key;

    #[test]
    fn shortcuts_follow_the_host() {
        assert_eq!(shortcut_key(0xE3, true, false), 0xE0, "Mac keyboard, PC host: Cmd is Ctrl");
        assert_eq!(shortcut_key(0xE3, true, true), 0xE3, "Mac to Mac: unchanged");
        assert_eq!(shortcut_key(0xE0, false, true), 0xE3, "PC keyboard, Mac host: Ctrl is Cmd");
        assert_eq!(shortcut_key(0xE0, false, false), 0xE0, "PC to PC: unchanged");
        assert_eq!(shortcut_key(0x06, false, true), 0x06, "letters never change");
    }
}
