//! Anywhere: the app people actually open.
//!
//! Four pages in one light, calm window:
//! - **Connect**: computers that are sharing appear by name; one click
//!   opens the viewer. The last computer used is remembered.
//! - **Share**: one button lets others connect to this computer, with live
//!   status, the address to type, and plain-words fixes for anything missing.
//! - **Settings**: how the viewer starts (quality, sound, microphone,
//!   full screen…) and how sharing behaves.
//! - **Extras**: optional drivers and permissions, each with a button.
//!
//! The streaming engines stay separate programs next to this one
//! (`aa-host`, `aa-viewer`), so this window is only a remote control and
//! costs next to nothing while a game streams: it redraws twice a second
//! at most, and reads only the end of the log files.

#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod checks;
mod procs;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use aa_platform::discover::{find_hosts, Found, DEFAULT_PORT};
use eframe::egui::{
    self, Align, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Layout, Margin, RichText, Sense,
    Shadow, Stroke, TextStyle, Ui,
};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Look
// ---------------------------------------------------------------------------

mod theme {
    use eframe::egui::Color32;
    pub const BG: Color32 = Color32::from_rgb(246, 247, 251);
    pub const SIDEBAR: Color32 = Color32::from_rgb(238, 240, 247);
    pub const CARD: Color32 = Color32::WHITE;
    pub const BORDER: Color32 = Color32::from_rgb(226, 229, 239);
    pub const TEXT: Color32 = Color32::from_rgb(17, 24, 39);
    pub const MUTED: Color32 = Color32::from_rgb(107, 114, 128);
    pub const FAINT: Color32 = Color32::from_rgb(156, 163, 175);
    pub const ACCENT: Color32 = Color32::from_rgb(79, 70, 229);
    pub const ACCENT_SOFT: Color32 = Color32::from_rgb(238, 237, 253);
    pub const GOOD: Color32 = Color32::from_rgb(22, 163, 74);
    pub const GOOD_SOFT: Color32 = Color32::from_rgb(220, 252, 231);
    pub const WARN: Color32 = Color32::from_rgb(180, 83, 9);
    pub const WARN_SOFT: Color32 = Color32::from_rgb(254, 243, 199);
    pub const DANGER: Color32 = Color32::from_rgb(220, 38, 38);
}
use theme::{
    ACCENT, ACCENT_SOFT, BG, BORDER, CARD, DANGER, FAINT, GOOD, GOOD_SOFT, MUTED, SIDEBAR, TEXT, WARN, WARN_SOFT,
};

/// The semibold face, for titles and buttons.
fn semibold() -> FontFamily {
    FontFamily::Name("semibold".into())
}

fn title(text: &str, size: f32) -> RichText {
    RichText::new(text).family(semibold()).size(size).color(TEXT)
}

fn style(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();
    fonts
        .font_data
        .insert("inter".into(), Arc::new(FontData::from_static(include_bytes!("../../../assets/Inter-Regular.ttf"))));
    fonts.font_data.insert(
        "inter-semibold".into(),
        Arc::new(FontData::from_static(include_bytes!("../../../assets/Inter-SemiBold.ttf"))),
    );
    // Inter first; egui's own fonts stay behind it for symbols Inter lacks.
    let fallback = fonts.families.get(&FontFamily::Proportional).cloned().unwrap_or_default();
    fonts.families.entry(FontFamily::Proportional).or_default().insert(0, "inter".into());
    let mut bold = vec!["inter-semibold".to_owned()];
    bold.extend(fallback);
    fonts.families.insert(semibold(), bold);
    ctx.set_fonts(fonts);

    let mut v = egui::Visuals::light();
    v.panel_fill = BG;
    v.window_fill = CARD;
    v.extreme_bg_color = Color32::WHITE; // text fields
    v.faint_bg_color = SIDEBAR;
    v.override_text_color = Some(TEXT);
    v.hyperlink_color = ACCENT;
    // Text selection and the filled part of sliders.
    v.selection.bg_fill = Color32::from_rgb(165, 160, 245);
    v.slider_trailing_fill = true;
    v.selection.stroke = Stroke::new(1.0, ACCENT);
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, BORDER);
    // bg_fill is also the slider rail: keep it visible on white.
    v.widgets.inactive.bg_fill = BORDER;
    v.widgets.inactive.weak_bg_fill = Color32::WHITE;
    v.widgets.inactive.bg_stroke = Stroke::new(1.0, BORDER);
    v.widgets.inactive.corner_radius = CornerRadius::same(8);
    v.widgets.hovered.bg_fill = SIDEBAR;
    v.widgets.hovered.weak_bg_fill = SIDEBAR;
    v.widgets.hovered.bg_stroke = Stroke::new(1.0, ACCENT);
    v.widgets.hovered.corner_radius = CornerRadius::same(8);
    v.widgets.active.bg_fill = ACCENT_SOFT;
    v.widgets.active.weak_bg_fill = ACCENT_SOFT;
    v.widgets.active.corner_radius = CornerRadius::same(8);
    v.widgets.open.corner_radius = CornerRadius::same(8);
    v.window_shadow = Shadow::NONE;
    v.popup_shadow = Shadow { offset: [0, 4], blur: 16, spread: 0, color: Color32::from_black_alpha(24) };
    ctx.set_theme(egui::Theme::Light);
    ctx.set_visuals_of(egui::Theme::Light, v);
    ctx.all_styles_mut(|s| {
        s.text_styles.insert(TextStyle::Body, FontId::proportional(14.5));
        s.text_styles.insert(TextStyle::Button, FontId::new(14.0, semibold()));
        s.text_styles.insert(TextStyle::Small, FontId::proportional(12.5));
        s.text_styles.insert(TextStyle::Heading, FontId::new(22.0, semibold()));
        s.text_styles.insert(TextStyle::Monospace, FontId::monospace(12.0));
        s.spacing.item_spacing = egui::vec2(10.0, 8.0);
        s.spacing.button_padding = egui::vec2(16.0, 8.0);
        s.spacing.interact_size.y = 30.0;
        s.spacing.slider_width = 220.0;
    });
}

/// A white card with a hairline border and a soft shadow.
fn card<R>(ui: &mut Ui, body: impl FnOnce(&mut Ui) -> R) -> R {
    egui::Frame::NONE
        .fill(CARD)
        .stroke(Stroke::new(1.0, BORDER))
        .corner_radius(CornerRadius::same(14))
        .shadow(Shadow { offset: [0, 1], blur: 6, spread: 0, color: Color32::from_black_alpha(10) })
        .inner_margin(Margin::same(18))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            body(ui)
        })
        .inner
}

fn section(ui: &mut Ui, heading: &str, sub: &str) {
    ui.label(title(heading, 15.5));
    if !sub.is_empty() {
        ui.label(RichText::new(sub).small().color(MUTED));
    }
    ui.add_space(8.0);
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Primary,
    Secondary,
    Danger,
}

fn button(ui: &mut Ui, text: &str, kind: Kind) -> egui::Response {
    let (fill, fg, stroke) = match kind {
        Kind::Primary => (ACCENT, Color32::WHITE, Stroke::NONE),
        Kind::Secondary => (Color32::WHITE, TEXT, Stroke::new(1.0, BORDER)),
        Kind::Danger => (Color32::WHITE, DANGER, Stroke::new(1.0, Color32::from_rgb(252, 202, 202))),
    };
    ui.add(
        egui::Button::new(RichText::new(text).family(semibold()).color(fg))
            .fill(fill)
            .stroke(stroke)
            .corner_radius(CornerRadius::same(9))
            .min_size(egui::vec2(0.0, 34.0)),
    )
}

/// A coloured pill with a dot: the one-glance status.
fn pill(ui: &mut Ui, text: &str, fg: Color32, bg: Color32) {
    egui::Frame::NONE.fill(bg).corner_radius(CornerRadius::same(255)).inner_margin(Margin::symmetric(10, 4)).show(
        ui,
        |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                let (r, _) = ui.allocate_exact_size(egui::vec2(8.0, 8.0), Sense::hover());
                ui.painter().circle_filled(r.center(), 4.0, fg);
                ui.label(RichText::new(text).small().family(semibold()).color(fg));
            });
        },
    );
}

/// An iOS-style switch; returns true when flipped.
fn toggle(ui: &mut Ui, on: &mut bool) -> bool {
    let size = egui::vec2(40.0, 22.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    if resp.clicked() {
        *on = !*on;
    }
    let t = ui.ctx().animate_bool_responsive(resp.id, *on);
    let track = egui::lerp(egui::Rgba::from(BORDER)..=egui::Rgba::from(ACCENT), t);
    let p = ui.painter();
    p.rect_filled(rect, CornerRadius::same(11), Color32::from(track));
    let x = egui::lerp(rect.left() + 11.0..=rect.right() - 11.0, t);
    p.circle_filled(egui::pos2(x, rect.center().y), 8.5, Color32::WHITE);
    resp.clicked()
}

/// A settings row: label and help text on the left, a switch on the right.
fn switch_row(ui: &mut Ui, label: &str, help: &str, on: &mut bool) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        ui.vertical(|ui| {
            ui.label(RichText::new(label).color(TEXT));
            if !help.is_empty() {
                ui.label(RichText::new(help).small().color(MUTED));
            }
        });
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| changed = toggle(ui, on));
    });
    changed
}

fn divider(ui: &mut Ui) {
    ui.add_space(4.0);
    let w = ui.available_width();
    let (r, _) = ui.allocate_exact_size(egui::vec2(w, 1.0), Sense::hover());
    ui.painter().rect_filled(r, CornerRadius::ZERO, BORDER);
    ui.add_space(4.0);
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
#[allow(clippy::struct_excessive_bools)] // settings are switches
struct Settings {
    fullscreen: bool,
    stretch: bool,
    mic: bool,
    mute_host: bool,
    show_stats: bool,
    max_mbps: f32,
    volume_boost: f32,
    share_on_launch: bool,
    /// Mac only: run sharing as administrator so controllers work.
    mac_controllers: bool,
    manual_address: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            fullscreen: false,
            stretch: false,
            mic: false,
            mute_host: false,
            show_stats: true,
            max_mbps: 80.0,
            volume_boost: 6.0,
            share_on_launch: false,
            mac_controllers: false,
            manual_address: String::new(),
        }
    }
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
    fn viewer_prefs(&self) -> procs::ViewerPrefs {
        procs::ViewerPrefs {
            fullscreen: self.fullscreen,
            stretch: self.stretch,
            mic: self.mic,
            mute_host: self.mute_host,
            show_stats: self.show_stats,
            max_mbps: self.max_mbps,
            volume_boost: self.volume_boost,
        }
    }
}

/// What the host's log says about the session.
#[derive(Debug, Default)]
struct HostStatus {
    addresses: Vec<String>,
    viewer: Option<String>,
    warnings: Vec<String>,
}

fn read_status(lines: &[String]) -> HostStatus {
    let mut s = HostStatus::default();
    for l in lines {
        if let Some((_, a)) = l.split_once("viewers can connect to: ") {
            s.addresses = a.split("  or  ").map(|x| x.trim().to_owned()).filter(|x| !x.is_empty()).collect();
        } else if l.contains("viewer connected") || l.contains("viewer reconnected") {
            let from = l.split("from=").nth(1).or_else(|| l.split("new=").nth(1));
            s.viewer = Some(from.and_then(|r| r.split_whitespace().next()).unwrap_or("someone").to_owned());
        } else if l.contains("viewer left") || l.contains("viewer timed out") {
            s.viewer = None;
        }
        if l.contains(" WARN ") || l.contains(" ERROR ") {
            let p = procs::plain(l).trim_start_matches("⚠ ").to_owned();
            if !s.warnings.contains(&p) {
                s.warnings.push(p);
            }
        }
    }
    let n = s.warnings.len();
    s.warnings.drain(..n.saturating_sub(4));
    s
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Page {
    Connect,
    Share,
    Settings,
    Extras,
}

struct App {
    page: Page,
    icon: Option<egui::TextureHandle>,
    settings: Settings,
    found: Arc<Mutex<Vec<Found>>>,
    recent: Option<String>,
    host: Option<procs::Host>,
    host_error: Option<String>,
    host_lines: Vec<String>,
    status: HostStatus,
    viewer: Option<procs::Viewer>,
    viewer_note: Option<(String, bool)>,
    checks: Vec<checks::Check>,
    checks_at: Instant,
    last_poll: Instant,
    copied: Option<Instant>,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        style(&cc.egui_ctx);
        let found: Arc<Mutex<Vec<Found>>> = Arc::default();
        spawn_discovery(Arc::clone(&found), cc.egui_ctx.clone());
        let icon =
            aa_platform::clipboard::png_to_rgba(include_bytes!("../../../assets/icon-256.png")).map(|(w, h, px)| {
                let img = egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &px);
                cc.egui_ctx.load_texture("icon", img, egui::TextureOptions::LINEAR)
            });
        // AA_PAGE picks the first page (used for screenshots in testing).
        let page = match std::env::var("AA_PAGE").as_deref() {
            Ok("share") => Page::Share,
            Ok("settings") => Page::Settings,
            Ok("extras") => Page::Extras,
            _ => Page::Connect,
        };
        let mut app = Self {
            page,
            icon,
            settings: Settings::load(),
            found,
            recent: aa_platform::discover::remembered().map(|a| a.to_string()),
            host: None,
            host_error: None,
            host_lines: Vec::new(),
            status: HostStatus::default(),
            viewer: None,
            viewer_note: None,
            checks: checks::run(),
            checks_at: Instant::now(),
            last_poll: Instant::now(),
            copied: None,
        };
        if app.settings.share_on_launch {
            app.start_sharing();
        }
        app
    }

    fn start_sharing(&mut self) {
        self.host_error = None;
        self.host_lines.clear();
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
        self.settings.save();
        match procs::Viewer::start(target, self.settings.viewer_prefs()) {
            Ok(v) => {
                self.viewer_note = Some((format!("Opening {label}…"), false));
                self.recent = Some(target.to_owned());
                self.viewer = Some(v);
            }
            Err(e) => self.viewer_note = Some((e.to_string(), true)),
        }
    }

    /// Twice a second: process states, logs, checklist.
    fn poll(&mut self) {
        if self.last_poll.elapsed() < Duration::from_millis(500) {
            return;
        }
        self.last_poll = Instant::now();
        if let Some(h) = self.host.as_mut() {
            let mut lines = procs::head(&h.log, 40);
            let tail = procs::tail(&h.log, 300);
            if lines.len() >= 40 {
                lines.extend(tail);
            } else {
                lines = tail;
            }
            self.host_lines = lines;
            self.status = read_status(&self.host_lines);
            if !h.alive() {
                let on_purpose = self.host_lines.iter().any(|l| l.contains("asked to stop"));
                if !on_purpose {
                    let last = self.host_lines.iter().rev().find(|l| !l.trim().is_empty()).map(|l| procs::plain(l));
                    self.host_error = Some(format!(
                        "Sharing stopped unexpectedly{}",
                        last.map(|l| format!(": {l}")).unwrap_or_default()
                    ));
                }
                self.host = None;
                self.status = HostStatus::default();
            }
        }
        if let Some(v) = self.viewer.as_mut() {
            if v.running() {
                if self.viewer_note.as_ref().is_some_and(|(t, _)| t.starts_with("Opening")) {
                    self.viewer_note = None;
                }
            } else {
                let problem = procs::tail(&v.log, 60)
                    .iter()
                    .rev()
                    .find(|l| l.contains("Error") || l.contains(" ERROR ") || l.contains("failed"))
                    .map(|l| procs::plain(l));
                self.viewer_note =
                    Some(problem.map_or_else(|| ("The viewer was closed.".to_owned(), false), |p| (p, true)));
                self.viewer = None;
            }
        }
        if self.checks_at.elapsed() > Duration::from_secs(10) {
            self.checks = checks::run();
            self.checks_at = Instant::now();
        }
    }

    // -----------------------------------------------------------------------
    // Sidebar
    // -----------------------------------------------------------------------

    fn sidebar(&mut self, ui: &mut Ui) {
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            if let Some(t) = &self.icon {
                ui.add(egui::Image::new(t).fit_to_exact_size(egui::vec2(34.0, 34.0)));
            }
            ui.vertical(|ui| {
                ui.add_space(1.0);
                ui.label(title("Anywhere", 17.0));
                ui.label(RichText::new(format!("Version {}", env!("CARGO_PKG_VERSION"))).small().color(FAINT));
            });
        });
        ui.add_space(22.0);
        let missing = self.checks.iter().filter(|c| !c.ok).count();
        for (page, label, badge) in [
            (Page::Connect, "Connect", None),
            (Page::Share, "Share", self.host.is_some().then_some(("ON", GOOD))),
            (Page::Settings, "Settings", None),
            (Page::Extras, "Extras", (missing > 0).then_some(("", WARN))),
        ] {
            let selected = self.page == page;
            let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 38.0), Sense::click());
            let bg = if selected {
                Color32::WHITE
            } else if resp.hovered() {
                Color32::from_rgb(230, 233, 243)
            } else {
                Color32::TRANSPARENT
            };
            let p = ui.painter();
            p.rect_filled(rect, CornerRadius::same(9), bg);
            if selected {
                p.rect_stroke(rect, CornerRadius::same(9), Stroke::new(1.0, BORDER), egui::StrokeKind::Inside);
                let bar = egui::Rect::from_min_size(rect.left_top() + egui::vec2(0.0, 9.0), egui::vec2(3.0, 20.0));
                p.rect_filled(bar, CornerRadius::same(2), ACCENT);
            }
            let font = if selected { FontId::new(14.0, semibold()) } else { FontId::proportional(14.0) };
            p.text(
                rect.left_center() + egui::vec2(14.0, 0.0),
                egui::Align2::LEFT_CENTER,
                label,
                font,
                if selected { TEXT } else { MUTED },
            );
            if let Some((text, colour)) = badge {
                let c = rect.right_center() - egui::vec2(16.0, 0.0);
                if text.is_empty() {
                    p.circle_filled(c, 4.0, colour);
                } else {
                    p.text(c, egui::Align2::RIGHT_CENTER, text, FontId::new(11.5, semibold()), colour);
                }
            }
            if resp.clicked() {
                self.page = page;
            }
            ui.add_space(2.0);
        }
        ui.with_layout(Layout::bottom_up(Align::Min), |ui| {
            let (text, fg) = match (&self.host, &self.status.viewer, &self.viewer) {
                (_, _, Some(v)) => (format!("Viewing {}", v.target.split(':').next().unwrap_or("")), GOOD),
                (Some(_), Some(_), None) => ("Someone is connected here".to_owned(), GOOD),
                (Some(_), None, None) => ("Sharing is on".to_owned(), ACCENT),
                (None, _, None) => ("Not connected".to_owned(), FAINT),
            };
            ui.horizontal(|ui| {
                let (r, _) = ui.allocate_exact_size(egui::vec2(8.0, 8.0), Sense::hover());
                ui.painter().circle_filled(r.center(), 4.0, fg);
                ui.label(RichText::new(text).small().color(MUTED));
            });
        });
    }

    // -----------------------------------------------------------------------
    // Pages
    // -----------------------------------------------------------------------

    fn page_header(ui: &mut Ui, heading: &str, sub: &str) {
        ui.label(title(heading, 24.0));
        ui.label(RichText::new(sub).color(MUTED));
        ui.add_space(18.0);
    }

    #[allow(clippy::too_many_lines)] // one page, top to bottom
    fn connect_page(&mut self, ui: &mut Ui) {
        Self::page_header(ui, "Connect", "Use another computer as if you were sitting at it.");
        let mut go: Option<(String, String)> = None;

        if let Some(v) = self.viewer.as_mut() {
            let target = v.target.clone();
            card(ui, |ui| {
                ui.horizontal(|ui| {
                    pill(ui, "Connected", GOOD, GOOD_SOFT);
                    ui.label(RichText::new(format!("Showing {target} in its own window")).color(TEXT));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if button(ui, "Disconnect", Kind::Danger).clicked() {
                            v.close();
                        }
                    });
                });
            });
            ui.add_space(12.0);
        } else if let Some((note, bad)) = &self.viewer_note {
            let (fg, bg) = if *bad { (WARN, WARN_SOFT) } else { (MUTED, SIDEBAR) };
            egui::Frame::NONE
                .fill(bg)
                .corner_radius(CornerRadius::same(10))
                .inner_margin(Margin::symmetric(14, 10))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.label(RichText::new(note).color(fg));
                });
            ui.add_space(12.0);
        }

        card(ui, |ui| {
            section(ui, "Computers on your network", "Computers appear here when Anywhere is sharing on them.");
            let found = self.found.lock().map(|f| f.clone()).unwrap_or_default();
            if found.is_empty() {
                ui.horizontal(|ui| {
                    // A slow blink, not a spinner: spinners redraw the
                    // window 60 times a second, forever.
                    let on = ui.input(|i| i.time) as u64 % 2 == 0;
                    let (r, _) = ui.allocate_exact_size(egui::vec2(10.0, 10.0), Sense::hover());
                    ui.painter().circle_filled(r.center(), 4.5, if on { ACCENT } else { ACCENT_SOFT });
                    ui.label(
                        RichText::new("Looking… Open Anywhere on the other computer and turn on sharing.").color(MUTED),
                    );
                });
            }
            for (i, h) in found.iter().enumerate() {
                if i > 0 {
                    divider(ui);
                }
                ui.horizontal(|ui| {
                    computer_glyph(ui, ACCENT);
                    ui.vertical(|ui| {
                        ui.label(RichText::new(&h.name).family(semibold()).color(TEXT));
                        ui.label(RichText::new(h.addr.ip().to_string()).small().color(MUTED));
                    });
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if button(ui, "Connect", Kind::Primary).clicked() {
                            go = Some((h.addr.to_string(), h.name.clone()));
                        }
                    });
                });
            }
        });
        ui.add_space(12.0);

        card(ui, |ui| {
            section(ui, "Connect by address", "If a computer doesn't show up, type the address its Share page shows.");
            ui.horizontal(|ui| {
                let edit = egui::TextEdit::singleline(&mut self.settings.manual_address)
                    .hint_text("e.g. 192.168.1.20")
                    .margin(Margin::symmetric(10, 7))
                    .desired_width((ui.available_width() - 120.0).max(140.0));
                let r = ui.add(edit);
                let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if (button(ui, "Connect", Kind::Primary).clicked() || enter)
                    && !self.settings.manual_address.trim().is_empty()
                {
                    let a = self.settings.manual_address.trim().to_owned();
                    go = Some((a.clone(), a));
                }
            });
            if let Some(r) = self.recent.clone() {
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Last used").small().color(MUTED));
                    if ui.link(RichText::new(&r).small()).clicked() {
                        go = Some((r.clone(), r.clone()));
                    }
                });
            }
        });
        ui.add_space(12.0);

        card(ui, |ui| {
            section(ui, "For this connection", "");
            let s = &mut self.settings;
            let mut changed = switch_row(ui, "Full screen", "", &mut s.fullscreen);
            divider(ui);
            changed |=
                switch_row(ui, "Send my microphone", "Your voice reaches the other computer's apps.", &mut s.mic);
            divider(ui);
            changed |= switch_row(
                ui,
                "Mute the other computer's speakers",
                "So nobody near it hears everything twice.",
                &mut s.mute_host,
            );
            if changed {
                s.save();
            }
            ui.add_space(4.0);
            ui.label(
                RichText::new(format!("While connected, {} opens these settings and more.", hotkey()))
                    .small()
                    .color(FAINT),
            );
        });

        if let Some((target, label)) = go {
            self.connect(&target, &label);
        }
    }

    #[allow(clippy::too_many_lines)] // one page, top to bottom
    fn share_page(&mut self, ui: &mut Ui) {
        Self::page_header(ui, "Share this computer", "Let your other computers see and control this one.");
        let sharing = self.host.is_some();
        card(ui, |ui| {
            ui.horizontal(|ui| {
                let (text, fg, bg) = match (sharing, &self.status.viewer) {
                    (false, _) => ("Off", MUTED, SIDEBAR),
                    (true, Some(_)) => ("Someone is connected", GOOD, GOOD_SOFT),
                    (true, None) => ("Waiting for a connection", ACCENT, ACCENT_SOFT),
                };
                pill(ui, text, fg, bg);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if sharing {
                        if button(ui, "Stop sharing", Kind::Danger).clicked() {
                            if let Some(h) = self.host.as_mut() {
                                h.stop();
                            }
                        }
                    } else if button(ui, "Start sharing", Kind::Primary).clicked() {
                        self.settings.save();
                        self.start_sharing();
                    }
                });
            });
            ui.add_space(10.0);
            let line = match (sharing, &self.status.viewer) {
                (false, _) => "Nobody can connect to this computer.".to_owned(),
                (true, Some(v)) => format!("Connected from {}.", v.split(':').next().unwrap_or(v)),
                (true, None) => "Open Anywhere on your other computer and pick this one.".to_owned(),
            };
            ui.label(RichText::new(line).color(TEXT));

            if sharing && !self.status.addresses.is_empty() {
                ui.add_space(10.0);
                ui.label(
                    RichText::new("If it isn't found automatically, type one of these there:").small().color(MUTED),
                );
                ui.horizontal_wrapped(|ui| {
                    for a in self.status.addresses.clone() {
                        let ip = a.trim_end_matches(":7700").to_owned();
                        let r = ui.add(
                            egui::Button::new(RichText::new(&ip).monospace().color(TEXT))
                                .fill(SIDEBAR)
                                .stroke(Stroke::new(1.0, BORDER))
                                .corner_radius(CornerRadius::same(7)),
                        );
                        if r.on_hover_text("Click to copy").clicked() {
                            ui.ctx().copy_text(ip);
                            self.copied = Some(Instant::now());
                        }
                    }
                    if self.copied.is_some_and(|t| t.elapsed() < Duration::from_secs(2)) {
                        ui.label(RichText::new("Copied").small().color(GOOD));
                    }
                });
            }
        });

        let problems: Vec<String> =
            self.host_error.iter().cloned().chain(self.status.warnings.iter().cloned()).collect();
        for p in problems {
            ui.add_space(10.0);
            egui::Frame::NONE
                .fill(WARN_SOFT)
                .corner_radius(CornerRadius::same(12))
                .inner_margin(Margin::same(14))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.label(RichText::new(&p).color(WARN));
                    if cfg!(target_os = "macos") {
                        if p.contains("Accessibility")
                            && button(ui, "Open Accessibility settings", Kind::Secondary).clicked()
                        {
                            checks::open_privacy("Privacy_Accessibility");
                        }
                        if (p.contains("record the screen") || p.contains("Screen"))
                            && button(ui, "Open Screen Recording settings", Kind::Secondary).clicked()
                        {
                            checks::open_privacy("Privacy_ScreenCapture");
                        }
                    }
                });
        }

        ui.add_space(12.0);
        card(ui, |ui| {
            section(ui, "Sharing options", "");
            let mut changed =
                switch_row(ui, "Start sharing when Anywhere opens", "", &mut self.settings.share_on_launch);
            if cfg!(target_os = "macos") {
                divider(ui);
                changed |= switch_row(
                    ui,
                    "Allow game controllers",
                    "macOS asks for your password when sharing starts. Takes effect next time you start sharing.",
                    &mut self.settings.mac_controllers,
                );
            }
            if changed {
                self.settings.save();
            }
        });

        if !self.host_lines.is_empty() {
            ui.add_space(12.0);
            card(ui, |ui| {
                egui::CollapsingHeader::new(title("Activity", 14.5)).default_open(false).show(ui, |ui| {
                    egui::ScrollArea::vertical().max_height(200.0).stick_to_bottom(true).show(ui, |ui| {
                        for l in &self.host_lines {
                            ui.label(RichText::new(procs::plain(l)).monospace().color(MUTED));
                        }
                    });
                });
            });
        }
    }

    fn settings_page(&mut self, ui: &mut Ui) {
        Self::page_header(ui, "Settings", "How connections start. You can change most of these while connected too.");
        let s = &mut self.settings;
        let mut changed = false;
        card(ui, |ui| {
            section(ui, "Picture", "");
            changed |= switch_row(ui, "Full screen", "Start connections full screen.", &mut s.fullscreen);
            divider(ui);
            changed |= switch_row(
                ui,
                "Fill the window",
                "Stretch instead of keeping the shape (may distort).",
                &mut s.stretch,
            );
            divider(ui);
            changed |= switch_row(ui, "Show statistics", "Frame rate and speed in a corner.", &mut s.show_stats);
            divider(ui);
            ui.label(RichText::new("Maximum quality").color(TEXT));
            ui.label(
                RichText::new("Upper limit for the stream. Lower it on slow Wi-Fi; the stream also adapts by itself.")
                    .small()
                    .color(MUTED),
            );
            changed |= ui
                .add(egui::Slider::new(&mut s.max_mbps, 5.0..=150.0).logarithmic(true).suffix(" Mbps").max_decimals(0))
                .changed();
        });
        ui.add_space(12.0);
        card(ui, |ui| {
            section(ui, "Sound", "");
            ui.label(RichText::new("Volume boost").color(TEXT));
            ui.label(RichText::new("Makes the other computer louder without distortion.").small().color(MUTED));
            changed |=
                ui.add(egui::Slider::new(&mut s.volume_boost, 0.0..=18.0).suffix(" dB").max_decimals(0)).changed();
            divider(ui);
            changed |=
                switch_row(ui, "Send my microphone", "Your voice reaches the other computer's apps.", &mut s.mic);
            divider(ui);
            changed |=
                switch_row(ui, "Mute the other computer's speakers", "Its sound plays only here.", &mut s.mute_host);
        });
        ui.add_space(12.0);
        card(ui, |ui| {
            section(ui, "Sharing", "");
            changed |= switch_row(ui, "Start sharing when Anywhere opens", "", &mut s.share_on_launch);
            if cfg!(target_os = "macos") {
                divider(ui);
                changed |= switch_row(
                    ui,
                    "Allow game controllers",
                    "Asks for your Mac password when sharing starts.",
                    &mut s.mac_controllers,
                );
            }
        });
        ui.add_space(12.0);
        card(ui, |ui| {
            section(ui, "Help", "");
            ui.horizontal_wrapped(|ui| {
                if button(ui, "Open log folder", Kind::Secondary).clicked() {
                    checks::open_url(&procs::data_dir().display().to_string());
                }
                if button(ui, "Check for updates", Kind::Secondary).clicked() {
                    checks::open_url("https://github.com/minute-creative/anywhere-alternative/releases/latest");
                }
                if button(ui, "Reset settings", Kind::Secondary).clicked() {
                    *s = Settings::default();
                    changed = true;
                }
            });
        });
        if changed {
            s.save();
        }
    }

    fn extras_page(&mut self, ui: &mut Ui) {
        Self::page_header(
            ui,
            "Extras",
            "Free add-ons for controllers and the microphone. Everything else works without them.",
        );
        if cfg!(target_os = "macos") {
            card(ui, |ui| {
                section(ui, "Permissions", "macOS asks for these the first time you share. Turn on Anywhere in each.");
                for (name, why, pane) in [
                    (
                        "Screen & System Audio Recording",
                        "Lets others see this Mac and hear its sound.",
                        "Privacy_ScreenCapture",
                    ),
                    ("Accessibility", "Lets others use this Mac's mouse and keyboard.", "Privacy_Accessibility"),
                    (
                        "Microphone",
                        "Lets you send your voice when connected to another computer.",
                        "Privacy_Microphone",
                    ),
                ] {
                    ui.horizontal(|ui| {
                        ui.vertical(|ui| {
                            ui.label(RichText::new(name).color(TEXT));
                            ui.label(RichText::new(why).small().color(MUTED));
                        });
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if button(ui, "Open", Kind::Secondary).clicked() {
                                checks::open_privacy(pane);
                            }
                        });
                    });
                    divider(ui);
                }
            });
            ui.add_space(12.0);
        }
        card(ui, |ui| {
            section(ui, "Add-ons", "");
            if self.checks.is_empty() {
                ui.label(RichText::new("Nothing extra is needed on this computer.").color(MUTED));
            }
            let n = self.checks.len();
            for (i, c) in self.checks.iter().enumerate() {
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.label(RichText::new(c.name).color(TEXT));
                        ui.label(RichText::new(c.why).small().color(MUTED));
                    });
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if c.ok {
                            pill(ui, "Installed", GOOD, GOOD_SOFT);
                        } else if button(ui, "Get it", Kind::Secondary).clicked() {
                            checks::open_url(c.url);
                        }
                    });
                });
                if i + 1 < n {
                    divider(ui);
                }
            }
        });
    }
}

/// A small monitor drawn with shapes (crisp at any scale, no icon font).
fn computer_glyph(ui: &mut Ui, colour: Color32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(36.0, 36.0), Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, CornerRadius::same(9), ACCENT_SOFT);
    let screen = egui::Rect::from_center_size(rect.center() - egui::vec2(0.0, 2.5), egui::vec2(18.0, 12.0));
    p.rect_stroke(screen, CornerRadius::same(2), Stroke::new(1.8, colour), egui::StrokeKind::Inside);
    let base = rect.center() + egui::vec2(0.0, 7.5);
    p.line_segment([base - egui::vec2(5.0, 0.0), base + egui::vec2(5.0, 0.0)], Stroke::new(1.8, colour));
}

fn hotkey() -> &'static str {
    if cfg!(target_os = "macos") {
        "Cmd+Shift+S"
    } else {
        "Ctrl+Shift+S"
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        self.poll();
        egui::Panel::left("nav")
            .exact_size(212.0)
            .resizable(false)
            .frame(egui::Frame::NONE.fill(SIDEBAR).inner_margin(Margin::same(16)).stroke(Stroke::new(1.0, BORDER)))
            .show(ui, |ui| self.sidebar(ui));
        egui::CentralPanel::default().frame(egui::Frame::NONE.fill(BG).inner_margin(Margin::symmetric(28, 24))).show(
            ui,
            |ui| {
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    // Leave room for the scroll bar; cap the reading width.
                    ui.set_max_width((ui.available_width() - 16.0).min(660.0));
                    match self.page {
                        Page::Connect => self.connect_page(ui),
                        Page::Share => self.share_page(ui),
                        Page::Settings => self.settings_page(ui),
                        Page::Extras => self.extras_page(ui),
                    }
                    ui.add_space(12.0);
                });
            },
        );
        // Only what changes by itself needs a timer: status twice a second.
        ui.ctx().request_repaint_after(Duration::from_millis(500));
    }

    fn on_exit(&mut self) {
        self.settings.save();
        if let Some(h) = self.host.as_mut() {
            h.stop();
        }
    }
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
            let changed = found.lock().map(|mut f| {
                let changed = *f != list;
                *f = list;
                changed
            });
            if changed.unwrap_or(false) {
                ctx.request_repaint();
            }
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
        .with_inner_size([900.0, 680.0])
        .with_min_inner_size([760.0, 520.0]);
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
            "2026-10-08T06:21:43.6Z  INFO aa_host::session: viewers can connect to: 192.168.1.20:7700  or  10.0.0.4:7700",
            "2026-10-08T06:22:00.0Z  INFO aa_host::session: viewer connected from=192.168.1.5:5555 negotiated=…",
            "2026-10-08T06:21:43.6Z  WARN aa_platform::macos::input: allow Accessibility",
        ]
        .map(String::from)
        .to_vec();
        let s = read_status(&lines);
        assert_eq!(s.addresses, vec!["192.168.1.20:7700", "10.0.0.4:7700"]);
        assert_eq!(s.viewer.as_deref(), Some("192.168.1.5:5555"));
        assert_eq!(s.warnings, vec!["allow Accessibility".to_owned()]);
        let mut more = lines.clone();
        more.push("2026-10-08T06:30:00.0Z  INFO aa_host::session: viewer left from=192.168.1.5:5555".into());
        assert_eq!(read_status(&more).viewer, None);
    }

    #[test]
    fn old_settings_files_still_load() {
        let s: Settings = serde_json::from_str(r#"{"mic":true}"#).unwrap();
        assert!(s.mic);
        assert!((s.max_mbps - 80.0).abs() < f32::EPSILON, "new fields get defaults");
    }
}
