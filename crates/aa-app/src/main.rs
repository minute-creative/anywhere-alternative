//! Anywhere: the app people actually open.
//!
//! Four pages in one light, calm window:
//! - **Connect**: computers that are sharing appear by name, at home or
//!   through Tailscale. The first time, you type the code the other one
//!   shows; after that one click connects.
//! - **Share**: one button lets your paired computers connect, with the
//!   pairing code for new ones, live status, and plain-words fixes.
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
mod service;
mod update;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use aa_platform::discover::{find_hosts, Found, DEFAULT_PORT};
use aa_platform::trust;
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
/// The text gets the width minus the switch, so long help wraps before it
/// instead of running underneath.
fn switch_row(ui: &mut Ui, label: &str, help: &str, on: &mut bool) -> bool {
    let mut changed = false;
    let text_w = (ui.available_width() - 64.0).max(120.0);
    ui.horizontal(|ui| {
        ui.vertical(|ui| {
            ui.set_width(text_w);
            ui.add(egui::Label::new(RichText::new(label).color(TEXT)).wrap());
            if !help.is_empty() {
                ui.add(egui::Label::new(RichText::new(help).small().color(MUTED)).wrap());
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
    /// Install new versions by themselves when nobody is connected.
    auto_update: bool,
    /// The "finish setting up" card was answered.
    addons_offered: bool,
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
            auto_update: true,
            addons_offered: false,
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
            let name = l.split("name=\"").nth(1).and_then(|r| r.split('"').next()).filter(|n| !n.is_empty());
            let who = name.or_else(|| from.and_then(|r| r.split_whitespace().next())).unwrap_or("someone");
            if l.contains("viewer connected") || s.viewer.is_none() {
                s.viewer = Some(who.to_owned());
            }
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

/// Results of work done in the background (so the window never freezes).
enum Bg {
    Probed(String, Result<Found, String>),
    Paired(String, Result<String, String>),
    Service(Result<(), String>),
    Updated(Result<update::Applied, String>),
    Addons(Result<(), String>),
}

/// "Type the code shown on <name>".
#[derive(Debug, Clone)]
struct PairPrompt {
    target: String,
    name: String,
    code: String,
    busy: bool,
    error: Option<String>,
}

#[allow(clippy::struct_excessive_bools)] // independent on/off states
struct App {
    page: Page,
    bg: Arc<Mutex<Vec<Bg>>>,
    ctx: egui::Context,
    pair: Option<PairPrompt>,
    paired_hosts: Vec<trust::PairedHost>,
    paired_viewers: Vec<trust::PairedViewer>,
    pair_code: String,
    /// Mac: sharing is always on (login screen, after a restart).
    service_on: bool,
    service_busy: bool,
    service_error: Option<String>,
    filevault: Option<bool>,
    /// Windows: Anywhere starts when you sign in.
    sign_in: bool,
    lists_at: Instant,
    updater: Arc<update::Updater>,
    /// "Updating to 0.5.0…" while an update is under way.
    updating: Option<String>,
    /// A version that failed (or was declined) this session: don't retry it.
    update_skip: Option<String>,
    update_error: Option<String>,
    addons_busy: bool,
    addons_note: Option<(String, bool)>,
    viewing_note_at: Instant,
    icon: Option<egui::TextureHandle>,
    settings: Settings,
    found: Arc<Mutex<Vec<Found>>>,
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
            bg: Arc::default(),
            ctx: cc.egui_ctx.clone(),
            pair: None,
            paired_hosts: trust::paired_hosts(),
            paired_viewers: trust::paired_viewers(),
            pair_code: trust::pair_code(),
            service_on: service::installed(),
            service_busy: false,
            service_error: None,
            filevault: service::filevault_on(),
            sign_in: service::starts_at_sign_in(),
            lists_at: Instant::now(),
            updater: Arc::default(),
            updating: None,
            update_skip: None,
            update_error: None,
            addons_busy: false,
            addons_note: None,
            viewing_note_at: Instant::now(),
            icon,
            settings: Settings::load(),
            found,
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
        update::spawn_checker(Arc::clone(&app.updater), cc.egui_ctx.clone());
        let background = std::env::args().any(|a| a == "--background");
        // Reopened by an update: as it was (sharing or not), minimised.
        let after_update = std::env::args().any(|a| a == "--after-update");
        let resume_sharing = after_update && update::take_resume().is_some_and(|r| r.sharing);
        if (app.settings.share_on_launch || background || resume_sharing) && !app.service_on {
            app.start_sharing();
        }
        if background || after_update {
            cc.egui_ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
        }
        app
    }

    /// Nobody connected either way, nothing half done: safe to update.
    fn idle(&self) -> bool {
        self.viewer.is_none()
            && self.status.viewer.is_none()
            && self.pair.is_none()
            && !self.service_busy
            && !self.addons_busy
            && !aa_platform::update::someone_connected()
    }

    /// Install a newer version when it's safe.
    fn maybe_update(&mut self) {
        if !self.settings.auto_update || self.updating.is_some() || !self.idle() {
            return;
        }
        let Some(rel) = self.updater.available() else { return };
        if self.update_skip.as_deref() == Some(rel.version.as_str()) {
            return;
        }
        self.updating = Some(format!("Updating to {}…", rel.version));
        self.update_error = None;
        let service_on = self.service_on;
        self.spawn(move |_| Bg::Updated(update::apply(rel, service_on).map_err(|e| e.to_string())));
    }

    /// Close for the update; it reopens the app afterwards.
    fn quit_for_update(&mut self) {
        update::save_resume(&update::Resume { sharing: self.host.is_some() });
        self.settings.save();
        self.ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }

    /// Run `work` on its own thread; its result arrives in `poll`.
    fn spawn(&self, work: impl FnOnce(&tokio::runtime::Runtime) -> Bg + Send + 'static) {
        let (bg, ctx) = (Arc::clone(&self.bg), self.ctx.clone());
        let _ = std::thread::Builder::new().name("app-task".into()).spawn(move || {
            let Ok(rt) = tokio::runtime::Builder::new_current_thread().enable_all().build() else { return };
            let r = work(&rt);
            if let Ok(mut q) = bg.lock() {
                q.push(r);
            }
            ctx.request_repaint();
        });
    }

    /// Connect to a computer: straight away if paired, else ask for its code.
    fn request_connect(&mut self, target: &str, label: &str, found: Option<&Found>) {
        match found.and_then(|f| f.key) {
            Some(k) if trust::paired_host(&k).is_some() => self.connect(target, label),
            Some(_) => self.ask_code(target, label),
            None => {
                // Typed address, or an older Anywhere: ask it who it is first.
                self.viewer_note = Some((format!("Checking {label}…"), false));
                let t = target.to_owned();
                self.spawn(move |rt| {
                    let r = match parse_target(&t) {
                        Some(addr) => rt.block_on(aa_platform::pairing::probe(addr)).map_err(|e| e.to_string()),
                        None => Err(format!("\"{t}\" isn't an address (for example 192.168.1.20)")),
                    };
                    Bg::Probed(t, r)
                });
            }
        }
    }

    fn ask_code(&mut self, target: &str, name: &str) {
        self.pair = Some(PairPrompt {
            target: target.to_owned(),
            name: name.to_owned(),
            code: String::new(),
            busy: false,
            error: None,
        });
        self.page = Page::Connect;
    }

    fn submit_code(&mut self) {
        let Some(p) = self.pair.as_mut() else { return };
        let Some(addr) = parse_target(&p.target) else { return };
        p.busy = true;
        p.error = None;
        let (t, code) = (p.target.clone(), p.code.clone());
        self.spawn(move |rt| {
            Bg::Paired(
                t,
                rt.block_on(aa_platform::pairing::pair(addr, &code)).map(|w| w.name).map_err(|e| e.to_string()),
            )
        });
    }

    fn install_addons(&mut self) {
        self.addons_busy = true;
        self.addons_note = Some(("Installing… (your computer asks to allow it once)".to_owned(), false));
        self.settings.addons_offered = true;
        self.settings.save();
        self.spawn(|_| Bg::Addons(service::install_addons().map_err(|e| e.to_string())));
    }

    /// Add-ons that can be installed for you and aren't yet.
    fn missing_addons(&self) -> usize {
        self.checks.iter().filter(|c| !c.ok && !c.name.starts_with("HEVC")).count()
    }

    fn addons_card(&mut self, ui: &mut Ui, first_run: bool) {
        let mut install = false;
        let mut later = false;
        card(ui, |ui| {
            if first_run {
                section(
                    ui,
                    "Finish setting up",
                    "Free add-ons for controllers, the microphone and connecting from anywhere. Downloaded from \
                     their makers.",
                );
            } else {
                section(ui, "Install what's missing", "One click; your computer asks to allow it once.");
            }
            ui.horizontal(|ui| {
                let label = if self.addons_busy { "Installing…" } else { "Install add-ons" };
                if ui.add_enabled_ui(!self.addons_busy, |ui| button(ui, label, Kind::Primary)).inner.clicked() {
                    install = true;
                }
                if first_run && button(ui, "Not now", Kind::Secondary).clicked() {
                    later = true;
                }
            });
            if let Some((t, bad)) = &self.addons_note {
                ui.label(RichText::new(t).small().color(if *bad { DANGER } else { MUTED }));
            }
        });
        if install {
            self.install_addons();
        }
        if later {
            self.settings.addons_offered = true;
            self.settings.save();
        }
    }

    fn set_service(&mut self, on: bool) {
        self.service_busy = true;
        self.service_error = None;
        if on {
            // The always-on copy needs the port this window's copy holds.
            if let Some(h) = self.host.as_mut() {
                h.stop();
            }
        }
        self.spawn(move |_| {
            Bg::Service(if on { service::install() } else { service::uninstall() }.map_err(|e| e.to_string()))
        });
    }

    fn handle_bg(&mut self) {
        let done: Vec<Bg> = self.bg.lock().map(|mut q| std::mem::take(&mut *q)).unwrap_or_default();
        for r in done {
            match r {
                Bg::Probed(t, Ok(f)) => {
                    self.viewer_note = None;
                    match f.key {
                        Some(k) if trust::paired_host(&k).is_some() => self.connect(&t, &f.name),
                        Some(_) => self.ask_code(&t, &f.name),
                        None => {
                            self.viewer_note = Some((
                                format!("{} runs an older Anywhere. Update it from the Releases page first.", f.name),
                                true,
                            ));
                        }
                    }
                }
                Bg::Probed(_, Err(e)) => self.viewer_note = Some((e, true)),
                Bg::Paired(t, Ok(name)) => {
                    self.pair = None;
                    self.refresh_lists();
                    self.connect(&t, &name);
                }
                Bg::Paired(_, Err(e)) => {
                    if let Some(p) = self.pair.as_mut() {
                        p.busy = false;
                        p.error = Some(e);
                    }
                }
                Bg::Updated(Ok(update::Applied::Quit)) => self.quit_for_update(),
                Bg::Updated(Ok(update::Applied::Requested)) => {
                    self.updating = Some("Installing the update…".to_owned());
                }
                Bg::Updated(Err(e)) => {
                    self.update_skip = self.updater.available().map(|r| r.version);
                    self.updating = None;
                    if e != "cancelled" {
                        self.update_error = Some(format!("The update didn't install: {e}"));
                    }
                }
                Bg::Addons(r) => {
                    self.addons_busy = false;
                    self.checks = checks::run();
                    self.addons_note = Some(match r {
                        Ok(()) => ("Add-ons installed.".to_owned(), false),
                        Err(e) if e == "cancelled" => ("Cancelled.".to_owned(), false),
                        Err(e) => (format!("Some add-ons didn't install: {e}"), true),
                    });
                }
                Bg::Service(r) => {
                    self.service_busy = false;
                    self.service_on = service::installed();
                    match r {
                        Err(e) if e != "cancelled" => self.service_error = Some(e),
                        _ => {}
                    }
                    if !self.service_on && self.settings.share_on_launch && self.host.is_none() {
                        self.start_sharing();
                    }
                }
            }
        }
    }

    fn refresh_lists(&mut self) {
        self.paired_hosts = trust::paired_hosts();
        self.paired_viewers = trust::paired_viewers();
        self.pair_code = trust::pair_code();
        self.lists_at = Instant::now();
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
        match procs::Viewer::start(target, label, self.settings.viewer_prefs()) {
            Ok(v) => {
                self.viewer_note = Some((format!("Opening {label}…"), false));
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
        self.handle_bg();
        // Windows service installing an update: close; it reopens us.
        if cfg!(target_os = "windows") && self.service_on && aa_platform::update::marker::running().exists() {
            aa_platform::update::note(&aa_platform::update::marker::relaunch(), true);
            self.quit_for_update();
            return;
        }
        // Tell the service we're watching another computer (no updates now).
        if self.service_on && self.viewing_note_at.elapsed() > Duration::from_secs(30) {
            self.viewing_note_at = Instant::now();
            aa_platform::update::note(&aa_platform::update::marker::viewing(), self.viewer.is_some());
        }
        self.maybe_update();
        if self.lists_at.elapsed() > Duration::from_secs(2) {
            self.refresh_lists();
        }
        if self.service_on && self.host.is_none() {
            let log = service::log_path();
            let mut lines = procs::head(&log, 40);
            if lines.len() >= 40 {
                lines.extend(procs::tail(&log, 300));
            } else {
                lines = procs::tail(&log, 300);
            }
            self.host_lines = lines;
            self.status = read_status(&self.host_lines);
        }
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
                let target = v.target.clone();
                self.viewer_note =
                    Some(problem.clone().map_or_else(|| ("The viewer was closed.".to_owned(), false), |p| (p, true)));
                self.viewer = None;
                if problem.is_some_and(|p| p.contains("not paired")) {
                    self.viewer_note = None;
                    self.ask_code(&target, &target);
                }
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
            (Page::Share, "Share", (self.host.is_some() || self.service_on).then_some(("ON", GOOD))),
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
            let sharing = self.host.is_some() || self.service_on;
            let (text, fg) = match (sharing, &self.status.viewer, &self.viewer) {
                (_, _, Some(v)) => (format!("Viewing {}", v.label), GOOD),
                (true, Some(_), None) => ("Someone is connected here".to_owned(), GOOD),
                (true, None, None) => ("Sharing is on".to_owned(), ACCENT),
                (false, _, None) => ("Not connected".to_owned(), FAINT),
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
        let mut go: Option<(String, String, Option<Found>)> = None;

        if let Some(v) = self.viewer.as_mut() {
            let label = v.label.clone();
            card(ui, |ui| {
                ui.horizontal(|ui| {
                    pill(ui, "Connected", GOOD, GOOD_SOFT);
                    ui.label(RichText::new(format!("{label} is open in its own window")).color(TEXT));
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

        if self.pair.is_some() {
            self.pair_card(ui);
            ui.add_space(12.0);
        }

        if !self.settings.addons_offered && self.missing_addons() > 0 {
            self.addons_card(ui, true);
            ui.add_space(12.0);
        }

        let found = self.found.lock().map(|f| f.clone()).unwrap_or_default();
        card(ui, |ui| {
            section(
                ui,
                "Your computers",
                "Computers sharing with Anywhere, at home or through Tailscale. The first time, you type a code.",
            );
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
                let paired = h.paired();
                ui.horizontal(|ui| {
                    computer_glyph(ui, ACCENT);
                    ui.vertical(|ui| {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(&h.name).family(semibold()).color(TEXT));
                            if paired {
                                ui.label(RichText::new("Paired").small().color(GOOD));
                            }
                        });
                        let how = if h.via_tailscale() { "Through Tailscale" } else { "On this network" };
                        ui.label(RichText::new(format!("{how} · {}", h.addr.ip())).small().color(MUTED));
                    });
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let (text, kind) = if paired { ("Connect", Kind::Primary) } else { ("Pair", Kind::Secondary) };
                        if button(ui, text, kind).clicked() {
                            go = Some((h.addr.to_string(), h.name.clone(), Some(h.clone())));
                        }
                    });
                });
            }
            // Paired computers not answering right now (switched off, or
            // away without Tailscale): still offered, the viewer keeps trying.
            let away: Vec<trust::PairedHost> = self
                .paired_hosts
                .iter()
                .filter(|p| !found.iter().any(|f| f.key.is_some_and(|k| aa_core::secure::to_hex(&k) == p.key)))
                .cloned()
                .collect();
            for p in away {
                divider(ui);
                ui.horizontal(|ui| {
                    computer_glyph(ui, FAINT);
                    ui.vertical(|ui| {
                        ui.label(RichText::new(&p.name).family(semibold()).color(MUTED));
                        ui.label(RichText::new("Paired · not answering right now").small().color(FAINT));
                    });
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.link(RichText::new("Forget").small().color(MUTED)).clicked() {
                            trust::forget_host(&p.key);
                            self.paired_hosts.retain(|x| x.key != p.key);
                        }
                        if let Some(a) = p.addrs.first() {
                            if button(ui, "Try", Kind::Secondary).clicked() {
                                go = Some((a.clone(), p.name.clone(), None));
                            }
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
                    .hint_text("e.g. 192.168.1.20 or 100.101.2.3")
                    .margin(Margin::symmetric(10, 7))
                    .desired_width((ui.available_width() - 120.0).max(140.0));
                let r = ui.add(edit);
                let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if (button(ui, "Connect", Kind::Primary).clicked() || enter)
                    && !self.settings.manual_address.trim().is_empty()
                {
                    let a = self.settings.manual_address.trim().to_owned();
                    go = Some((a.clone(), a, None));
                }
            });
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

        if let Some((target, label, found)) = go {
            // A remembered, paired computer: connect directly.
            let known = found.is_none() && self.paired_hosts.iter().any(|p| p.addrs.first() == Some(&target));
            if known {
                self.connect(&target, &label);
            } else {
                self.request_connect(&target, &label, found.as_ref());
            }
        }
    }

    fn pair_card(&mut self, ui: &mut Ui) {
        let mut submit = false;
        let mut cancel = false;
        let Some(p) = self.pair.as_mut() else { return };
        egui::Frame::NONE
            .fill(CARD)
            .stroke(Stroke::new(1.5, ACCENT))
            .corner_radius(CornerRadius::same(14))
            .inner_margin(Margin::same(18))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(title(&format!("Pair with {}", p.name), 16.0));
                ui.label(
                    RichText::new(format!(
                        "On {}, open Anywhere and go to Share. Type the six-digit code shown there. You only do this once.",
                        p.name
                    ))
                    .color(MUTED),
                );
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    let r = ui.add(
                        egui::TextEdit::singleline(&mut p.code)
                            .hint_text("123 456")
                            .font(FontId::new(20.0, semibold()))
                            .margin(Margin::symmetric(12, 8))
                            .desired_width(150.0)
                            .char_limit(7),
                    );
                    if !p.busy && p.code.is_empty() {
                        r.request_focus();
                    }
                    let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    let ready = aa_core::secure::normalize_code(&p.code).len() == 6 && !p.busy;
                    if (ui.add_enabled_ui(ready, |ui| button(ui, if p.busy { "Pairing…" } else { "Pair" }, Kind::Primary)).inner.clicked()
                        || enter)
                        && ready
                    {
                        submit = true;
                    }
                    if button(ui, "Cancel", Kind::Secondary).clicked() {
                        cancel = true;
                    }
                });
                if let Some(e) = &p.error {
                    ui.add_space(6.0);
                    ui.label(RichText::new(e).color(DANGER));
                }
            });
        if submit {
            self.submit_code();
        }
        if cancel {
            self.pair = None;
        }
    }

    #[allow(clippy::too_many_lines)] // one page, top to bottom
    fn share_page(&mut self, ui: &mut Ui) {
        Self::page_header(ui, "Share this computer", "Let your other computers see and control this one.");
        let sharing = self.host.is_some() || self.service_on;
        card(ui, |ui| {
            ui.horizontal(|ui| {
                let (text, fg, bg) = match (sharing, &self.status.viewer) {
                    (false, _) => ("Off", MUTED, SIDEBAR),
                    (true, Some(_)) => ("Someone is connected", GOOD, GOOD_SOFT),
                    (true, None) => ("Waiting for a connection", ACCENT, ACCENT_SOFT),
                };
                pill(ui, text, fg, bg);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if self.service_on {
                        ui.label(RichText::new("Always on").small().family(semibold()).color(GOOD));
                    } else if sharing {
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
                (true, Some(v)) => format!("{} is connected.", v.split(':').next().unwrap_or(v)),
                (true, None) => "Open Anywhere on your other computer and pick this one.".to_owned(),
            };
            ui.label(RichText::new(line).color(TEXT));
            if self.service_on {
                ui.label(
                    RichText::new(
                        "This computer shares itself all the time, also at the sign-in screen and after a restart. \
                         Change that in Settings.",
                    )
                    .small()
                    .color(MUTED),
                );
            }

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

        ui.add_space(12.0);
        card(ui, |ui| {
            section(
                ui,
                "Pairing code",
                "The first time another computer connects, type this code there. It changes after each use.",
            );
            ui.horizontal(|ui| {
                egui::Frame::NONE
                    .fill(ACCENT_SOFT)
                    .corner_radius(CornerRadius::same(10))
                    .inner_margin(Margin::symmetric(16, 8))
                    .show(ui, |ui| {
                        ui.label(
                            RichText::new(trust::spaced(&self.pair_code)).family(semibold()).size(26.0).color(ACCENT),
                        );
                    });
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if button(ui, "New code", Kind::Secondary).clicked() {
                        self.pair_code = trust::new_pair_code();
                    }
                });
            });
            if !sharing {
                ui.add_space(4.0);
                ui.label(RichText::new("Sharing must be on for the code to work.").small().color(FAINT));
            }
            if !self.paired_viewers.is_empty() {
                ui.add_space(12.0);
                ui.label(RichText::new("Computers that can connect without a code").small().color(MUTED));
                ui.add_space(4.0);
                for v in self.paired_viewers.clone() {
                    ui.horizontal(|ui| {
                        computer_glyph(ui, ACCENT);
                        ui.label(RichText::new(if v.name.is_empty() { "A computer" } else { &v.name }).color(TEXT));
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if ui.link(RichText::new("Remove").small().color(DANGER)).clicked() {
                                trust::forget_viewer(&v.key);
                                self.paired_viewers.retain(|x| x.key != v.key);
                            }
                        });
                    });
                }
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

    #[allow(clippy::too_many_lines)] // one page, top to bottom
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
        let mut want: Option<bool> = None;
        card(ui, |ui| {
            section(ui, "After a restart or power cut", "");
            let mac = cfg!(target_os = "macos");
            let (label, help, waiting) = if mac {
                (
                    "Share this Mac at all times",
                    "Turns the Mac back on when power returns and shares it from the login screen, so you can \
                     log in from your other computer. Asks for your Mac password once.",
                    "Waiting for your Mac password…",
                )
            } else {
                (
                    "Share this PC at all times",
                    "Starts with Windows and shares the sign-in screen, so you can sign in from your other \
                     computer. Keeps the PC awake while plugged in. Windows asks to allow changes once.",
                    "Waiting for you to allow the change…",
                )
            };
            let mut on = self.service_on;
            if switch_row(ui, label, help, &mut on) && !self.service_busy {
                want = Some(on);
            }
            if self.service_busy {
                ui.label(RichText::new(waiting).small().color(MUTED));
            }
            if let Some(e) = &self.service_error {
                ui.label(RichText::new(e).small().color(DANGER));
            }
            if mac && self.filevault == Some(true) {
                ui.add_space(6.0);
                egui::Frame::NONE
                    .fill(WARN_SOFT)
                    .corner_radius(CornerRadius::same(10))
                    .inner_margin(Margin::same(12))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.label(
                            RichText::new(
                                "FileVault is on. After a restart this Mac waits for its password before macOS \
                                 starts, and no app can show that screen. Turn FileVault off in System Settings \
                                 → Privacy & Security if you need to reach the Mac after a power cut.",
                            )
                            .small()
                            .color(WARN),
                        );
                        if button(ui, "Open FileVault settings", Kind::Secondary).clicked() {
                            checks::open_privacy("FileVault");
                        }
                    });
            }
            if cfg!(target_os = "windows") {
                divider(ui);
                let mut on = self.sign_in;
                if switch_row(
                    ui,
                    "Open Anywhere when I sign in",
                    "Starts minimised, ready to connect to your other computers.",
                    &mut on,
                ) {
                    match service::set_start_at_sign_in(on) {
                        Ok(()) => self.sign_in = on,
                        Err(e) => self.service_error = Some(e.to_string()),
                    }
                }
            }
        });
        if let Some(on) = want {
            self.set_service(on);
        }
        let s = &mut self.settings;
        ui.add_space(12.0);
        let mut check_now = false;
        let status = {
            let st = self.updater.state.lock();
            let (latest, err) = st.as_ref().map_or((None, None), |st| (st.latest.clone(), st.error.clone()));
            match (&self.updating, &self.update_error, latest) {
                (Some(u), _, _) => u.clone(),
                (None, Some(e), _) => e.clone(),
                (None, None, Some(r)) if aa_platform::update::is_newer(&r.version, aa_platform::update::current()) => {
                    format!("Version {} is ready; it installs when nobody is connected.", r.version)
                }
                (None, None, Some(_)) => "You have the newest version.".to_owned(),
                (None, None, None) if err.is_some() => "Couldn't check for updates (no internet?).".to_owned(),
                (None, None, None) => "Checking for updates…".to_owned(),
            }
        };
        card(ui, |ui| {
            section(ui, "Updates", &format!("Version {}", aa_platform::update::current()));
            ui.label(RichText::new(&status).color(TEXT));
            ui.add_space(4.0);
            changed |= switch_row(
                ui,
                "Install updates automatically",
                "Only when nobody is connected. Anywhere reopens by itself afterwards.",
                &mut s.auto_update,
            );
            if button(ui, "Check now", Kind::Secondary).clicked() {
                check_now = true;
            }
        });
        ui.add_space(12.0);
        card(ui, |ui| {
            section(ui, "Help", "");
            ui.horizontal_wrapped(|ui| {
                if button(ui, "Open log folder", Kind::Secondary).clicked() {
                    checks::open_url(&procs::data_dir().display().to_string());
                }
                if button(ui, "Releases page", Kind::Secondary).clicked() {
                    checks::open_url("https://github.com/minute-creative/anywhere-alternative/releases/latest");
                }
                if button(ui, "Reset settings", Kind::Secondary).clicked() {
                    *s = Settings::default();
                    changed = true;
                }
            });
        });
        if check_now {
            self.update_skip = None;
            self.update_error = None;
            self.updater.check_now.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        if changed {
            s.save();
        }
    }

    fn extras_page(&mut self, ui: &mut Ui) {
        Self::page_header(
            ui,
            "Extras",
            "Free add-ons for controllers, the microphone and connecting from anywhere. Everything else works without them.",
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
        if self.missing_addons() > 0 || self.addons_busy {
            self.addons_card(ui, false);
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

/// "host:port", or a bare IP (default port).
fn parse_target(t: &str) -> Option<std::net::SocketAddr> {
    let t = t.trim();
    t.parse().ok().or_else(|| t.parse::<std::net::IpAddr>().ok().map(|ip| std::net::SocketAddr::new(ip, DEFAULT_PORT)))
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
                let mut area = egui::ScrollArea::vertical().auto_shrink([false, false]);
                // AA_SCROLL=<px>: start scrolled down (screenshots in testing).
                if let Some(y) = std::env::var("AA_SCROLL").ok().and_then(|v| v.parse::<f32>().ok()) {
                    area = area.vertical_scroll_offset(y);
                }
                area.show(ui, |ui| {
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
