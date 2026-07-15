// zap desktop — control panel that hosts the zap web server, mirroring the
// Android app. Remote devices connect via the URL/QR shown here.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use eframe::egui;
use egui::{Color32, FontId, Margin, RichText, Rounding, TextStyle};
use qrcode::QrCode;
use zap_core::web::{self, Credentials, Direction, ServeConfig, ServerHandle, ServerInfo, TransferInfo};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Share,
    Transfers,
}

const ACCENT: Color32 = Color32::from_rgb(0xD9, 0x8A, 0x1E); // amber — reads on light & dark
const ACCENT_BTN: Color32 = Color32::from_rgb(0xE0, 0x93, 0x22); // primary button fill
const WARN: Color32 = Color32::from_rgb(0xC7, 0x3B, 0x2E);
const OK: Color32 = Color32::from_rgb(0x2E, 0x9E, 0x57); // green — "reachable" confirmation
const BG_LIGHT: Color32 = Color32::from_rgb(0xF7, 0xF6, 0xF3); // light window / panel background
const BG_DARK: Color32 = Color32::from_rgb(0x0D, 0x0D, 0x0F); // dark bg (matches the web/Android UI)

/// Window/panel background for the active theme.
fn theme_bg(dark: bool) -> Color32 {
    if dark { BG_DARK } else { BG_LIGHT }
}

fn main() -> eframe::Result<()> {
    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([440.0, 600.0])
        .with_min_inner_size([400.0, 500.0])
        .with_title("Zap");
    if let Ok(icon) = eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon_256.png")) {
        viewport = viewport.with_icon(std::sync::Arc::new(icon));
    }
    let options = eframe::NativeOptions { viewport, ..Default::default() };
    eframe::run_native("Zap", options, Box::new(|_cc| Ok(Box::<ZapApp>::default())))
}

/// Layer our accent + spacing + rounding on top of a deterministic light or dark
/// base (not the washed-out system default), re-applied every frame.
fn tune_theme(ctx: &egui::Context, dark: bool) {
    let mut style = (*ctx.style()).clone();
    {
        let mut v = if dark {
            let mut v = egui::Visuals::dark();
            v.panel_fill = BG_DARK;
            v.faint_bg_color = Color32::from_rgb(0x17, 0x17, 0x1A); // cards
            v.extreme_bg_color = Color32::from_rgb(0x1E, 0x1E, 0x24); // text fields
            v.override_text_color = Some(Color32::from_rgb(0xF2, 0xF2, 0xF2));
            v.widgets.noninteractive.bg_stroke =
                egui::Stroke::new(1.0, Color32::from_rgb(0x2A, 0x2A, 0x30));
            v
        } else {
            let mut v = egui::Visuals::light();
            v.panel_fill = BG_LIGHT;
            v.faint_bg_color = Color32::from_rgb(0xFF, 0xFF, 0xFF);
            v.extreme_bg_color = Color32::from_rgb(0xFF, 0xFF, 0xFF);
            v.override_text_color = Some(Color32::from_rgb(0x22, 0x20, 0x1D));
            v.widgets.noninteractive.bg_stroke =
                egui::Stroke::new(1.0, Color32::from_rgb(0xE2, 0xDF, 0xD8));
            v
        };
        v.hyperlink_color = ACCENT;
        v.selection.bg_fill = ACCENT.gamma_multiply(0.35);
        let r = Rounding::same(10.0);
        for w in [
            &mut v.widgets.noninteractive,
            &mut v.widgets.inactive,
            &mut v.widgets.hovered,
            &mut v.widgets.active,
            &mut v.widgets.open,
        ] {
            w.rounding = r;
        }
        style.visuals = v;
    }
    style.spacing.item_spacing = egui::vec2(10.0, 10.0);
    style.spacing.button_padding = egui::vec2(14.0, 8.0);
    style.spacing.interact_size.y = 30.0;
    style.text_styles = [
        (TextStyle::Heading, FontId::proportional(24.0)),
        (TextStyle::Body, FontId::proportional(15.0)),
        (TextStyle::Button, FontId::proportional(15.0)),
        (TextStyle::Monospace, FontId::monospace(14.0)),
        (TextStyle::Small, FontId::proportional(12.0)),
    ]
    .into();
    ctx.set_style(style);
}

/// A small pill toggle switch (light ↔ dark). Flips `dark` on click.
fn theme_toggle(ui: &mut egui::Ui, dark: &mut bool) {
    let size = egui::vec2(40.0, 22.0);
    let (rect, mut resp) = ui.allocate_exact_size(size, egui::Sense::click());
    if resp.clicked() {
        *dark = !*dark;
        resp.mark_changed();
    }
    let how = ui.ctx().animate_bool(resp.id, *dark);
    let radius = rect.height() / 2.0;
    let track = if *dark { ACCENT } else { Color32::from_gray(0xC4) };
    ui.painter().rect(rect, Rounding::same(radius), track, egui::Stroke::NONE);
    let knob_x = egui::lerp((rect.left() + radius)..=(rect.right() - radius), how);
    ui.painter()
        .circle_filled(egui::pos2(knob_x, rect.center().y), radius - 3.0, Color32::WHITE);
    resp.on_hover_text(if *dark { "Switch to light theme" } else { "Switch to dark theme" });
}

struct Running {
    info: ServerInfo,
    handle: ServerHandle,
    qr: Option<egui::TextureHandle>,
    started: Instant,
}

/// How long to wait for a first client before warning that nothing can connect.
const NO_CLIENT_WARN_SECS: u64 = 20;

/// The grace period, with a gated env override (`ZAP_NO_CLIENT_SECS`) so the
/// "no device connected" warning can be screenshotted without a 20s wait.
fn no_client_warn_secs() -> u64 {
    std::env::var("ZAP_NO_CLIENT_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(NO_CLIENT_WARN_SECS)
}

struct ZapApp {
    dir: PathBuf,
    port: u16,
    secure: bool,
    user: String,
    pass: String,
    running: Option<Running>,
    error: Option<String>,
    speed: f64,
    sample: Option<(Instant, u64)>,
    tab: Tab,
    tspeed: HashMap<u64, (Instant, u64, f64)>, // per-transfer: last sample time, bytes, smoothed speed
    shot_frames: u32,
    dark: bool,
}

impl Default for ZapApp {
    fn default() -> Self {
        let dir = dirs::download_dir()
            .or_else(dirs::home_dir)
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."));
        Self {
            dir,
            port: 8080,
            secure: false,
            user: "zap".to_string(),
            pass: "zap".to_string(),
            running: None,
            error: None,
            speed: 0.0,
            sample: None,
            tab: Tab::Share,
            tspeed: HashMap::new(),
            shot_frames: 0,
            dark: std::env::var("ZAP_DARK").is_ok(), // default light; env forces dark for screenshots
        }
    }
}

impl ZapApp {
    fn start(&mut self, ctx: &egui::Context) {
        self.error = None;
        let auth = self.secure.then(|| Credentials {
            user: if self.user.is_empty() { "zap".into() } else { self.user.clone() },
            pass: if self.pass.is_empty() { "zap".into() } else { self.pass.clone() },
        });
        let config = ServeConfig {
            dir: self.dir.clone(),
            port: self.port,
            bind: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            auth,
        };
        match web::spawn(config) {
            Ok((info, handle)) => {
                // The QR carries the pairing key when secured, so scanning it
                // signs in without typing the password.
                let qr = qr_texture(ctx, &info.url_with_key());
                self.running = Some(Running { info, handle, qr, started: Instant::now() });
                self.speed = 0.0;
                self.sample = None;
            }
            Err(e) => self.error = Some(format!("Could not start: {e:#}")),
        }
    }

    fn stop(&mut self) {
        self.running = None;
        self.speed = 0.0;
        self.sample = None;
    }

    fn update_speed(&mut self, ctx: &egui::Context) {
        let Some(bytes) = self.running.as_ref().map(|r| r.handle.bytes_transferred()) else {
            return;
        };
        ctx.request_repaint_after(Duration::from_millis(120));
        let now = Instant::now();
        match self.sample {
            Some((t0, b0)) => {
                let dt = now.duration_since(t0).as_secs_f64();
                if dt >= 0.4 {
                    let inst = bytes.saturating_sub(b0) as f64 / dt;
                    self.speed = self.speed * 0.4 + inst * 0.6;
                    if self.speed < 1.0 {
                        self.speed = 0.0;
                    }
                    self.sample = Some((now, bytes));
                }
            }
            None => self.sample = Some((now, bytes)),
        }
    }

    /// Poll per-transfer activity and compute a smoothed speed for each.
    /// Returns newest-first.
    fn poll_transfers(&mut self) -> Vec<(TransferInfo, f64)> {
        let list = self.running.as_ref().map(|r| r.handle.transfers()).unwrap_or_default();
        let now = Instant::now();
        let mut rows = Vec::with_capacity(list.len());
        for t in &list {
            let e = self.tspeed.entry(t.id).or_insert((now, t.done, 0.0));
            let dt = now.duration_since(e.0).as_secs_f64();
            if dt >= 0.35 {
                let inst = t.done.saturating_sub(e.1) as f64 / dt;
                e.2 = e.2 * 0.4 + inst * 0.6;
                e.0 = now;
                e.1 = t.done;
            }
            let speed = if t.finished { 0.0 } else { e.2 };
            rows.push((t.clone(), speed));
        }
        // Drop tracking for transfers no longer in the list.
        self.tspeed.retain(|id, _| list.iter().any(|t| t.id == *id));
        rows.reverse(); // newest first
        if rows.is_empty() && std::env::var("ZAP_SHOT_DEMO").is_ok() {
            return vec![
                (
                    TransferInfo { id: 2, name: "IMG_2231.mov".into(), direction: Direction::Upload, done: 734_003_200, total: Some(1_610_612_736), finished: false, ok: false, verified: false, elapsed_secs: 12.0 },
                    28_311_552.0,
                ),
                (
                    TransferInfo { id: 1, name: "Q3-report.pdf".into(), direction: Direction::Download, done: 2_411_724, total: Some(2_411_724), finished: true, ok: true, verified: true, elapsed_secs: 1.0 },
                    0.0,
                ),
            ];
        }
        rows
    }
}

impl eframe::App for ZapApp {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        theme_bg(self.dark).to_normalized_gamma_f32()
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        tune_theme(ctx, self.dark);

        // Debug: ZAP_SHOT=<path> captures our own window to a PNG and exits.
        if let Ok(path) = std::env::var("ZAP_SHOT") {
            self.shot_frames += 1;
            ctx.request_repaint();
            if self.shot_frames == 1 && std::env::var("ZAP_SHOT_RUNNING").is_ok() && self.running.is_none() {
                self.start(ctx);
            }
            if self.shot_frames == 1 && std::env::var("ZAP_SHOT_DEMO").is_ok() {
                self.tab = Tab::Transfers;
            }
            if self.shot_frames == 4 {
                ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot);
            }
            if let Some(img) = ctx.input(|i| {
                i.events.iter().find_map(|e| match e {
                    egui::Event::Screenshot { image, .. } => Some(image.clone()),
                    _ => None,
                })
            }) {
                let _ = save_png(&path, &img);
                std::process::exit(0);
            }
        }

        self.update_speed(ctx);
        let transfer_rows = self.poll_transfers();
        let running = self.running.is_some();

        // Primary action pinned to the bottom (always reachable).
        egui::TopBottomPanel::bottom("actions")
            .frame(egui::Frame::default().fill(theme_bg(self.dark)).inner_margin(Margin::symmetric(20.0, 14.0)))
            .show(ctx, |ui| {
                let (label, fill, fg) = if running {
                    ("Stop server", ui.visuals().widgets.inactive.bg_fill, ui.visuals().text_color())
                } else {
                    ("Start server", ACCENT_BTN, Color32::from_rgb(0x1A, 0x12, 0x04))
                };
                let btn = egui::Button::new(RichText::new(label).size(16.0).strong().color(fg))
                    .fill(fill)
                    .rounding(12.0);
                if ui.add_sized([ui.available_width(), 46.0], btn).clicked() {
                    if running { self.stop() } else { self.start(ctx) }
                }
                if let Some(err) = &self.error {
                    ui.add_space(8.0);
                    ui.colored_label(WARN, err);
                }
            });

        let panel = egui::Frame::default().fill(theme_bg(self.dark)).inner_margin(Margin {
            left: 20.0,
            right: 20.0,
            top: 14.0,
            bottom: 6.0,
        });
        egui::CentralPanel::default().frame(panel).show(ctx, |ui| {
            egui::ScrollArea::vertical().auto_shrink([false; 2]).show(ui, |ui| {
                // Header
                ui.horizontal(|ui| {
                    ui.label(RichText::new("⚡").size(26.0).color(ACCENT));
                    ui.add_space(4.0);
                    ui.vertical(|ui| {
                        ui.label(RichText::new("Zap").size(23.0).strong());
                        ui.label(RichText::new("Lightning-fast file transfer over Wi-Fi").size(12.5).weak());
                    });
                    // Light/dark toggle, pinned top-right.
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::TOP), |ui| {
                        theme_toggle(ui, &mut self.dark);
                    });
                });
                ui.add_space(14.0);

                // Tabs
                ui.horizontal(|ui| {
                    if ui.selectable_label(self.tab == Tab::Share, RichText::new("Share").size(14.0)).clicked() {
                        self.tab = Tab::Share;
                    }
                    ui.add_space(4.0);
                    let tlabel = if transfer_rows.is_empty() {
                        "Transfers".to_string()
                    } else {
                        format!("Transfers ({})", transfer_rows.len())
                    };
                    if ui.selectable_label(self.tab == Tab::Transfers, RichText::new(tlabel).size(14.0)).clicked() {
                        self.tab = Tab::Transfers;
                    }
                });
                ui.add_space(12.0);

                match self.tab {
                    Tab::Share => {
                        if let Some(r) = &self.running {
                            self.status_running(ui, r);
                        } else {
                            card(ui, |ui| {
                                ui.label(RichText::new("Stopped").size(15.0).strong());
                                ui.add_space(3.0);
                                ui.label(RichText::new("Start to share over Wi-Fi").weak());
                            });
                            ui.add_space(12.0);
                            self.folder_card(ui);
                            ui.add_space(12.0);
                            self.settings_card(ui);
                        }
                    }
                    Tab::Transfers => transfers_view(ui, running, &transfer_rows),
                }
                ui.add_space(6.0);
            });
        });
    }
}

impl ZapApp {
    fn status_running(&self, ui: &mut egui::Ui, r: &Running) {
        card(ui, |ui| {
            let reachable = r.info.lan_ip.is_some();
            if reachable {
                ui.label(RichText::new("REACHABLE AT").size(12.0).strong().color(OK));
            } else {
                ui.label(RichText::new("● Server running").size(15.0).strong().color(ACCENT));
            }
            ui.add_space(8.0);
            if !reachable {
                ui.label(
                    RichText::new("⚠ No Wi-Fi detected — connect to Wi-Fi, then Stop and Start again.")
                        .size(12.5)
                        .color(WARN),
                );
                ui.add_space(6.0);
            }
            let url = r.info.url();
            ui.horizontal(|ui| {
                ui.label(RichText::new(&url).monospace().size(17.0).strong());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.small_button("Copy").clicked() {
                        ui.output_mut(|o| o.copied_text = url.clone());
                    }
                });
            });
            if reachable {
                ui.add_space(3.0);
                ui.label(RichText::new("Open this on the other device (same Wi-Fi).").size(11.5).weak());
            }

            // Watchdog: reachable, but no device has connected after a grace
            // period → the likely cause is AP/client isolation or a wrong network.
            if reachable
                && r.handle.requests_seen() == 0
                && r.started.elapsed().as_secs() >= no_client_warn_secs()
            {
                ui.add_space(10.0);
                egui::Frame::none()
                    .fill(WARN.gamma_multiply(0.12))
                    .rounding(10.0)
                    .inner_margin(Margin::same(12.0))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.label(RichText::new("⚠ No device has connected yet").size(13.0).strong().color(WARN));
                        ui.add_space(4.0);
                        ui.label(
                            RichText::new(
                                "If the other device can't open the link:\n\
                                 • make sure both are on the same Wi-Fi\n\
                                 • turn off your router's \u{201C}AP / client isolation\u{201D}\n\
                                 • a guest network often blocks this — use the main one",
                            )
                            .size(12.0),
                        );
                    });
            }
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                let active = self.speed > 1.0;
                let t = ui.input(|i| i.time);
                let a = if active { 0.5 + 0.5 * (((t * 7.0).sin() * 0.5 + 0.5) as f32) } else { 0.35 };
                ui.label(RichText::new("⚡").size(15.0).color(ACCENT.gamma_multiply(a)));
                let col = if active { ui.visuals().text_color() } else { ui.visuals().weak_text_color() };
                ui.label(RichText::new(fmt_speed(self.speed)).monospace().size(13.0).color(col));
            });
            if let Some(qr) = &r.qr {
                ui.add_space(10.0);
                ui.vertical_centered(|ui| {
                    ui.image((qr.id(), egui::vec2(150.0, 150.0)));
                    ui.label(RichText::new("Scan on the other device").size(11.0).weak());
                });
            }
            ui.add_space(8.0);
            ui.label(
                RichText::new("Both devices must be on the same Wi-Fi. If it won't connect, turn off your router's \u{201C}AP / client isolation\u{201D}.")
                    .size(11.0)
                    .weak(),
            );
        });
    }

    fn folder_card(&mut self, ui: &mut egui::Ui) {
        card(ui, |ui| {
            ui.label(RichText::new("SHARED FOLDER").size(11.0).weak());
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new(truncate_path(&self.dir.display().to_string(), 32)).size(14.0));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Change").clicked() {
                        if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                            self.dir = dir;
                        }
                    }
                });
            });
        });
    }

    fn settings_card(&mut self, ui: &mut egui::Ui) {
        card(ui, |ui| {
            ui.checkbox(&mut self.secure, "Require a password");
            if self.secure {
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    ui.label(RichText::new("User").weak());
                    ui.add(egui::TextEdit::singleline(&mut self.user).desired_width(150.0));
                });
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Pass").weak());
                    ui.add(egui::TextEdit::singleline(&mut self.pass).password(true).desired_width(150.0));
                });
            }
            ui.add_space(10.0);
            ui.separator();
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new("Port").weak());
                ui.add(egui::DragValue::new(&mut self.port).range(1024..=65535));
            });
        });
    }
}

fn card<R>(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui) -> R) {
    let v = ui.visuals();
    let fill = v.faint_bg_color;
    let stroke = v.widgets.noninteractive.bg_stroke;
    egui::Frame::none()
        .fill(fill)
        .stroke(stroke)
        .rounding(14.0)
        .inner_margin(Margin::same(16.0))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui);
        });
}

/// Activity list: one card per transfer with live per-file speed.
fn transfers_view(ui: &mut egui::Ui, running: bool, rows: &[(TransferInfo, f64)]) {
    if rows.is_empty() {
        card(ui, |ui| {
            let msg = if running {
                "No transfers yet — send or grab a file from another device."
            } else {
                "Start the server to see transfers here."
            };
            ui.label(RichText::new(msg).weak());
        });
        return;
    }
    for (t, speed) in rows {
        card(ui, |ui| {
            ui.horizontal(|ui| {
                let (arrow, dir_txt) = match t.direction {
                    Direction::Upload => ("⬇", "Incoming"),
                    Direction::Download => ("⬆", "Outgoing"),
                };
                ui.label(RichText::new(arrow).size(17.0).color(ACCENT));
                ui.add_space(4.0);
                ui.vertical(|ui| {
                    ui.label(RichText::new(&t.name).size(14.0).strong());
                    let line = match t.total {
                        Some(tot) if tot > 0 => {
                            let pct = (t.done as f64 / tot as f64 * 100.0).min(100.0);
                            format!("{dir_txt} · {} / {} ({pct:.0}%)", human_size(t.done), human_size(tot))
                        }
                        _ => format!("{dir_txt} · {}", human_size(t.done)),
                    };
                    ui.label(RichText::new(line).size(11.5).weak());
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if t.finished {
                        let (txt, col) = if t.ok {
                            let label = if t.verified { "verified" } else { "done" };
                            (label, Color32::from_rgb(0x2E, 0x9E, 0x57))
                        } else {
                            ("failed", WARN)
                        };
                        ui.label(RichText::new(txt).size(12.0).color(col));
                    } else {
                        ui.label(RichText::new(fmt_speed(*speed)).monospace().size(12.5).color(ACCENT));
                    }
                });
            });
            if let (Some(tot), false) = (t.total, t.finished) {
                if tot > 0 {
                    let frac = (t.done as f32 / tot as f32).clamp(0.0, 1.0);
                    ui.add_space(6.0);
                    ui.add(egui::ProgressBar::new(frac).fill(ACCENT));
                }
            }
        });
        ui.add_space(8.0);
    }
}

fn human_size(n: u64) -> String {
    let units = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < units.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 { format!("{n} B") } else { format!("{v:.1} {}", units[i]) }
}

fn fmt_speed(bps: f64) -> String {
    if bps < 1.0 {
        return "0 B/s".to_string();
    }
    let units = ["B/s", "KB/s", "MB/s", "GB/s"];
    let mut v = bps;
    let mut i = 0;
    while v >= 1024.0 && i < units.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 { format!("{v:.0} {}", units[i]) } else { format!("{v:.1} {}", units[i]) }
}

fn truncate_path(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let tail: String = s.chars().rev().take(max - 1).collect::<Vec<_>>().into_iter().rev().collect();
    format!("…{tail}")
}

fn qr_texture(ctx: &egui::Context, url: &str) -> Option<egui::TextureHandle> {
    let code = QrCode::new(url.as_bytes()).ok()?;
    let colors = code.to_colors();
    let w = (colors.len() as f64).sqrt() as usize;
    let scale = 6;
    let quiet = 4;
    let dim = (w + quiet * 2) * scale;
    let mut pixels = vec![Color32::WHITE; dim * dim];
    for y in 0..w {
        for x in 0..w {
            if colors[y * w + x] == qrcode::Color::Dark {
                for dy in 0..scale {
                    for dx in 0..scale {
                        let px = (x + quiet) * scale + dx;
                        let py = (y + quiet) * scale + dy;
                        pixels[py * dim + px] = Color32::BLACK;
                    }
                }
            }
        }
    }
    let image = egui::ColorImage { size: [dim, dim], pixels };
    Some(ctx.load_texture("qr", image, egui::TextureOptions::NEAREST))
}

// ---- Debug PNG saver (stored-zlib, no extra deps) ----
fn save_png(path: &str, img: &egui::ColorImage) -> std::io::Result<()> {
    let (w, h) = (img.size[0], img.size[1]);
    let mut raw = Vec::with_capacity(h * (w * 4 + 1));
    for y in 0..h {
        raw.push(0);
        for x in 0..w {
            raw.extend_from_slice(&img.pixels[y * w + x].to_array());
        }
    }
    let mut png = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&(w as u32).to_be_bytes());
    ihdr.extend_from_slice(&(h as u32).to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
    write_chunk(&mut png, b"IHDR", &ihdr);
    write_chunk(&mut png, b"IDAT", &zlib_stored(&raw));
    write_chunk(&mut png, b"IEND", &[]);
    std::fs::write(path, png)
}

fn write_chunk(out: &mut Vec<u8>, typ: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let mut cd = typ.to_vec();
    cd.extend_from_slice(data);
    out.extend_from_slice(&cd);
    out.extend_from_slice(&crc32(&cd).to_be_bytes());
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

fn zlib_stored(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0x78, 0x01];
    let mut i = 0;
    while i < data.len() {
        let end = (i + 65535).min(data.len());
        let chunk = &data[i..end];
        out.push(if end >= data.len() { 1 } else { 0 });
        let len = chunk.len() as u16;
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&(!len).to_le_bytes());
        out.extend_from_slice(chunk);
        i = end;
    }
    let (mut a, mut b) = (1u32, 0u32);
    for &x in data {
        a = (a + x as u32) % 65521;
        b = (b + a) % 65521;
    }
    out.extend_from_slice(&(((b << 16) | a) as u32).to_be_bytes());
    out
}
