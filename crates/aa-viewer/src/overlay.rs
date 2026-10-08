//! The in-window settings overlay.
//!
//! Toggled with Ctrl/Cmd+Shift+S. While open it owns the keyboard and
//! mouse (nothing is forwarded to the host) so changing a setting can't
//! click something on the remote machine. Drawn with `egui` into the same
//! render pass as the video, so it costs nothing when hidden.
//!
//! Settings live in [`Settings`]; the window applies them each frame.

use std::sync::Arc;

use winit::window::Window;

/// What the user can change at runtime.
#[derive(Debug, Clone, Copy, PartialEq)]
#[allow(clippy::struct_excessive_bools)] // it is a settings panel; these are checkboxes
pub struct Settings {
    pub fullscreen: bool,
    pub stretch: bool,
    /// Bitrate ceiling the host may use, in Mbps.
    pub max_mbps: f32,
    pub show_stats: bool,
    /// Silence the host PC's own speakers while we stream.
    pub mute_host: bool,
    /// Extra loudness on this Mac, in dB (limited, so it never clips).
    pub volume_boost_db: f32,
    /// Send this Mac's microphone to the PC.
    pub send_mic: bool,
}

impl Settings {
    pub const fn new(fullscreen: bool, stretch: bool) -> Self {
        Self {
            fullscreen,
            stretch,
            max_mbps: 80.0,
            show_stats: true,
            mute_host: false,
            volume_boost_db: 6.0,
            send_mic: false,
        }
    }
}

/// Live numbers for the stats line; the session updates these.
#[derive(Debug, Clone, Copy, Default)]
pub struct LiveStats {
    pub fps: u32,
    pub mbps: f32,
    pub rtt_ms: f32,
    pub loss_pct: f32,
}

pub struct Overlay {
    ctx: egui::Context,
    state: egui_winit::State,
    renderer: egui_wgpu::Renderer,
    pub open: bool,
    /// Shown for a few seconds after connecting so the hotkey is discoverable.
    hint_until: Option<std::time::Instant>,
}

impl std::fmt::Debug for Overlay {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Overlay").field("open", &self.open).finish_non_exhaustive()
    }
}

impl Overlay {
    pub fn new(window: &Arc<Window>, device: &wgpu::Device, surface_format: wgpu::TextureFormat) -> Self {
        let ctx = egui::Context::default();
        // Light panels matching the Anywhere app (white, hairline border,
        // indigo accent), slightly translucent so the video shows through.
        let mut v = egui::Visuals::light();
        let accent = egui::Color32::from_rgb(79, 70, 229);
        v.window_fill = egui::Color32::from_rgba_unmultiplied(255, 255, 255, 244);
        v.panel_fill = v.window_fill;
        v.window_stroke = egui::Stroke::new(1.0, egui::Color32::from_rgb(226, 229, 239));
        v.window_corner_radius = egui::CornerRadius::same(14);
        v.selection.bg_fill = egui::Color32::from_rgb(165, 160, 245);
        v.selection.stroke = egui::Stroke::new(1.0, accent);
        v.slider_trailing_fill = true;
        v.widgets.inactive.bg_fill = egui::Color32::from_rgb(226, 229, 239);
        v.hyperlink_color = accent;
        ctx.set_theme(egui::Theme::Light);
        ctx.set_visuals_of(egui::Theme::Light, v);
        let state = egui_winit::State::new(
            ctx.clone(),
            egui::ViewportId::ROOT,
            window.as_ref(),
            Some(window.scale_factor() as f32),
            None,
            None,
        );
        let renderer = egui_wgpu::Renderer::new(device, surface_format, egui_wgpu::RendererOptions::default());
        Self {
            ctx,
            state,
            renderer,
            open: false,
            hint_until: Some(std::time::Instant::now() + std::time::Duration::from_secs(5)),
        }
    }

    /// Feed a window event. Returns true if the overlay consumed it (the
    /// caller must then NOT forward it to the host).
    pub fn on_event(&mut self, window: &Window, event: &winit::event::WindowEvent) -> bool {
        if !self.open
            && !matches!(
                event,
                winit::event::WindowEvent::Resized(_) | winit::event::WindowEvent::ScaleFactorChanged { .. }
            )
        {
            return false;
        }
        let r = self.state.on_window_event(window, event);
        self.open && r.consumed
    }

    pub fn toggle(&mut self) {
        self.open = !self.open;
        self.hint_until = None;
    }

    /// Build the UI for this frame and draw it. Call inside an open render
    /// pass whose target is the surface texture. Returns the settings after
    /// any edits the user made.
    #[allow(clippy::too_many_arguments)]
    pub fn draw(
        &mut self,
        window: &Window,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        view: &wgpu::TextureView,
        size: [u32; 2],
        settings: &mut Settings,
        stats: LiveStats,
        host_label: &str,
    ) {
        let show_hint = self.hint_until.is_some_and(|t| std::time::Instant::now() < t);
        if !self.open && !show_hint && !settings.show_stats {
            return;
        }

        let raw = self.state.take_egui_input(window);
        self.ctx.begin_pass(raw);

        if show_hint {
            egui::Area::new(egui::Id::new("hint")).anchor(egui::Align2::CENTER_TOP, [0.0, 16.0]).show(
                &self.ctx,
                |ui| {
                    egui::Frame::popup(ui.style()).show(ui, |ui| {
                        ui.label(format!("Connected to {host_label}  ·  {} for settings", Self::hotkey_label()));
                    });
                },
            );
        }

        if settings.show_stats && !self.open {
            egui::Area::new(egui::Id::new("stats")).anchor(egui::Align2::RIGHT_BOTTOM, [-8.0, -8.0]).show(
                &self.ctx,
                |ui| {
                    egui::Frame::popup(ui.style()).show(ui, |ui| {
                        // One line, never wrapped next to the screen edge.
                        ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);
                        ui.monospace(format!(
                            "{} fps  {:.1} Mbps  {:.0} ms  {:.1}% loss",
                            stats.fps, stats.mbps, stats.rtt_ms, stats.loss_pct
                        ));
                    });
                },
            );
        }

        if self.open {
            let mut open = true;
            egui::Window::new("Anywhere")
                .open(&mut open)
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(&self.ctx, |ui| {
                    ui.set_min_width(320.0);
                    ui.heading("Display");
                    ui.checkbox(&mut settings.fullscreen, "Fullscreen");
                    ui.checkbox(&mut settings.stretch, "Stretch to fill (ignores aspect ratio)");
                    ui.add_space(8.0);
                    ui.heading("Stream");
                    ui.add(egui::Slider::new(&mut settings.max_mbps, 2.0..=150.0).text("Max Mbps").logarithmic(true));
                    ui.checkbox(&mut settings.show_stats, "Show stats");
                    ui.add_space(8.0);
                    ui.heading("Audio");
                    ui.checkbox(&mut settings.mute_host, "Mute PC speakers (sound plays here only)");
                    ui.add(egui::Slider::new(&mut settings.volume_boost_db, 0.0..=18.0).text("Volume boost (dB)"));
                    ui.checkbox(&mut settings.send_mic, "Send my microphone to the PC");
                    ui.add_space(8.0);
                    ui.monospace(format!(
                        "{} fps  {:.1} Mbps  {:.0} ms RTT  {:.1}% loss",
                        stats.fps, stats.mbps, stats.rtt_ms, stats.loss_pct
                    ));
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new(format!("{} to close", Self::hotkey_label())).weak());
                    });
                });
            if !open {
                self.open = false;
            }
        }

        let mut out = self.ctx.end_pass();
        self.state.handle_platform_output(window, out.platform_output);
        let ppp = self.ctx.pixels_per_point();
        let jobs = self.ctx.tessellate(out.shapes, ppp);
        let desc = egui_wgpu::ScreenDescriptor { size_in_pixels: size, pixels_per_point: ppp };
        for (id, deltas) in &out.textures_delta.set {
            for delta in deltas {
                self.renderer.update_texture(device, queue, *id, delta);
            }
        }
        self.renderer.update_buffers(device, queue, encoder, &jobs, &desc);
        {
            let pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("overlay"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view,
                    resolve_target: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store },
                    depth_slice: None,
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            let mut pass = pass.forget_lifetime();
            self.renderer.render(&mut pass, &jobs, &desc);
        }
        for id in &out.textures_delta.free {
            self.renderer.free_texture(id);
        }
        // Handled above; egui checks (in debug builds) that nothing was lost.
        out.textures_delta.clear();
    }

    fn hotkey_label() -> &'static str {
        if cfg!(target_os = "macos") {
            "⌘⇧S"
        } else {
            "Ctrl+Shift+S"
        }
    }
}
