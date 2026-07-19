// Zulu desktop - clipboard / link / snippet sync over the LAN. One device
// hosts the znet-core server (reused from Zap); every paired device runs this
// same app and its clipboard stays in sync. Copy on one, it's on the others.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod imageclip;
mod sync;
mod tlsclient;

use std::net::{IpAddr, Ipv4Addr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use eframe::egui;
use egui::{Color32, FontId, Margin, RichText, Rounding, TextStyle};
use qrcode::QrCode;
use znet_core::web::{self, ServeConfig, ServerHandle, ServerInfo, TlsMaterial};

use sync::{SyncHandle, SyncState};

/// The browser receiver served at `/` to devices without the native app (any
/// phone or laptop browser): live clip list + tap-to-copy + a paste-and-send box.
const ZULU_HTML: &str = include_str!("zulu.html");

const ACCENT: Color32 = Color32::from_rgb(0x7f, 0xa0, 0xd4); // Zulu blue - reads on light & dark
const ACCENT_BTN: Color32 = Color32::from_rgb(0x6f, 0x8f, 0xc4); // primary button fill
const WARN: Color32 = Color32::from_rgb(0xC7, 0x3B, 0x2E);
const OK: Color32 = Color32::from_rgb(0x2E, 0x9E, 0x57);
const BG_LIGHT: Color32 = Color32::from_rgb(0xF6, 0xF7, 0xFA); // cool near-white
const BG_DARK: Color32 = Color32::from_rgb(0x0D, 0x0D, 0x0F); // family dark base

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Host,
    Join,
}

fn theme_bg(dark: bool) -> Color32 {
    if dark {
        BG_DARK
    } else {
        BG_LIGHT
    }
}

fn main() -> eframe::Result<()> {
    let viewport = egui::ViewportBuilder::default()
        .with_inner_size([440.0, 620.0])
        .with_min_inner_size([400.0, 520.0])
        .with_title("Zulu");
    let options = eframe::NativeOptions { viewport, ..Default::default() };
    eframe::run_native("Zulu", options, Box::new(|_cc| Ok(Box::<ZuluApp>::default())))
}

/// Deterministic light/dark base + Zulu accent, re-applied every frame (mirrors
/// zap-desktop's `tune_theme` so the family looks consistent).
fn tune_theme(ctx: &egui::Context, dark: bool) {
    let mut style = (*ctx.style()).clone();
    let mut v = if dark {
        let mut v = egui::Visuals::dark();
        v.panel_fill = BG_DARK;
        v.faint_bg_color = Color32::from_rgb(0x17, 0x17, 0x1A);
        v.extreme_bg_color = Color32::from_rgb(0x1E, 0x1E, 0x24);
        v.override_text_color = Some(Color32::from_rgb(0xF2, 0xF2, 0xF2));
        v.widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0, Color32::from_rgb(0x2A, 0x2A, 0x30));
        v
    } else {
        let mut v = egui::Visuals::light();
        v.panel_fill = BG_LIGHT;
        v.faint_bg_color = Color32::from_rgb(0xFF, 0xFF, 0xFF);
        v.extreme_bg_color = Color32::from_rgb(0xFF, 0xFF, 0xFF);
        v.override_text_color = Some(Color32::from_rgb(0x1D, 0x20, 0x26));
        v.widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0, Color32::from_rgb(0xDD, 0xE1, 0xE8));
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
    style.spacing.item_spacing = egui::vec2(10.0, 10.0);
    style.spacing.button_padding = egui::vec2(14.0, 8.0);
    style.spacing.interact_size.y = 30.0;
    style.text_styles = [
        (TextStyle::Heading, FontId::proportional(24.0)),
        (TextStyle::Body, FontId::proportional(15.0)),
        (TextStyle::Button, FontId::proportional(15.0)),
        (TextStyle::Monospace, FontId::monospace(13.5)),
        (TextStyle::Small, FontId::proportional(12.0)),
    ]
    .into();
    ctx.set_style(style);
}

/// Small pill toggle (light <-> dark).
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

struct Hosting {
    info: ServerInfo,
    _handle: ServerHandle,
    qr: Option<egui::TextureHandle>,
}

struct ZuluApp {
    mode: Mode,
    port: u16,
    peer_url: String,
    /// Host mode: encrypt the LAN hop with a self-signed cert (native peers pin
    /// its fingerprint; the browser receiver only works plaintext).
    secure: bool,
    /// The generated cert while hosting securely (None = plain HTTP).
    tls_material: Option<TlsMaterial>,
    hosting: Option<Hosting>,
    sync: Option<SyncHandle>,
    state: Arc<Mutex<SyncState>>,
    error: Option<String>,
    dark: bool,
    shot_frames: u32,
    /// Pinned snippets - frequently-pasted text, persisted across runs. Clicking
    /// one puts it back on the clipboard (and syncs it when connected).
    pins: Vec<String>,
    /// One-shot guard so `ZULU_AUTOHOST` (a test hook) starts hosting once.
    tried_autohost: bool,
}

impl Default for ZuluApp {
    fn default() -> Self {
        Self {
            mode: Mode::Host,
            port: 8080,
            peer_url: String::new(),
            secure: false,
            tls_material: None,
            hosting: None,
            sync: None,
            state: Arc::new(Mutex::new(SyncState::default())),
            error: None,
            dark: std::env::var("ZULU_DARK").is_ok(),
            shot_frames: 0,
            pins: load_pins(),
            tried_autohost: false,
        }
    }
}

impl ZuluApp {
    fn running(&self) -> bool {
        self.sync.is_some()
    }

    fn start(&mut self, ctx: &egui::Context) {
        self.error = None;
        self.state = Arc::new(Mutex::new(SyncState::default()));

        let base = match self.mode {
            Mode::Host => {
                // Generate a fresh self-signed cert when hosting securely.
                self.tls_material = if self.secure {
                    match web::tls::self_signed(host_sans()) {
                        Ok(m) => Some(m),
                        Err(e) => {
                            self.error = Some(format!("Encryption setup failed: {e:#}"));
                            return;
                        }
                    }
                } else {
                    None
                };
                let config = ServeConfig {
                    dir: clip_dir(),
                    port: self.port,
                    bind: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                    auth: None, // open on the LAN; encryption is the `secure` toggle
                    history: None,
                    index_html: Some(ZULU_HTML.to_string()),
                    tls: self.tls_material.clone(),
                };
                match web::spawn(config) {
                    Ok((info, handle)) => {
                        // The QR/URL carries the scheme + cert fingerprint so a
                        // native peer pins it automatically.
                        let qr = qr_texture(ctx, &info.url_with_key());
                        // The host's own client talks to itself over loopback,
                        // matching the server's scheme (and pinning its cert).
                        let base = match &self.tls_material {
                            Some(m) => format!("https://127.0.0.1:{}?fp={}", info.port, m.fingerprint),
                            None => format!("127.0.0.1:{}", info.port),
                        };
                        self.hosting = Some(Hosting { info, _handle: handle, qr });
                        base
                    }
                    Err(e) => {
                        self.error = Some(format!("Could not host: {e:#}"));
                        return;
                    }
                }
            }
            Mode::Join => {
                let url = self.peer_url.trim().to_string();
                if url.is_empty() {
                    self.error = Some("Enter the host's URL (from its Zulu window).".into());
                    return;
                }
                url
            }
        };

        match SyncHandle::start(&base, Arc::clone(&self.state)) {
            Some(h) => self.sync = Some(h),
            None => {
                self.error = Some(format!("Couldn't parse address: {base}"));
                self.hosting = None;
            }
        }
    }

    fn stop(&mut self) {
        if let Some(h) = self.sync.take() {
            h.stop();
        }
        self.hosting = None; // dropping the ServerHandle stops the server
        self.tls_material = None;
    }
}

impl eframe::App for ZuluApp {
    fn clear_color(&self, _v: &egui::Visuals) -> [f32; 4] {
        theme_bg(self.dark).to_normalized_gamma_f32()
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        tune_theme(ctx, self.dark);
        ctx.request_repaint_after(Duration::from_millis(300)); // keep the live view fresh

        // Test hook: auto-start hosting once, so an end-to-end sync run can be
        // driven headlessly without clicking the button.
        if !self.tried_autohost && std::env::var("ZULU_AUTOHOST").is_ok() {
            self.tried_autohost = true;
            self.secure = std::env::var("ZULU_SECURE").is_ok();
            self.start(ctx);
        }

        // Debug: ZULU_SHOT=<path> captures our own window to a PNG and exits.
        if let Ok(path) = std::env::var("ZULU_SHOT") {
            self.shot_frames += 1;
            ctx.request_repaint();
            if self.shot_frames == 1 && std::env::var("ZULU_SHOT_RUNNING").is_ok() && !self.running() {
                self.start(ctx);
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

        let snap = self.snapshot();
        let running = self.running();

        egui::TopBottomPanel::bottom("actions")
            .frame(egui::Frame::default().fill(theme_bg(self.dark)).inner_margin(Margin::symmetric(20.0, 14.0)))
            .show(ctx, |ui| {
                let (label, fill, fg) = if running {
                    ("Stop sync", ui.visuals().widgets.inactive.bg_fill, ui.visuals().text_color())
                } else {
                    (start_label(self.mode), ACCENT_BTN, Color32::from_rgb(0x0A, 0x0F, 0x1A))
                };
                let btn = egui::Button::new(RichText::new(label).size(16.0).strong().color(fg))
                    .fill(fill)
                    .rounding(12.0);
                if ui.add_sized([ui.available_width(), 46.0], btn).clicked() {
                    if running {
                        self.stop()
                    } else {
                        self.start(ctx)
                    }
                }
                if let Some(err) = self.error.clone().or_else(|| snap.error.clone()) {
                    ui.add_space(8.0);
                    ui.colored_label(WARN, err);
                }
            });

        let panel = egui::Frame::default()
            .fill(theme_bg(self.dark))
            .inner_margin(Margin { left: 20.0, right: 20.0, top: 14.0, bottom: 6.0 });
        egui::CentralPanel::default().frame(panel).show(ctx, |ui| {
            egui::ScrollArea::vertical().auto_shrink([false; 2]).show(ui, |ui| {
                self.header(ui);
                ui.add_space(12.0);
                if running {
                    self.running_view(ui, &snap);
                } else {
                    self.setup_view(ui);
                }
            });
        });
    }
}

impl ZuluApp {
    fn snapshot(&self) -> SyncSnapshot {
        let s = self.state.lock().unwrap_or_else(|p| p.into_inner());
        SyncSnapshot {
            connected: s.connected,
            presence: s.presence,
            sent: s.sent,
            received: s.received,
            error: s.error.clone(),
            recent: s.recent.iter().rev().take(12).map(|c| (c.text.clone(), c.incoming)).collect(),
        }
    }

    fn header(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            // Painted badge (egui's default font lacks most symbol glyphs, so we
            // draw the family mark instead of relying on a Unicode icon).
            let (rect, _) = ui.allocate_exact_size(egui::vec2(30.0, 30.0), egui::Sense::hover());
            ui.painter().rect_filled(rect, Rounding::same(8.0), ACCENT);
            ui.painter().text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                "z",
                FontId::proportional(19.0),
                Color32::from_rgb(0x0A, 0x0F, 0x1A),
            );
            ui.add_space(8.0);
            ui.vertical(|ui| {
                ui.label(RichText::new("Zulu").size(23.0).strong());
                ui.label(RichText::new("Your clipboard, on every device").size(12.5).weak());
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::TOP), |ui| {
                theme_toggle(ui, &mut self.dark);
            });
        });
    }

    fn setup_view(&mut self, ui: &mut egui::Ui) {
        // Mode picker.
        ui.horizontal(|ui| {
            if ui.selectable_label(self.mode == Mode::Host, RichText::new("Host").size(14.0)).clicked() {
                self.mode = Mode::Host;
            }
            if ui.selectable_label(self.mode == Mode::Join, RichText::new("Join").size(14.0)).clicked() {
                self.mode = Mode::Join;
            }
        });
        ui.add_space(10.0);

        match self.mode {
            Mode::Host => {
                card(ui, self.dark, |ui| {
                    ui.label(RichText::new("Host this session").strong());
                    ui.label(
                        RichText::new("This device runs the sync server. Other devices open Zulu, pick Join, and paste the URL shown here once you start.")
                            .size(12.5)
                            .weak(),
                    );
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        ui.label("Port");
                        let mut port = self.port.to_string();
                        if ui.add(egui::TextEdit::singleline(&mut port).desired_width(70.0)).changed() {
                            if let Ok(p) = port.parse() {
                                self.port = p;
                            }
                        }
                    });
                    ui.add_space(6.0);
                    ui.checkbox(&mut self.secure, "Encrypt the connection (TLS)");
                    if self.secure {
                        ui.label(
                            RichText::new("Native devices only: each pins the host's certificate from the QR/URL. The browser receiver can't open an encrypted host without a warning.")
                                .size(11.5)
                                .weak(),
                        );
                    }
                });
            }
            Mode::Join => {
                card(ui, self.dark, |ui| {
                    ui.label(RichText::new("Join a session").strong());
                    ui.label(
                        RichText::new("Paste the URL from the hosting device's Zulu window (e.g. http://192.168.1.9:8080).")
                            .size(12.5)
                            .weak(),
                    );
                    ui.add_space(6.0);
                    ui.add(
                        egui::TextEdit::singleline(&mut self.peer_url)
                            .hint_text("http://192.168.1.9:8080")
                            .desired_width(f32::INFINITY),
                    );
                });
            }
        }

        ui.add_space(12.0);
        note(
            ui,
            "Desktop <-> desktop syncs automatically. On phones the OS blocks background clipboard reads, so sending is a share-sheet tap (assisted) - by design, not a bug.",
        );
    }

    fn running_view(&mut self, ui: &mut egui::Ui, snap: &SyncSnapshot) {
        // Status row.
        ui.horizontal(|ui| {
            let (dot, txt) = if snap.connected {
                (OK, "Connected")
            } else {
                (WARN, "Connecting...")
            };
            paint_dot(ui, dot, 5.0);
            ui.label(RichText::new(txt).strong());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(RichText::new(format!("{} device(s) paired", snap.presence)).size(12.5).weak());
            });
        });
        ui.add_space(8.0);

        // Host: show how others join.
        if let Some(h) = &self.hosting {
            let encrypted = self.tls_material.is_some();
            card(ui, self.dark, |ui| {
                ui.label(RichText::new("Others join at").size(12.5).weak());
                if encrypted {
                    // The fingerprint must travel too, so show the full keyed URL
                    // (also in the QR) and flag that it's encrypted.
                    ui.label(RichText::new(h.info.url()).monospace().color(ACCENT).size(15.0));
                    ui.label(RichText::new("Encrypted - scan the QR (or paste the full link) so the device pins this host's certificate:").size(11.0).color(OK));
                    ui.label(RichText::new(h.info.url_with_key()).monospace().size(10.5).weak());
                } else {
                    ui.label(RichText::new(h.info.url()).monospace().color(ACCENT).size(15.0));
                }
                if let Some(qr) = &h.qr {
                    ui.add_space(8.0);
                    ui.vertical_centered(|ui| {
                        ui.image((qr.id(), egui::vec2(150.0, 150.0)));
                    });
                }
            });
            ui.add_space(8.0);
        }

        // Counters.
        ui.horizontal(|ui| {
            stat(ui, "Sent", snap.sent);
            stat(ui, "Received", snap.received);
        });
        ui.add_space(8.0);

        // Pinned snippets.
        if !self.pins.is_empty() {
            ui.label(RichText::new("Pinned").size(12.5).weak());
            ui.add_space(4.0);
            let mut remove: Option<usize> = None;
            for (i, text) in self.pins.clone().iter().enumerate() {
                match pin_row(ui, self.dark, text) {
                    PinAction::Use => set_clipboard(text),
                    PinAction::Remove => remove = Some(i),
                    PinAction::None => {}
                }
            }
            if let Some(i) = remove {
                self.pins.remove(i);
                save_pins(&self.pins);
            }
            ui.add_space(10.0);
        }

        // Activity list.
        ui.label(RichText::new("Recent clips").size(12.5).weak());
        ui.add_space(4.0);
        if snap.recent.is_empty() {
            note(ui, "Copy something on any paired device - it shows up here.");
        } else {
            for (text, incoming) in &snap.recent {
                if clip_row(ui, self.dark, text, *incoming) && !self.pins.iter().any(|p| p == text) {
                    self.pins.push(text.clone());
                    save_pins(&self.pins);
                }
            }
        }
    }
}

// ---- small UI pieces ----

fn start_label(mode: Mode) -> &'static str {
    match mode {
        Mode::Host => "Start hosting",
        Mode::Join => "Connect",
    }
}

fn card(ui: &mut egui::Ui, dark: bool, add: impl FnOnce(&mut egui::Ui)) {
    let fill = if dark {
        Color32::from_rgb(0x17, 0x17, 0x1A)
    } else {
        Color32::WHITE
    };
    egui::Frame::default()
        .fill(fill)
        .rounding(12.0)
        .inner_margin(Margin::same(14.0))
        .stroke(ui.visuals().widgets.noninteractive.bg_stroke)
        .show(ui, |ui| {
            ui.vertical(|ui| add(ui));
        });
}

fn note(ui: &mut egui::Ui, text: &str) {
    egui::Frame::default()
        .fill(ACCENT.gamma_multiply(0.10))
        .rounding(10.0)
        .inner_margin(Margin::same(12.0))
        .show(ui, |ui| {
            ui.label(RichText::new(text).size(12.5));
        });
}

fn stat(ui: &mut egui::Ui, label: &str, value: u64) {
    egui::Frame::default()
        .fill(ui.visuals().faint_bg_color)
        .rounding(10.0)
        .inner_margin(Margin::symmetric(14.0, 10.0))
        .show(ui, |ui| {
            ui.vertical(|ui| {
                ui.label(RichText::new(value.to_string()).size(20.0).strong().color(ACCENT));
                ui.label(RichText::new(label).size(11.5).weak());
            });
        });
}

/// A recent-clip row. Returns true if its "Pin" button was clicked.
fn clip_row(ui: &mut egui::Ui, dark: bool, text: &str, incoming: bool) -> bool {
    let fill = card_fill(dark);
    let mut pin = false;
    egui::Frame::default()
        .fill(fill)
        .rounding(10.0)
        .inner_margin(Margin::symmetric(12.0, 8.0))
        .stroke(ui.visuals().widgets.noninteractive.bg_stroke)
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                // Colour encodes direction: green in, blue out.
                let color = if incoming { OK } else { ACCENT };
                paint_dot(ui, color, 4.0);
                ui.add_space(2.0);
                let one_line = text.replace('\n', " ");
                ui.label(RichText::new(truncate(&one_line, 46)).size(13.0));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    pin = ui
                        .add(egui::Button::new(RichText::new("Pin").size(12.0)).frame(false))
                        .on_hover_text("Keep this snippet")
                        .clicked();
                });
            });
        });
    pin
}

enum PinAction {
    Use,
    Remove,
    None,
}

/// A pinned-snippet row: click the text to reuse it, "×" to remove it.
fn pin_row(ui: &mut egui::Ui, dark: bool, text: &str) -> PinAction {
    let mut action = PinAction::None;
    egui::Frame::default()
        .fill(card_fill(dark))
        .rounding(10.0)
        .inner_margin(Margin::symmetric(12.0, 8.0))
        .stroke(egui::Stroke::new(1.0, ACCENT.gamma_multiply(0.5)))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                paint_dot(ui, ACCENT, 4.0);
                ui.add_space(2.0);
                let one_line = text.replace('\n', " ");
                if ui
                    .add(egui::Button::new(RichText::new(truncate(&one_line, 44)).size(13.0)).frame(false))
                    .on_hover_text("Copy to clipboard")
                    .clicked()
                {
                    action = PinAction::Use;
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .add(egui::Button::new(RichText::new("×").size(16.0)).frame(false))
                        .on_hover_text("Unpin")
                        .clicked()
                    {
                        action = PinAction::Remove;
                    }
                });
            });
        });
    action
}

fn card_fill(dark: bool) -> Color32 {
    if dark {
        Color32::from_rgb(0x17, 0x17, 0x1A)
    } else {
        Color32::WHITE
    }
}

// ---- pinned snippets: persistence + clipboard ----

/// Put `text` on the OS clipboard. When a sync session is running, the sender
/// thread notices and broadcasts it, so reusing a pin also shares it.
fn set_clipboard(text: &str) {
    if let Ok(mut c) = arboard::Clipboard::new() {
        let _ = c.set_text(text.to_string());
    }
}

fn pins_path() -> Option<std::path::PathBuf> {
    let dir = dirs::data_local_dir()?.join("zulu");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir.join("pins.txt"))
}

/// Load pinned snippets (one per line, newlines escaped).
fn load_pins() -> Vec<String> {
    let Some(path) = pins_path() else { return Vec::new() };
    let Ok(data) = std::fs::read_to_string(path) else { return Vec::new() };
    data.lines().filter(|l| !l.is_empty()).map(unescape_line).collect()
}

/// Persist pinned snippets atomically (temp file + rename).
fn save_pins(pins: &[String]) {
    let Some(path) = pins_path() else { return };
    let body: String = pins.iter().map(|p| format!("{}\n", escape_line(p))).collect();
    let tmp = path.with_extension("tmp");
    if std::fs::write(&tmp, body).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

/// Escape a snippet to a single line: backslash then newline/carriage-return.
fn escape_line(s: &str) -> String {
    s.replace('\\', "\\\\").replace('\n', "\\n").replace('\r', "\\r")
}

/// Inverse of [`escape_line`].
fn unescape_line(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('r') => out.push('\r'),
                Some('\\') => out.push('\\'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Draw a small filled status dot inline (default egui fonts lack a reliable
/// bullet glyph, so we paint one).
fn paint_dot(ui: &mut egui::Ui, color: Color32, radius: f32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(radius * 2.0 + 4.0, radius * 2.0 + 4.0), egui::Sense::hover());
    ui.painter().circle_filled(rect.center(), radius, color);
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max).collect();
        out.push('…');
        out
    }
}

/// Subject-alternative names for the self-signed host cert: the LAN IP plus the
/// loopback names the host's own client uses.
fn host_sans() -> Vec<String> {
    let mut sans = vec!["localhost".to_string(), "127.0.0.1".to_string()];
    if let Some(ip) = web::lan_ip() {
        sans.insert(0, ip.to_string());
    }
    sans
}

/// Where the host's znet-core server keeps its (unused, for Zulu) share dir.
/// Zulu doesn't transfer files, but the server still wants a directory to bind.
fn clip_dir() -> std::path::PathBuf {
    let dir = dirs::data_local_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("zulu");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// A snapshot of the shared sync state for one frame (avoids holding the lock
/// while drawing).
struct SyncSnapshot {
    connected: bool,
    presence: usize,
    sent: u64,
    received: u64,
    error: Option<String>,
    recent: Vec<(String, bool)>,
}

// ---- QR + screenshot (copied from zap-desktop; keep the family harness) ----

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

#[cfg(test)]
mod tests {
    use super::{escape_line, unescape_line};

    #[test]
    fn pin_escaping_round_trips() {
        for s in ["plain", "two\nlines", "back\\slash", "crlf\r\nhere", "mix\\n\nreal", ""] {
            assert_eq!(unescape_line(&escape_line(s)), s, "round trip {s:?}");
        }
    }

    #[test]
    fn escaped_pin_is_single_line() {
        assert!(!super::escape_line("a\nb\nc").contains('\n'), "no raw newlines in the on-disk form");
    }
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
    out.extend_from_slice(&((b << 16) | a).to_be_bytes());
    out
}
