//! Anywhere Alternative: the app people actually open.
//!
//! One window with two jobs:
//! - **Connect to another computer**: computers sharing themselves on the
//!   network appear in a list by name; one click opens the viewer.
//! - **Share this computer**: one button starts the host; the window says
//!   whether anyone is connected and, in plain words, what is missing
//!   (permissions, optional drivers) with a button to fix each.
//!
//! The streaming engines stay separate programs next to this one
//! (`aa-host`, `aa-viewer`), so this window is only ever a remote control.

#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod checks;
mod procs;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use aa_platform::discover::{find_hosts, Found, DEFAULT_PORT};
use eframe::egui::{self, Color32, CornerRadius, FontId, Margin, RichText, Stroke, TextStyle};
use serde::{Deserialize, Serialize};

// Colours (dark, calm, one accent).
const BG: Color32 = Color32::from_rgb(14, 17, 34);
const CARD: Color32 = Color32::from_rgb(26, 31, 60);
const EDGE: Color32 = Color32::from_rgb(44, 52, 92);
const TEXT: Color32 = Color32::from_rgb(232, 236, 248);
const MUTED: Color32 = Color32::from_rgb(150, 160, 190);
const ACCENT: Color32 = Color32::from_rgb(90, 216, 255);
const GOOD: Color32 = Color32::from_rgb(110, 220, 150);
const WARN: Color32 = Color32::from_rgb(255, 196, 92);
const STOP: Color32 = Color32::from_rgb(240, 110, 110);

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
#[allow(clippy::struct_excessive_bools)] // settings are switches
struct Settings {
    fullscreen: bool,
    mic: bool,
    share_on_launch: bool,
    /// Mac only: run sharing as administrator so controllers work.
    mac_controllers: bool,
    manual_address: String,
}

impl Settings {
    fn path() -> std::path::PathBuf {
        procs::data_dir().join("settings.json")
    }
    fn load() -> Self {
        std::fs::read(Self::path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }
    fn save(&self) {
        if let Ok(b) = serde_json::to_vec_pretty(self) {
            let _ = std::fs::write(Self::path(), b);
        }
    }
}

/// What the host's log says about the session.
#[derive(Debug, Default)]
struct HostStatus {
    addresses: Option<String>,
    viewer: Option<String>,
    warnings: Vec<String>,
}

fn read_status(lines: &[String]) -> HostStatus {
    let mut s = HostStatus::default();
    for l in lines {
        if let Some((_, a)) = l.split_once("viewers can connect to: ") {
            s.addresses = Some(a.trim().to_owned());
        } else if l.contains("viewer connected") || l.contains("viewer reconnected") {
            let from = l.split("from=").nth(1).or_else(|| l.split("new=").nth(1));
            s.viewer = Some(from.and_then(|r| r.split_whitespace().next()).unwrap_or("someone").to_owned());
        } else if l.contains("viewer left") || l.contains("viewer timed out") {
            s.viewer = None;
        }
        if l.contains(" WARN ") || l.contains(" ERROR ") {
            let p = procs::plain(l);
            if !s.warnings.contains(&p) {
                s.warnings.push(p);
            }
        }
    }
    let n = s.warnings.len();
    s.warnings.drain(..n.saturating_sub(4));
    s
}

struct App {
    icon: Option<egui::TextureHandle>,
    settings: Settings,
    found: Arc<Mutex<Vec<Found>>>,
    host: Option<procs::Host>,
    host_error: Option<String>,
    host_lines: Vec<String>,
    viewer: Option<procs::Viewer>,
    viewer_note: Option<String>,
    checks: Vec<checks::Check>,
    checks_at: Instant,
    last_poll: Instant,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        style(&cc.egui_ctx);
        let found: Arc<Mutex<Vec<Found>>> = Arc::default();
        spawn_discovery(Arc::clone(&found), cc.egui_ctx.clone());
        let settings = Settings::load();
        let icon =
            aa_platform::clipboard::png_to_rgba(include_bytes!("../../../assets/icon-256.png")).map(|(w, h, px)| {
                let img = egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &px);
                cc.egui_ctx.load_texture("icon", img, egui::TextureOptions::LINEAR)
            });
        let mut app = Self {
            icon,
            settings,
            found,
            host: None,
            host_error: None,
            host_lines: Vec::new(),
            viewer: None,
            viewer_note: None,
            checks: checks::run(),
            checks_at: Instant::now(),
            last_poll: Instant::now(),
        };
        if app.settings.share_on_launch {
            app.start_sharing();
        }
        app
    }

    fn start_sharing(&mut self) {
        self.host_error = None;
        match procs::Host::start(self.settings.mac_controllers && cfg!(target_os = "macos")) {
            Ok(h) => self.host = Some(h),
            Err(e) => self.host_error = Some(e.to_string()),
        }
    }

    fn connect(&mut self, target: &str, label: &str) {
        if let Some(v) = self.viewer.as_mut() {
            v.close();
        }
        self.viewer = None;
        match procs::Viewer::start(target, self.settings.fullscreen, self.settings.mic) {
            Ok(v) => {
                self.viewer_note = Some(format!("Opened {label}"));
                self.viewer = Some(v);
            }
            Err(e) => self.viewer_note = Some(format!("⚠ {e}")),
        }
    }

    /// Twice a second: process states, logs, checklist.
    fn poll(&mut self) {
        if self.last_poll.elapsed() < Duration::from_millis(500) {
            return;
        }
        self.last_poll = Instant::now();
        if let Some(h) = self.host.as_mut() {
            self.host_lines = procs::tail(&h.log, 300);
            if !h.alive() {
                let stopped_on_purpose = self.host_lines.iter().any(|l| l.contains("asked to stop"));
                if !stopped_on_purpose {
                    let last = self.host_lines.iter().rev().find(|l| !l.trim().is_empty()).map(|l| procs::plain(l));
                    self.host_error = Some(format!(
                        "Sharing stopped unexpectedly{}",
                        last.map(|l| format!(": {l}")).unwrap_or_default()
                    ));
                }
                self.host = None;
            }
        }
        if let Some(v) = self.viewer.as_mut() {
            if !v.running() {
                let problem = procs::tail(&v.log, 50)
                    .iter()
                    .rev()
                    .find(|l| l.contains("Error") || l.contains(" ERROR ") || l.contains("failed"))
                    .map(|l| procs::plain(l));
                self.viewer_note = Some(problem.map_or_else(|| "Viewer closed".to_owned(), |p| format!("⚠ {p}")));
                self.viewer = None;
            }
        }
        if self.checks_at.elapsed() > Duration::from_secs(10) {
            self.checks = checks::run();
            self.checks_at = Instant::now();
        }
    }

    fn connect_card(&mut self, ui: &mut egui::Ui) {
        card(ui, "Connect to another computer", |ui| {
            if let Some(v) = self.viewer.as_mut() {
                let target = v.target.clone();
                ui.horizontal(|ui| {
                    dot(ui, GOOD);
                    ui.label(format!("Showing {target} in its own window"));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.add(button("Disconnect", STOP)).clicked() {
                            v.close();
                        }
                    });
                });
                ui.add_space(8.0);
            } else if let Some(note) = &self.viewer_note {
                ui.label(RichText::new(note).color(if note.starts_with('⚠') { WARN } else { MUTED }));
                ui.add_space(6.0);
            }

            let found = self.found.lock().map(|f| f.clone()).unwrap_or_default();
            if found.is_empty() {
                ui.horizontal(|ui| {
                    ui.add(egui::Spinner::new().color(ACCENT));
                    ui.label(RichText::new("Looking for computers that are sharing…").color(MUTED));
                });
            }
            let mut clicked: Option<(String, String)> = None;
            for h in &found {
                egui::Frame::NONE
                    .fill(BG)
                    .corner_radius(CornerRadius::same(10))
                    .inner_margin(Margin::symmetric(12, 8))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            dot(ui, ACCENT);
                            ui.vertical(|ui| {
                                ui.label(RichText::new(&h.name).strong().color(TEXT));
                                ui.label(RichText::new(h.addr.to_string()).small().color(MUTED));
                            });
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if ui.add(button("Connect", ACCENT)).clicked() {
                                    clicked = Some((h.addr.to_string(), h.name.clone()));
                                }
                            });
                        });
                    });
                ui.add_space(6.0);
            }
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new("Or type an address").color(MUTED));
                let edit = egui::TextEdit::singleline(&mut self.settings.manual_address)
                    .hint_text("192.168.1.20")
                    .desired_width(170.0);
                let r = ui.add(edit);
                let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if (ui.add(button("Connect", ACCENT)).clicked() || enter)
                    && !self.settings.manual_address.trim().is_empty()
                {
                    let a = self.settings.manual_address.trim().to_owned();
                    clicked = Some((a.clone(), a));
                }
            });
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.checkbox(&mut self.settings.fullscreen, "Full screen");
                ui.checkbox(&mut self.settings.mic, "Send my microphone");
            });
            if let Some((target, label)) = clicked {
                self.settings.save();
                self.connect(&target, &label);
            }
        });
    }

    fn share_card(&mut self, ui: &mut egui::Ui) {
        let status = read_status(&self.host_lines);
        card(ui, "Share this computer", |ui| {
            let (colour, text) = match (&self.host, &status.viewer) {
                (None, _) => (MUTED, "Off. Nobody can connect to this computer.".to_owned()),
                (Some(_), Some(v)) => (GOOD, format!("Someone is connected ({v})")),
                (Some(_), None) => (ACCENT, "On. Waiting for someone to connect.".to_owned()),
            };
            ui.horizontal(|ui| {
                dot(ui, colour);
                ui.label(RichText::new(text).color(TEXT));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if self.host.is_some() {
                        if ui.add(button("Stop sharing", STOP)).clicked() {
                            if let Some(h) = self.host.as_mut() {
                                h.stop();
                            }
                        }
                    } else if ui.add(button("Start sharing", ACCENT)).clicked() {
                        self.settings.save();
                        self.start_sharing();
                    }
                });
            });
            if let (Some(_), Some(a)) = (&self.host, &status.addresses) {
                ui.label(RichText::new(format!("If it isn't found automatically, type: {a}")).small().color(MUTED));
            }
            if let Some(e) = &self.host_error {
                ui.add_space(4.0);
                ui.label(RichText::new(format!("⚠ {e}")).color(WARN));
            }
            if self.host.is_some() {
                for w in &status.warnings {
                    ui.add_space(4.0);
                    ui.label(RichText::new(w).color(WARN));
                    if cfg!(target_os = "macos") {
                        if w.contains("Accessibility") && ui.add(button("Open Accessibility settings", EDGE)).clicked()
                        {
                            checks::open_privacy("Privacy_Accessibility");
                        }
                        if w.contains("Screen") && ui.add(button("Open Screen Recording settings", EDGE)).clicked() {
                            checks::open_privacy("Privacy_ScreenCapture");
                        }
                    }
                }
            }
            ui.add_space(8.0);
            ui.checkbox(&mut self.settings.share_on_launch, "Start sharing when this app opens");
            if cfg!(target_os = "macos") {
                ui.checkbox(
                    &mut self.settings.mac_controllers,
                    "Allow game controllers (asks for your Mac password when sharing starts)",
                );
            }
            if !self.host_lines.is_empty() {
                egui::CollapsingHeader::new(RichText::new("Details").color(MUTED)).show(ui, |ui| {
                    egui::ScrollArea::vertical().max_height(160.0).stick_to_bottom(true).show(ui, |ui| {
                        for l in &self.host_lines {
                            ui.label(RichText::new(procs::plain(l)).monospace().small().color(MUTED));
                        }
                    });
                });
            }
        });
    }

    fn extras_card(&self, ui: &mut egui::Ui) {
        if self.checks.is_empty() {
            return;
        }
        card(ui, "Optional extras", |ui| {
            for c in &self.checks {
                ui.horizontal(|ui| {
                    dot(ui, if c.ok { GOOD } else { EDGE });
                    ui.vertical(|ui| {
                        ui.label(RichText::new(c.name).strong().color(TEXT));
                        ui.label(RichText::new(c.why).small().color(MUTED));
                    });
                    if !c.ok {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.add(button("Get it", EDGE)).clicked() {
                                checks::open_url(c.url);
                            }
                        });
                    }
                });
                ui.add_space(4.0);
            }
        });
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.poll();
        egui::CentralPanel::default().frame(egui::Frame::NONE.fill(BG).inner_margin(Margin::same(20))).show(ui, |ui| {
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                ui.horizontal(|ui| {
                    if let Some(t) = &self.icon {
                        ui.add(egui::Image::new(t).fit_to_exact_size(egui::vec2(48.0, 48.0)));
                    }
                    ui.vertical(|ui| {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new("Anywhere").size(24.0).strong().color(TEXT));
                            ui.label(RichText::new(env!("CARGO_PKG_VERSION")).small().color(MUTED));
                        });
                        ui.label(
                            RichText::new("Use your other computers as if you were sitting at them.").color(MUTED),
                        );
                    });
                });
                ui.add_space(14.0);
                self.connect_card(ui);
                ui.add_space(12.0);
                self.share_card(ui);
                ui.add_space(12.0);
                self.extras_card(ui);
            });
        });
        ui.ctx().request_repaint_after(Duration::from_millis(500));
    }

    fn on_exit(&mut self) {
        self.settings.save();
        if let Some(h) = self.host.as_mut() {
            h.stop();
        }
    }
}

/// A filled status dot (drawn, so it never depends on a font glyph).
fn dot(ui: &mut egui::Ui, colour: Color32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(14.0, 14.0), egui::Sense::hover());
    ui.painter().circle_filled(rect.center(), 5.5, colour);
}

fn card(ui: &mut egui::Ui, title: &str, body: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::NONE
        .fill(CARD)
        .stroke(Stroke::new(1.0, EDGE))
        .corner_radius(CornerRadius::same(14))
        .inner_margin(Margin::same(16))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(RichText::new(title).size(17.0).strong().color(TEXT));
            ui.add_space(10.0);
            body(ui);
        });
}

fn button(text: &str, colour: Color32) -> egui::Button<'static> {
    let dark_text = colour == ACCENT || colour == GOOD;
    egui::Button::new(RichText::new(text.to_owned()).strong().color(if dark_text { BG } else { TEXT }))
        .fill(colour)
        .corner_radius(CornerRadius::same(8))
        .min_size(egui::vec2(96.0, 30.0))
}

fn style(ctx: &egui::Context) {
    let mut v = egui::Visuals::dark();
    v.panel_fill = BG;
    v.window_fill = CARD;
    v.override_text_color = Some(TEXT);
    v.selection.bg_fill = ACCENT.linear_multiply(0.4);
    v.hyperlink_color = ACCENT;
    ctx.set_theme(egui::Theme::Dark);
    ctx.set_visuals_of(egui::Theme::Dark, v);
    ctx.all_styles_mut(|s| {
        s.text_styles.insert(TextStyle::Body, FontId::proportional(15.0));
        s.text_styles.insert(TextStyle::Button, FontId::proportional(15.0));
        s.text_styles.insert(TextStyle::Small, FontId::proportional(12.5));
        s.spacing.item_spacing = egui::vec2(8.0, 6.0);
        s.spacing.button_padding = egui::vec2(14.0, 6.0);
    });
}

/// Keep the list of sharing computers fresh (every 3 s), leaving out this
/// computer itself.
fn spawn_discovery(found: Arc<Mutex<Vec<Found>>>, ctx: egui::Context) {
    let _ = std::thread::Builder::new().name("discovery".into()).spawn(move || {
        let Ok(rt) = tokio::runtime::Builder::new_current_thread().enable_all().build() else { return };
        let own: Vec<std::net::IpAddr> =
            aa_platform::lan::ipv4_interfaces().iter().map(|i| std::net::IpAddr::V4(i.ip)).collect();
        loop {
            let mut list = rt.block_on(find_hosts(DEFAULT_PORT)).unwrap_or_default();
            // AA_SHOW_SELF=1 keeps this computer in the list (testing on one machine).
            if std::env::var_os("AA_SHOW_SELF").is_none() {
                list.retain(|h| !own.contains(&h.addr.ip()) && !h.addr.ip().is_loopback());
            }
            // One row per computer: a machine with Wi-Fi and a cable answers
            // on both; keep the first (non-loopback sorts first).
            list.sort_by_key(|h| (h.name.clone(), h.addr.ip().is_loopback()));
            list.dedup_by(|a, b| a.name == b.name);
            if let Ok(mut f) = found.lock() {
                *f = list;
            }
            ctx.request_repaint();
            std::thread::sleep(Duration::from_secs(3));
        }
    });
}

fn main() -> eframe::Result {
    tracing_subscriber::fmt().with_env_filter("info").init();
    let icon = aa_platform::clipboard::png_to_rgba(include_bytes!("../../../assets/icon-256.png"))
        .map(|(width, height, rgba)| egui::IconData { rgba, width, height });
    let mut viewport = egui::ViewportBuilder::default()
        .with_title("Anywhere")
        .with_inner_size([560.0, 780.0])
        .with_min_inner_size([460.0, 560.0]);
    if let Some(i) = icon {
        viewport = viewport.with_icon(i);
    }
    let options = eframe::NativeOptions { viewport, ..Default::default() };
    eframe::run_native("Anywhere", options, Box::new(|cc| Ok(Box::new(App::new(cc)))))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_follows_the_host_log() {
        let lines: Vec<String> = [
            "2026-10-08T06:21:43.6Z  INFO aa_host::session: viewers can connect to: 192.168.1.20:7700",
            "2026-10-08T06:22:00.0Z  INFO aa_host::session: viewer connected from=192.168.1.5:5555 negotiated=…",
            "2026-10-08T06:21:43.6Z  WARN aa_platform::macos::input: allow Accessibility",
        ]
        .map(String::from)
        .to_vec();
        let s = read_status(&lines);
        assert_eq!(s.addresses.as_deref(), Some("192.168.1.20:7700"));
        assert_eq!(s.viewer.as_deref(), Some("192.168.1.5:5555"));
        assert_eq!(s.warnings, vec!["⚠ allow Accessibility".to_owned()]);
        let mut more = lines.clone();
        more.push("2026-10-08T06:30:00.0Z  INFO aa_host::session: viewer left from=192.168.1.5:5555".into());
        assert_eq!(read_status(&more).viewer, None);
    }
}
