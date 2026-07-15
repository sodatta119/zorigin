// zap desktop — control panel that hosts the zap web server, mirroring the
// Android app. Remote devices connect via the URL/QR shown here.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use std::net::{IpAddr, Ipv4Addr};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use eframe::egui;
use egui::{Color32, FontId, Margin, RichText, Rounding, Stroke, TextStyle};
use qrcode::QrCode;
use zap_core::web::{self, Credentials, ServeConfig, ServerHandle, ServerInfo};

const ACCENT: Color32 = Color32::from_rgb(0xD9, 0x93, 0x2A); // softer amber for small accents
const ACCENT_BTN: Color32 = Color32::from_rgb(0xBE, 0x7C, 0x1E); // deeper amber for large fills
const BG: Color32 = Color32::from_rgb(0x0F, 0x0F, 0x12);
const CARD: Color32 = Color32::from_rgb(0x18, 0x18, 0x1D);
const CARD_BORDER: Color32 = Color32::from_rgb(0x2A, 0x2A, 0x30);
const TEXT: Color32 = Color32::from_rgb(0xEC, 0xEC, 0xEE);
const MUTED: Color32 = Color32::from_rgb(0x8A, 0x8A, 0x92);

fn main() -> eframe::Result<()> {
    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([440.0, 640.0])
        .with_min_inner_size([400.0, 560.0])
        .with_title("zap")
        // Dark, unified title bar on macOS: content draws under a transparent
        // title bar (traffic-light buttons still float on top).
        .with_fullsize_content_view(true)
        .with_titlebar_shown(true)
        .with_title_shown(false);
    if let Ok(icon) = eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon_256.png")) {
        viewport = viewport.with_icon(std::sync::Arc::new(icon));
    }
    let options = eframe::NativeOptions { viewport, ..Default::default() };
    eframe::run_native(
        "zap",
        options,
        Box::new(|cc| {
            setup_style(&cc.egui_ctx);
            Ok(Box::<ZapApp>::default())
        }),
    )
}

fn setup_style(ctx: &egui::Context) {
    let mut v = egui::Visuals::dark();
    v.panel_fill = BG;
    v.window_fill = BG;
    v.extreme_bg_color = Color32::from_rgb(0x0B, 0x0B, 0x0E);
    v.faint_bg_color = CARD;
    v.override_text_color = Some(TEXT);
    v.hyperlink_color = ACCENT;
    v.selection.bg_fill = Color32::from_rgb(0x4A, 0x3A, 0x12);
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
    v.widgets.inactive.bg_fill = Color32::from_rgb(0x24, 0x24, 0x2A);
    v.widgets.inactive.weak_bg_fill = Color32::from_rgb(0x24, 0x24, 0x2A);
    v.widgets.hovered.bg_fill = Color32::from_rgb(0x2E, 0x2E, 0x36);
    v.widgets.hovered.weak_bg_fill = Color32::from_rgb(0x2E, 0x2E, 0x36);
    v.widgets.active.bg_fill = Color32::from_rgb(0x36, 0x36, 0x40);

    let mut style = egui::Style { visuals: v, ..Default::default() };
    style.spacing.item_spacing = egui::vec2(10.0, 12.0);
    style.spacing.button_padding = egui::vec2(14.0, 8.0);
    style.spacing.interact_size.y = 32.0;
    style.text_styles = [
        (TextStyle::Heading, FontId::proportional(26.0)),
        (TextStyle::Body, FontId::proportional(15.0)),
        (TextStyle::Button, FontId::proportional(15.0)),
        (TextStyle::Monospace, FontId::monospace(14.0)),
        (TextStyle::Small, FontId::proportional(12.0)),
    ]
    .into();
    ctx.set_style(style);
}

struct Running {
    info: ServerInfo,
    handle: ServerHandle,
    qr: Option<egui::TextureHandle>,
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
    shot_frames: u32,
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
            shot_frames: 0,
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
                let qr = qr_texture(ctx, &info.url());
                self.running = Some(Running { info, handle, qr });
                self.speed = 0.0;
                self.sample = None;
            }
            Err(e) => self.error = Some(format!("Could not start: {e:#}")),
        }
    }

    fn stop(&mut self) {
        self.running = None; // dropping ServerHandle stops the server
        self.speed = 0.0;
        self.sample = None;
    }

    /// Poll the byte counter and update a smoothed bytes/sec figure.
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
}

impl eframe::App for ZapApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // eframe may reset visuals to follow the system (light) theme after our
        // creator runs — re-apply our dark theme whenever that happens.
        if ctx.style().visuals.override_text_color != Some(TEXT) {
            setup_style(ctx);
        }

        // Debug: with ZAP_SHOT=<path> set, capture our own window to a PNG and exit.
        if let Ok(path) = std::env::var("ZAP_SHOT") {
            self.shot_frames += 1;
            ctx.request_repaint();
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
        let running = self.running.is_some();

        // Extra top margin clears the floating macOS traffic-light buttons.
        let panel = egui::Frame::default()
            .fill(BG)
            .inner_margin(Margin { left: 20.0, right: 20.0, top: 34.0, bottom: 18.0 });
        egui::CentralPanel::default().frame(panel).show(ctx, |ui| {
            // Header
            ui.horizontal(|ui| {
                ui.label(RichText::new("⚡").size(28.0).color(ACCENT));
                ui.add_space(4.0);
                ui.vertical(|ui| {
                    ui.label(RichText::new("zap").size(24.0).strong());
                    ui.label(RichText::new("Lightning-fast file transfer over Wi-Fi").size(12.5).color(MUTED));
                });
            });
            ui.add_space(18.0);

            // Status card
            card(ui, |ui| {
                if let Some(r) = &self.running {
                    ui.label(RichText::new("● Server running").size(15.0).strong().color(ACCENT));
                    ui.add_space(8.0);
                    if r.info.lan_ip.is_none() {
                        ui.label(
                            RichText::new("⚠ No Wi-Fi network detected — connect to Wi-Fi, then Stop and Start again.")
                                .size(12.5)
                                .color(Color32::from_rgb(0xE0, 0x55, 0x4B)),
                        );
                        ui.add_space(6.0);
                    }
                    let url = r.info.url();
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(&url).monospace().size(15.0));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.small_button("Copy").clicked() {
                                ui.output_mut(|o| o.copied_text = url.clone());
                            }
                        });
                    });
                    ui.add_space(6.0);
                    // Live throughput + animated bolt
                    ui.horizontal(|ui| {
                        let active = self.speed > 1.0;
                        let t = ui.input(|i| i.time);
                        let a = if active { 0.5 + 0.5 * (((t * 7.0).sin() * 0.5 + 0.5) as f32) } else { 0.35 };
                        ui.label(RichText::new("⚡").size(16.0).color(ACCENT.gamma_multiply(a)));
                        ui.label(
                            RichText::new(fmt_speed(self.speed))
                                .monospace()
                                .size(13.0)
                                .color(if active { TEXT } else { MUTED }),
                        );
                    });
                    if let Some(qr) = &r.qr {
                        ui.add_space(12.0);
                        ui.vertical_centered(|ui| {
                            ui.image((qr.id(), egui::vec2(190.0, 190.0)));
                            ui.label(RichText::new("Scan on the other device").size(11.0).color(MUTED));
                        });
                    }
                    ui.add_space(10.0);
                    ui.label(
                        RichText::new("Both devices must be on the same Wi-Fi. If it won't connect, turn off your router's \u{201C}AP / client isolation\u{201D}.")
                            .size(11.0)
                            .color(MUTED),
                    );
                } else {
                    ui.label(RichText::new("Stopped").size(15.0).strong());
                    ui.add_space(4.0);
                    ui.label(RichText::new("Start to share over Wi-Fi").color(MUTED));
                }
            });

            ui.add_space(12.0);

            // Shared folder card
            card(ui, |ui| {
                ui.label(RichText::new("SHARED FOLDER").size(11.0).color(MUTED));
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    let text = truncate_path(&self.dir.display().to_string(), 34);
                    ui.label(RichText::new(text).size(14.0));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.add_enabled(!running, egui::Button::new("Change")).clicked() {
                            if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                                self.dir = dir;
                            }
                        }
                    });
                });
            });

            ui.add_space(12.0);

            // Settings card (security + port)
            card(ui, |ui| {
                ui.add_enabled(!running, egui::Checkbox::new(&mut self.secure, "Require a password"));
                if self.secure {
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("User").color(MUTED));
                        ui.add_enabled(!running, egui::TextEdit::singleline(&mut self.user).desired_width(140.0));
                    });
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("Pass").color(MUTED));
                        ui.add_enabled(
                            !running,
                            egui::TextEdit::singleline(&mut self.pass).password(true).desired_width(140.0),
                        );
                    });
                }
                ui.add_space(10.0);
                ui.separator();
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Port").color(MUTED));
                    ui.add_enabled(!running, egui::DragValue::new(&mut self.port).range(1024..=65535));
                });
            });

            ui.add_space(18.0);

            // Start / Stop
            let (label, fill, fg) = if running {
                ("Stop server", Color32::from_rgb(0x2A, 0x2A, 0x30), TEXT)
            } else {
                ("Start server", ACCENT_BTN, Color32::from_rgb(0x14, 0x10, 0x08))
            };
            let btn = egui::Button::new(RichText::new(label).size(16.0).strong().color(fg))
                .fill(fill)
                .rounding(12.0);
            if ui.add_sized([ui.available_width(), 46.0], btn).clicked() {
                if running {
                    self.stop();
                } else {
                    self.start(ctx);
                }
            }

            if let Some(err) = &self.error {
                ui.add_space(10.0);
                ui.colored_label(Color32::from_rgb(0xE0, 0x55, 0x4B), err);
            }
        });
    }
}

fn card<R>(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui) -> R) {
    egui::Frame::none()
        .fill(CARD)
        .stroke(Stroke::new(1.0, CARD_BORDER))
        .rounding(14.0)
        .inner_margin(Margin::same(16.0))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui);
        });
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
    if i == 0 {
        format!("{v:.0} {}", units[i])
    } else {
        format!("{v:.1} {}", units[i])
    }
}

fn truncate_path(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let tail: String = s.chars().rev().take(max - 1).collect::<Vec<_>>().into_iter().rev().collect();
    format!("…{tail}")
}

// ---- Debug PNG saver (stored-zlib, no extra deps) ----
fn save_png(path: &str, img: &egui::ColorImage) -> std::io::Result<()> {
    let (w, h) = (img.size[0], img.size[1]);
    let mut raw = Vec::with_capacity(h * (w * 4 + 1));
    for y in 0..h {
        raw.push(0); // filter byte
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

/// Render `url` as a QR code into an egui texture.
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
