use crate::audio::{MAX_BANDS, SharedState, NUM_BARS};
use crate::filter::{Biquad, FilterKind};
use cpal::traits::{DeviceTrait, HostTrait};
use eframe::egui;
use egui::ecolor::Hsva;
use egui::{Align2, Color32, FontId, Pos2, Rect, RichText, Sense, Slider, Stroke, Vec2};
use std::sync::Arc;
use std::sync::atomic::Ordering;

const MIN_FREQ: f32 = 20.0;
const MAX_FREQ: f32 = 20_000.0;
const DEFAULT_DB_MIN: f32 = -60.0;
const DEFAULT_DB_MAX: f32 = 60.0;
const DEFAULT_VOL_MIN: f32 = 1.0 / 3.0; // lower bound of the master volume fader
const DEFAULT_VOL_MAX: f32 = 3.0;       // upper bound of the master volume fader

// Console (training) mode constraints, matching a typical channel EQ.
const CONSOLE_DB_MIN: f32 = -15.0;
const CONSOLE_DB_MAX: f32 = 15.0;
const CONSOLE_Q_MIN: f32 = 0.5;
const CONSOLE_Q_MAX: f32 = 5.0;
const CONSOLE_GAIN_STEP: f32 = 0.05; // dB per unit of scroll delta

const BAND_COLORS: [Color32; 6] = [
    Color32::from_rgb(255, 107, 107),
    Color32::from_rgb(255, 190, 90),
    Color32::from_rgb(120, 220, 120),
    Color32::from_rgb(90, 200, 220),
    Color32::from_rgb(130, 150, 255),
    Color32::from_rgb(220, 130, 240),
];

pub struct EqApp {
    state: Arc<SharedState>,
    _engine: Option<crate::audio::AudioEngine>,
    audio_status: String,
    selected: Option<usize>,
    paused: bool,
    show_settings: bool,
    theme_dark: bool,
    idle_anim: bool,   // animate the bars on stop
    peak_hold: bool,   // draw peak-hold caps
    db_min: f32,
    db_max: f32,
    volume_gain: f32,   // current master volume x
    vol_min: f32,       // fader lower bound (from settings)
    vol_max: f32,       // fader upper bound (from settings)
    seek_drag: bool,    // user is dragging the seek scrubber
    seek_pos: f32,      // scrubber value while dragging
    q_hint: Option<(usize, String, f32)>, // (band, text, time) of last scroll-to-knob adjustment
    console_mode: bool, // training mode: fixed 5-band strip, knob-style controls
    knob_ui: bool,     // tile style: analog knobs instead of sliders
    saved_db_min: f32,  // plot range restored when console mode is switched off
    saved_db_max: f32,
    selected_device: Option<(Option<crate::audio::AudioEngine>, String)>,
    selected_output: Option<String>,
    selected_input: Option<String>,
    show_device_selection: bool,
    available_outputs: Vec<String>,
    available_inputs: Vec<String>,
    vis_levels: Vec<f32>,
    vis_peaks: Vec<f32>,
}

fn config_path() -> std::path::PathBuf {
    let base = per_os_config_dir();
    base.join("eqvis").join("config.txt")
}

/// Platform-correct user config directory:
/// - Windows: `%APPDATA%`
/// - macOS:   `~/Library/Application Support`
/// - Linux/BSD/other Unix: `$XDG_CONFIG_HOME` or `~/.config`
/// Falls back to the system temp dir if the env vars are missing.
fn per_os_config_dir() -> std::path::PathBuf {
    #[cfg(target_os = "windows")]
    return std::env::var_os("APPDATA")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);

    #[cfg(target_os = "macos")]
    return std::env::var_os("HOME")
        .map(|h| std::path::PathBuf::from(h).join("Library").join("Application Support"))
        .unwrap_or_else(std::env::temp_dir);

    #[cfg(all(unix, not(target_os = "macos")))]
    return std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".config")))
        .unwrap_or_else(std::env::temp_dir);

    #[cfg(not(any(target_os = "windows", target_os = "macos", unix)))]
    return std::env::temp_dir();
}

/// Load persisted settings, falling back to (and immediately saving) the
/// defaults when no config file exists yet.
fn load_settings(app: &mut EqApp) {
    let path = config_path();
    let Ok(text) = std::fs::read_to_string(&path) else {
        save_settings(app);
        return;
    };
    let get = |key: &str| -> Option<String> {
        text.lines().find_map(|l| {
            let (k, v) = l.split_once('=')?;
            (k.trim() == key).then(|| v.trim().to_string())
        })
    };
    let parse_bool = |s: Option<String>| s.and_then(|v| v.parse::<u8>().ok()).map(|b| b != 0);
    if let Some(b) = parse_bool(get("theme_dark")) {
        app.theme_dark = b;
    }
    if let Some(b) = parse_bool(get("idle_anim")) {
        app.idle_anim = b;
    }
    if let Some(b) = parse_bool(get("peak_hold")) {
        app.peak_hold = b;
    }
    if let Some(b) = parse_bool(get("console_mode")) {
        app.console_mode = b;
    }
    if let Some(b) = parse_bool(get("knob_ui")) {
        app.knob_ui = b;
    }
    if let Some(v) = get("db_min").and_then(|v| v.parse::<f32>().ok()) {
        app.db_min = v.clamp(-60.0, 0.0);
    }
    if let Some(v) = get("db_max").and_then(|v| v.parse::<f32>().ok()) {
        app.db_max = v.clamp(0.0, 60.0);
    }
    if let Some(v) = get("vol_min").and_then(|v| v.parse::<f32>().ok()) {
        app.vol_min = v.clamp(0.05, 1.0);
    }
    if let Some(v) = get("vol_max").and_then(|v| v.parse::<f32>().ok()) {
        app.vol_max = v.clamp(1.0, 5.0);
    }
    if let Some(v) = get("num_bands").and_then(|v| v.parse::<u32>().ok()) {
        app.state.set_band_count(v as usize);
    }
    if app.vol_max < app.vol_min + 0.05 {
        app.vol_max = (app.vol_min + 0.05).min(5.0);
    }
    if app.db_max < app.db_min {
        app.db_min = app.db_max;
    }
    // Console mode locks the range to ±15 dB and the strip to 5 fixed bands.
    if app.console_mode {
        app.db_min = CONSOLE_DB_MIN;
        app.db_max = CONSOLE_DB_MAX;
        app.state.apply_console_mode(true);
    }
    app.clamp_gains();
}

fn save_settings(app: &EqApp) {
    let path = config_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let text = format!(
        "theme_dark={}\nidle_anim={}\npeak_hold={}\nconsole_mode={}\nknob_ui={}\ndb_min={}\ndb_max={}\nvol_min={}\nvol_max={}\nnum_bands={}\n",
        app.theme_dark as u8,
        app.idle_anim as u8,
        app.peak_hold as u8,
        app.console_mode as u8,
        app.knob_ui as u8,
        app.db_min,
        app.db_max,
        app.vol_min,
        app.vol_max,
        app.state.num_bands()
    );
    let _ = std::fs::write(path, text);
}

impl EqApp {
    pub fn new(state: Arc<SharedState>, engine: Option<crate::audio::AudioEngine>, status: String) -> Self {
        let mut app = Self {
            state,
            _engine: engine,
            audio_status: status,
            selected: None,
            paused: false,
            show_settings: false,
            theme_dark: true,
            idle_anim: true,
            peak_hold: true,
            db_min: DEFAULT_DB_MIN,
            db_max: DEFAULT_DB_MAX,
            volume_gain: 1.0,
            vol_min: DEFAULT_VOL_MIN,
            vol_max: DEFAULT_VOL_MAX,
            seek_drag: false,
            seek_pos: 0.0,
            q_hint: None,
            console_mode: false,
            knob_ui: false,
            saved_db_min: DEFAULT_DB_MIN,
            saved_db_max: DEFAULT_DB_MAX,
            selected_device: None,
            selected_output: None,
            selected_input: None,
            show_device_selection: false,
            available_outputs: Vec::new(),
            available_inputs: Vec::new(),
            vis_levels: vec![0.0; NUM_BARS],
            vis_peaks: vec![0.0; NUM_BARS],
        };
        load_settings(&mut app);
        app
    }

    fn band_color(i: usize) -> Color32 {
        BAND_COLORS[i % BAND_COLORS.len()]
    }

    // Theme-aware palette: the graph, band tiles and analyzer stay console-dark
    // in dark mode and become light in light mode.

    fn graph_bg(&self) -> Color32 {
        if self.theme_dark {
            Color32::from_rgb(24, 24, 28)
        } else {
            Color32::from_rgb(226, 228, 236)
        }
    }

    fn grid_line(&self) -> Color32 {
        if self.theme_dark {
            Color32::from_gray(50)
        } else {
            Color32::from_gray(180)
        }
    }

    fn grid_label(&self) -> Color32 {
        if self.theme_dark {
            Color32::from_gray(120)
        } else {
            Color32::from_gray(95)
        }
    }

    fn zero_line(&self) -> Color32 {
        if self.theme_dark {
            Color32::from_gray(90)
        } else {
            Color32::from_gray(150)
        }
    }

    fn tile_bg(&self) -> Color32 {
        if self.theme_dark {
            Color32::from_rgb(28, 28, 34)
        } else {
            Color32::from_rgb(244, 245, 250)
        }
    }

    fn tile_stroke(&self) -> Color32 {
        if self.theme_dark {
            Color32::from_gray(48)
        } else {
            Color32::from_gray(195)
        }
    }

    fn plot_overlay(&self) -> Color32 {
        if self.theme_dark {
            Color32::from_rgba_unmultiplied(0, 0, 0, 90)
        } else {
            Color32::from_rgba_unmultiplied(0, 0, 0, 22)
        }
    }

    fn zero_band(&self) -> Color32 {
        if self.theme_dark {
            Color32::from_rgba_unmultiplied(255, 255, 255, 26)
        } else {
            Color32::from_rgba_unmultiplied(0, 0, 0, 40)
        }
    }

    fn slot_band(&self) -> Color32 {
        if self.theme_dark {
            Color32::from_rgba_unmultiplied(255, 255, 255, 7)
        } else {
            Color32::from_rgba_unmultiplied(0, 0, 0, 10)
        }
    }

    fn peak_cap(&self) -> Color32 {
        if self.theme_dark {
            Color32::from_rgb(255, 255, 235)
        } else {
            Color32::from_rgb(55, 58, 68)
        }
    }

    fn pin_ring(&self) -> Color32 {
        if self.theme_dark {
            Color32::WHITE
        } else {
            Color32::from_rgb(45, 48, 58)
        }
    }

    fn freq_to_x(freq: f32, rect: Rect) -> f32 {
        let t = (freq.max(MIN_FREQ).ln() - MIN_FREQ.ln()) / (MAX_FREQ.ln() - MIN_FREQ.ln());
        rect.left() + t * rect.width()
    }

    fn x_to_freq(x: f32, rect: Rect) -> f32 {
        let t = ((x - rect.left()) / rect.width()).clamp(0.0, 1.0);
        (MIN_FREQ.ln() + t * (MAX_FREQ.ln() - MIN_FREQ.ln())).exp()
    }

    fn db_to_y(&self, db: f32, rect: Rect) -> f32 {
        let span = (self.db_max - self.db_min).abs().max(0.001);
        let t = (db - self.db_min) / span;
        rect.bottom() - t * rect.height()
    }

    fn y_to_db(&self, y: f32, rect: Rect) -> f32 {
        let span = (self.db_max - self.db_min).abs().max(0.001);
        let t = ((rect.bottom() - y) / rect.height()).clamp(0.0, 1.0);
        self.db_min + t * span
    }

    /// Horizontal dB grid lines that adapt to the current floor/roof range.
    fn gridlines(&self) -> Vec<f32> {
        let range = (self.db_max - self.db_min).abs();
        let step = if range >= 72.0 {
            12.0
        } else if range >= 36.0 {
            6.0
        } else {
            3.0
        };
        let mut out = Vec::new();
        let mut db = (self.db_min / step).ceil() * step;
        while db <= self.db_max + 0.001 {
            out.push(db);
            db += step;
        }
        if out.is_empty() {
            out.push(self.db_min);
            out.push(self.db_max);
        }
        out
    }

    /// Clamp every band's gain to the current floor/roof so dots can't sit
    /// outside the graph after a settings change.
    fn clamp_gains(&self) {
        let lo = self.db_min.min(self.db_max);
        let hi = self.db_max.max(self.db_min);
        for band in self.state.bands.iter().take(self.state.num_bands()) {
            let g = band.gain().clamp(lo, hi);
            band.set_gain(g);
        }
    }

    /// Pick the band whose graph node is nearest to `pos`, within `max_dist`.
    fn nearest_node(&self, pos: Pos2, rect: Rect, max_dist: f32) -> Option<usize> {
        let mut best: Option<(usize, f32)> = None;
        for (i, band) in self.state.bands.iter().take(self.state.num_bands()).enumerate() {
            let node_pos = Pos2::new(
                Self::freq_to_x(band.freq(), rect),
                self.db_to_y(band.gain(), rect),
            );
            let d = node_pos.distance(pos);
            if d < max_dist && best.map_or(true, |(_, bd)| d < bd) {
                best = Some((i, d));
            }
        }
        best.map(|(i, _)| i)
    }

    fn refresh_device_lists(&mut self) {
        // DEBUG: print every device to the terminal once, so the available
        // names are easy to cross-reference with what the OS actually exposes.
        let host = cpal::default_host();
        println!("--- Available Output Devices ---");
        if let Ok(devs) = host.output_devices() {
            for d in devs {
                println!("{:?}", d.name());
            }
        }
        println!("--- Available Input Devices ---");
        if let Ok(devs) = host.input_devices() {
            for d in devs {
                println!("{:?}", d.name());
            }
        }
        println!("-------------------------------");

        // cpal only lists WASAPI endpoints Windows marks "active". Merge in a
        // raw enumeration of every endpoint (disabled/unplugged/... included)
        // so e.g. a headset mic that Windows hasn't fully configured still shows.
        let mut output_names = host
            .output_devices()
            .map(|devices| devices.filter_map(|d| d.name().ok()).collect::<Vec<_>>())
            .unwrap_or_default();
        let mut input_names = host
            .input_devices()
            .map(|devices| devices.filter_map(|d| d.name().ok()).collect::<Vec<_>>())
            .unwrap_or_default();
        #[cfg(target_os = "windows")]
        {
            output_names.extend(crate::win_devices::enumerate_all_audio_endpoints(false));
            input_names.extend(crate::win_devices::enumerate_all_audio_endpoints(true));
        }
        output_names.sort();
        output_names.dedup();
        input_names.sort();
        input_names.dedup();
        self.available_outputs = output_names;
        self.available_inputs = input_names;
    }

    fn load_file(&mut self) {
        if let Some(path) = rfd::FileDialog::new()
            .add_filter("audio", &["mp3", "wav", "flac", "ogg", "m4a"])
            .pick_file()
        {
            let path_str = path.to_string_lossy().into_owned();
            match crate::audio::AudioEngine::try_new_file(self.state.clone(), &path_str) {
                Ok(engine) => {
                    self._engine = Some(engine);
                    self.audio_status = format!("playing: {}", path_str);
                    self.show_device_selection = false;
                }
                Err(e) => self.audio_status = format!("file error: {}", e),
            }
        }
    }

    /// One band's control strip, laid out like a channel fader row.
    fn band_strip(&mut self, ui: &mut egui::Ui, i: usize) {
        let color = Self::band_color(i);
        let selected = self.selected == Some(i);
        let dim = self.grid_label();
        let band = &self.state.bands[i];

        let frame = egui::Frame::default()
            .fill(self.tile_bg())
            .stroke(if selected {
                Stroke::new(1.5_f32, color)
            } else {
                Stroke::new(1.0_f32, self.tile_stroke())
            })
            .rounding(4.0)
            .inner_margin(egui::Margin::symmetric(6.0, 3.0));

        frame.show(ui, |ui| {
            ui.spacing_mut().item_spacing = Vec2::new(3.0, 2.0);
            ui.spacing_mut().slider_width = 96.0;
            ui.set_width(118.0);
            ui.vertical(|ui| {
                // Header line: clickable band label + per-band on/off.
                let label_color = if band.enabled() {
                    color
                } else {
                    color.linear_multiply(0.4)
                };
                ui.horizontal(|ui| {
                    if ui
                        .add(egui::SelectableLabel::new(
                            selected,
                            RichText::new(format!("BAND {}", i + 1)).color(label_color).strong(),
                        ))
                        .clicked()
                    {
                        self.selected = Some(i);
                    }
                    let mut enabled = band.enabled();
                    if ui
                        .checkbox(&mut enabled, "on")
                        .on_hover_text("Enable / bypass this band")
                        .changed()
                    {
                        band.enabled.store(enabled, Ordering::Relaxed);
                    }
                });

                // Gain fader.
                ui.label(RichText::new("GAIN").size(10.0).color(dim));
                let mut g = band.gain();
                let g_resp = ui.add(
                    Slider::new(&mut g, self.db_min..=self.db_max)
                        .vertical()
                        .show_value(true)
                        .trailing_fill(true),
                );
                let g_resp = g_resp.on_hover_text("Gain of this band in dB");
                if g_resp.changed() {
                    band.set_gain(g);
                }

                // Filter shape.
                let kind_resp =
                    egui::ComboBox::from_id_source(("kind", i))
                        .selected_text(band.kind().label())
                        .show_ui(ui, |ui| {
                            for k in FilterKind::ALL {
                                if ui.selectable_label(band.kind() == k, k.label()).clicked() {
                                    band.set_kind(k);
                                }
                            }
                        });
                kind_resp.response.on_hover_text("Filter shape");

                // Frequency, log scale.
                ui.horizontal(|ui| {
                    ui.label(RichText::new("FREQ").size(10.0).color(dim));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(RichText::new(fmt_freq(band.freq())).size(10.0).color(dim));
                    });
                });
                let mut f = band.freq();
                let f_resp = ui.add(
                    Slider::new(&mut f, MIN_FREQ..=MAX_FREQ)
                        .logarithmic(true)
                        .show_value(false),
                );
                let f_resp = f_resp.on_hover_text("Center frequency");
                if f_resp.changed() {
                    band.set_freq(f);
                }

                // Q bandwidth.
                ui.horizontal(|ui| {
                    ui.label(RichText::new("BANDWIDTH").size(10.0).color(dim));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(RichText::new(format!("Q {:.2}", band.q())).size(10.0).color(dim));
                    });
                });
                let mut q = band.q();
                let q_resp = ui.add(Slider::new(&mut q, 0.1..=10.0).show_value(false));
                let q_resp = q_resp.on_hover_text(
                    "Filter Q: how wide the band's effect is. Small number = wide, \
                     large number = a very narrow spike/notch.",
                );
                if q_resp.changed() {
                    band.set_q(q);
                }
            });
        });
    }

    /// A console-style rotary knob. Rotate it by scrolling while the pointer
    /// is over it, or click and drag up/down (drag up = clockwise). `log` uses
    /// a logarithmic scale (frequency); everything else is linear. Returns the
    /// possibly-adjusted value plus its `Response` (for tooltips).
    fn knob(
        ui: &mut egui::Ui,
        value: f32,
        min: f32,
        max: f32,
        log: bool,
        color: Color32,
    ) -> (egui::Response, f32) {
        let size = Vec2::new(44.0, 44.0);
        let (rect, resp) = ui.allocate_exact_size(size, Sense::click_and_drag());

        if ui.is_rect_visible(rect) {
            let painter = ui.painter();
            let center = rect.center();
            let r = (rect.width() * 0.5 - 7.0).max(6.0);
            let t = if log {
                ((value / min).ln() / (max / min).ln()).clamp(0.0, 1.0)
            } else {
                ((value - min) / (max - min)).clamp(0.0, 1.0)
            };
            let start = 225.0_f32.to_radians(); // 7 o'clock
            let sweep = 270.0_f32.to_radians(); // 270° of rotation, like hardware
            let vis = ui.visuals();
            let body = vis.widgets.inactive.bg_fill;
            let fg = vis.widgets.inactive.fg_stroke.color;

            // Knob body + a thin rim.
            painter.circle_filled(center, r, body);
            painter.circle_stroke(center, r, Stroke::new(1.5_f32, fg.linear_multiply(0.5_f32)));

            // Value arc sweeping clockwise from 7 o'clock.
            if t > 0.001 {
                let pts: Vec<Pos2> = (0..=28)
                    .map(|k| {
                        let a = start + sweep * (k as f32 / 28.0) * t;
                        center + Vec2::new(a.cos() * (r - 1.0), a.sin() * (r - 1.0))
                    })
                    .collect();
                painter.add(egui::Shape::line(pts, Stroke::new(2.5_f32, color)));
            }
            // Pointer.
            let ang = start + sweep * t;
            let dir = Vec2::new(ang.cos(), ang.sin());
            painter.line_segment(
                [center, center + dir * (r - 6.0)],
                Stroke::new(3.0_f32, fg),
            );
        }

        let mut new_val = value;
        let mut changed = false;
        if resp.hovered() {
            let scroll = ui.input(|i| i.raw_scroll_delta.y);
            if scroll.abs() > 0.0 {
                let step = if log {
                    value * (max / min).powf(scroll * 0.002)
                } else {
                    value + scroll * (max - min) * 0.0004
                };
                new_val = step;
                changed = true;
            }
        }
        if resp.dragged() {
            let dy = resp.drag_delta().y;
            if dy.abs() > 0.0 {
                let step = if log {
                    value * (max / min).powf(-dy * 0.003)
                } else {
                    value - dy * (max - min) * 0.0015
                };
                new_val = step;
                changed = true;
            }
        }
        if changed {
            new_val = new_val.clamp(min, max);
        }
        (resp, new_val)
    }

    /// Knob-style band tile: a channel-EQ section rendered as three rotary
    /// knobs (GAIN / FREQ / BANDWIDTH) that you turn with the scroll wheel or
    /// by dragging straight up/down. In console mode the ranges clamp to the
    /// desk (±15 dB, Q 0.5-5.0) and the filter type is fixed; otherwise it
    /// uses the full ranges and a type dropdown, like the slider tiles.
    fn knob_band_strip(&mut self, ui: &mut egui::Ui, i: usize) {
        let color = Self::band_color(i);
        let selected = self.selected == Some(i);
        let dim = self.grid_label();
        let band = &self.state.bands[i];
        let enabled = band.enabled();
        let knob_color = if enabled {
            color
        } else {
            color.linear_multiply(0.35)
        };
        let (qmin, qmax) = if self.console_mode {
            (CONSOLE_Q_MIN, CONSOLE_Q_MAX)
        } else {
            (0.1, 10.0)
        };

        let frame = egui::Frame::default()
            .fill(self.tile_bg())
            .stroke(if selected {
                Stroke::new(1.5_f32, color)
            } else {
                Stroke::new(1.0_f32, self.tile_stroke())
            })
            .rounding(4.0)
            .inner_margin(egui::Margin::symmetric(8.0, 4.0));

        frame.show(ui, |ui| {
            ui.spacing_mut().item_spacing = Vec2::new(3.0, 2.0);
            ui.set_width(158.0);
            ui.vertical(|ui| {
                // Header line: clickable band label + per-band on/off.
                let label_color = if enabled {
                    color
                } else {
                    color.linear_multiply(0.4)
                };
                ui.horizontal(|ui| {
                    if ui
                        .add(egui::SelectableLabel::new(
                            selected,
                            RichText::new(format!("BAND {}", i + 1))
                                .color(label_color)
                                .strong(),
                        ))
                        .clicked()
                    {
                        self.selected = Some(i);
                    }
                    let mut en = enabled;
                    if ui
                        .checkbox(&mut en, "on")
                        .on_hover_text("Enable / bypass this band")
                        .changed()
                    {
                        band.enabled.store(en, Ordering::Relaxed);
                    }
                });

                ui.add_space(2.0);
                ui.horizontal(|ui| {
                    // GAIN
                    ui.vertical(|ui| {
                        ui.label(RichText::new("GAIN").size(10.0).color(dim));
                        let g = band.gain();
                        let (resp, ng) =
                            Self::knob(ui, g, self.db_min, self.db_max, false, knob_color);
                        let _ = resp.on_hover_text("Gain of this band in dB");
                        if ng != g {
                            band.set_gain(ng);
                        }
                        ui.label(
                            RichText::new(format!("{:+4.2} dB", band.gain()))
                                .size(9.0)
                                .color(dim),
                        );
                    });
                    // FREQ
                    ui.vertical(|ui| {
                        ui.label(RichText::new("FREQ").size(10.0).color(dim));
                        let f = band.freq();
                        let (resp, nf) =
                            Self::knob(ui, f, MIN_FREQ, MAX_FREQ, true, knob_color);
                        let _ = resp.on_hover_text("Center frequency");
                        if nf != f {
                            band.set_freq(nf);
                        }
                        ui.label(RichText::new(fmt_freq(band.freq())).size(9.0).color(dim));
                    });
                    // BANDWIDTH
                    ui.vertical(|ui| {
                        ui.label(RichText::new("BANDWIDTH").size(10.0).color(dim));
                        let q = band.q();
                        let (resp, nq) =
                            Self::knob(ui, q, qmin, qmax, false, knob_color);
                        let _ = resp.on_hover_text(
                            "Filter Q: how wide the band's effect is. Small number = wide, \
                             large number = a very narrow spike/notch.",
                        );
                        if nq != q {
                            band.set_q(nq);
                        }
                        ui.label(RichText::new(format!("Q {:.2}", band.q())).size(9.0).color(dim));
                    });
                });

                ui.add_space(1.0);
                if self.console_mode {
                    ui.label(RichText::new(band.kind().label()).size(10.0).color(dim));
                } else {
                    egui::ComboBox::from_id_source(("knd", i))
                        .selected_text(band.kind().label())
                        .show_ui(ui, |ui| {
                            for k in FilterKind::ALL {
                                if ui.selectable_label(band.kind() == k, k.label()).clicked() {
                                    band.set_kind(k);
                                }
                            }
                        });
                }
            });
        });
    }

    fn device_selection_window(&mut self, ctx: &egui::Context) {
        if !self.show_device_selection {
            return;
        }
        let Some((ref mut engine_opt, ref mut status_msg)) = self.selected_device else {
            self.show_device_selection = false;
            return;
        };

        let mut close_window = false;
        let mut load_file_clicked = false;
        let mut create_engine_clicked = false;

        egui::Window::new("Select Audio Devices").show(ctx, |ui| {
            ui.label("Output device:");
            egui::ComboBox::from_id_source("output_device_combo")
                .selected_text(self.selected_output.clone().unwrap_or_default())
                .show_ui(ui, |ui| {
                    for name in &self.available_outputs {
                        if ui
                            .selectable_label(
                                self.selected_output.as_ref().is_some_and(|s| s == name),
                                name,
                            )
                            .clicked()
                        {
                            self.selected_output = Some(name.clone());
                        }
                    }
                });

            ui.label("Input device (optional):");
            egui::ComboBox::from_id_source("input_device_combo")
                .selected_text(self.selected_input.clone().unwrap_or_default())
                .show_ui(ui, |ui| {
                    if ui.selectable_label(self.selected_input.is_none(), "<none>").clicked() {
                        self.selected_input = None;
                    }
                    for name in &self.available_inputs {
                        if ui
                            .selectable_label(
                                self.selected_input.as_ref().is_some_and(|s| s == name),
                                name,
                            )
                            .clicked()
                        {
                            self.selected_input = Some(name.clone());
                        }
                    }
                });

            ui.horizontal(|ui| {
                if ui.button("Create Engine").clicked() {
                    create_engine_clicked = true;
                }
                if ui.button("Load File").clicked() {
                    load_file_clicked = true;
                }
            });
        });

        if create_engine_clicked {
            if let Some(out_name) = self.selected_output.clone() {
                match crate::audio::AudioEngine::try_new_named(
                    self.state.clone(),
                    &out_name,
                    self.selected_input.as_deref(),
                ) {
                    Ok(engine) => *engine_opt = Some(engine),
                    Err(e) => *status_msg = format!("audio: unavailable ({e})"),
                }
            }
            if engine_opt.is_some() {
                self._engine = engine_opt.take();
                self.audio_status = "audio: running".to_string();
                close_window = true;
            }
        }

        if load_file_clicked {
            self.selected_device = None;
            self.show_device_selection = false;
            self.load_file();
            return;
        }

        if close_window {
            self.selected_device = None;
            self.show_device_selection = false;
        }
    }

    fn settings_window(&mut self, ctx: &egui::Context) {
        if !self.show_settings {
            return;
        }
        let mut open = true;
        let mut dirty = false;
        egui::Window::new("Settings").open(&mut open).show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label("Theme:");
                if ui.selectable_value(&mut self.theme_dark, true, "Dark").clicked() {
                    dirty = true;
                }
                if ui.selectable_value(&mut self.theme_dark, false, "Light").clicked() {
                    dirty = true;
                }
            });

            if ui.checkbox(&mut self.idle_anim, "Bar animation on stop").changed() {
                dirty = true;
            }
            if ui.checkbox(&mut self.peak_hold, "Show peak-hold caps").changed() {
                dirty = true;
            }

            ui.separator();

            let before = self.console_mode;
            ui.checkbox(
                &mut self.console_mode,
                "Console mode (training)",
            )
            .on_hover_text(
                "Train on a real desk layout: a fixed 5-band strip, ±15 dB gain, \
                 Q 0.5-5.0. Dots stay draggable like a digital console's \
                 touchscreen; scroll on a dot also turns it like a knob (scroll \
                 = gain, shift+scroll = frequency, ctrl+scroll = Q).",
            );
            if self.console_mode != before {
                if self.console_mode {
                    self.saved_db_min = self.db_min;
                    self.saved_db_max = self.db_max;
                    self.db_min = CONSOLE_DB_MIN;
                    self.db_max = CONSOLE_DB_MAX;
                    self.state.apply_console_mode(true);
                    self.selected = self.selected.filter(|s| *s < self.state.num_bands());
                } else {
                    self.db_min = self.saved_db_min;
                    self.db_max = self.saved_db_max;
                    self.state.apply_console_mode(false);
                    self.selected = None;
                }
                self.clamp_gains();
                dirty = true;
            }

            let btn = ui
                .checkbox(
                    &mut self.knob_ui,
                    "Knob-style band tiles",
                )
                .on_hover_text(
                    "Swap the row of sliders under the graph for analog rotary \
                     knobs (GAIN / FREQ / BANDWIDTH). Spin them with the scroll \
                     wheel or drag straight up/down.",
                );
            if btn.changed() {
                dirty = true;
            }

            ui.horizontal(|ui| {
                ui.label("Bands:");
                let cur = self.state.num_bands();
                if ui
                    .add_enabled(!self.console_mode && cur > 2, egui::Button::new("-"))
                    .on_hover_text("Remove the last band and spread the rest")
                    .clicked()
                {
                    self.state.remove_band();
                    self.state.evenly_spread_bands();
                    self.selected = self.selected.filter(|s| *s < self.state.num_bands());
                    dirty = true;
                }
                ui.label(format!("{}", cur));
                if ui
                    .add_enabled(!self.console_mode && cur < MAX_BANDS, egui::Button::new("+"))
                    .on_hover_text("Add a band and spread them evenly")
                    .clicked()
                {
                    self.state.add_band();
                    self.state.evenly_spread_bands();
                    dirty = true;
                }
            });

            ui.separator();

            let mut floor = self.db_min;
            if ui
                .add_enabled(
                    !self.console_mode,
                    Slider::new(&mut floor, -60.0..=0.0).text("dB floor"),
                )
                .changed()
            {
                self.db_min = floor;
                self.clamp_gains();
                dirty = true;
            }
            let mut roof = self.db_max;
            if ui
                .add_enabled(
                    !self.console_mode,
                    Slider::new(&mut roof, 0.0..=60.0).text("dB roof"),
                )
                .changed()
            {
                self.db_max = roof;
                self.clamp_gains();
                dirty = true;
            }

            ui.separator();

            let mut vmin = self.vol_min;
            if ui
                .add(Slider::new(&mut vmin, 0.05..=1.0).text("Volume min (x)"))
                .changed()
            {
                self.vol_min = vmin;
                self.vol_max = self.vol_max.max(self.vol_min + 0.05);
                dirty = true;
            }
            let mut vmax = self.vol_max;
            if ui
                .add(Slider::new(&mut vmax, 1.0..=5.0).text("Volume max (x)"))
                .changed()
            {
                self.vol_max = vmax.max(self.vol_min + 0.05);
                dirty = true;
            }

            ui.separator();
            if ui.button("Reset settings").clicked() {
                self.theme_dark = true;
                self.idle_anim = true;
                self.peak_hold = true;
                self.db_min = DEFAULT_DB_MIN;
                self.db_max = DEFAULT_DB_MAX;
                self.vol_min = DEFAULT_VOL_MIN;
                self.vol_max = DEFAULT_VOL_MAX;
                if self.console_mode {
                    self.db_min = CONSOLE_DB_MIN;
                    self.db_max = CONSOLE_DB_MAX;
                    self.state.apply_console_mode(true);
                }
                self.clamp_gains();
                dirty = true;
            }
        });
        if !open {
            self.show_settings = false;
        }
        if dirty {
            save_settings(self);
        }
    }

    /// Player strip below the graph: a scrubbable progress bar with a m:ss
    /// readout. Play/pause lives in the top bar, volume in the side panel.
    /// Position and duration are tracked by the audio thread in SharedState,
    /// so this works for any decoder (including ones rodio can't measure).
    fn transport_bar(&mut self, ui: &mut egui::Ui) {
        let engine_present = self._engine.is_some();
        let is_file = engine_present
            && self._engine.as_ref().map(|e| e.is_file()).unwrap_or(false);
        let total_s = self.state.total_ms.load(Ordering::Relaxed) as f32 / 1000.0;
        let has_progress = is_file && total_s > 0.0;

        // While the scrubber is being dragged keep the hand-picked value,
        // otherwise follow the real playback position.
        let mut pos = if self.seek_drag {
            self.seek_pos
        } else {
            self.state.play_ms.load(Ordering::Relaxed) as f32 / 1000.0
        };
        pos = pos.clamp(0.0, total_s.max(0.001));

        ui.horizontal(|ui| {
            let scrub_w = (ui.available_width() - 90.0).max(40.0);
            let slider = Slider::new(&mut pos, 0.0..=total_s.max(0.001)).show_value(false);
            let resp = ui
                .add_enabled_ui(has_progress, |ui| {
                    ui.add_sized([scrub_w, 18.0], slider)
                })
                .inner;
            if resp.drag_started() {
                self.seek_drag = true;
            }
            if resp.dragged() {
                self.seek_pos = pos;
                let target = (pos * 1000.0).round() as u64;
                self.state.seek_to_ms(target);
                // Update the display immediately; the audio thread applies the
                // jump as soon as it pulls the next chunk.
                self.state.play_ms.store(target.min(total_s as u64 * 1000), Ordering::Relaxed);
            }
            if resp.drag_released() {
                self.seek_drag = false;
            }

            ui.label(format!("{} / {}", Self::fmt_time(pos), Self::fmt_time(total_s)));
        });
    }

    /// Vertical master-volume fader, shown in the side panel next to the
    /// graph. Its track runs the full height of the graph exactly.
    fn volume_fader(&mut self, ui: &mut egui::Ui) {
        ui.vertical_centered(|ui| {
            ui.add_space(4.0);
            ui.label("Vol");
            let lo = self.vol_min;
            let hi = self.vol_max.max(lo + 0.05);
            let mut v = self.volume_gain.clamp(lo, hi);
            let rail = (ui.available_height() - 12.0).max(60.0);
            ui.spacing_mut().slider_width = rail;
            let vr = ui
                .add(Slider::new(&mut v, lo..=hi).vertical().suffix("x").fixed_decimals(2))
                .on_hover_text("Overall output volume");
            if vr.changed() {
                self.volume_gain = v;
                self.state.master_gain.store(v.to_bits(), Ordering::Relaxed);
            }
        });
    }

    fn fmt_time(s: f32) -> String {
        let s = s.max(0.0) as u32;
        if s >= 3600 {
            format!("{:02}:{:02}:{:02}", s / 3600, (s % 3600) / 60, s % 60)
        } else {
            format!("{:02}:{:02}", s / 60, s % 60)
        }
    }

    /// GNOME/Wayland refuses to draw OS titlebars, so on Linux we paint a
    /// headerbar in the style of libadwaita apps (like GNOME Settings) with
    /// working window controls. Windows/macOS keep their real OS titlebars.
    #[cfg(target_os = "linux")]
    fn draw_titlebar(&mut self, ctx: &egui::Context) {
        let dark = self.theme_dark;
        let (fill, text, hover_fill) = if dark {
            (
                Color32::from_rgb(34, 36, 41),
                Color32::from_rgb(208, 212, 218),
                Color32::from_rgb(70, 74, 82),
            )
        } else {
            (
                Color32::from_rgb(245, 245, 246),
                Color32::from_rgb(48, 52, 58),
                Color32::from_rgb(202, 202, 204),
            )
        };

        egui::TopBottomPanel::top("native_titlebar")
            .resizable(false)
            .exact_height(34.0)
            .frame(egui::Frame::none().fill(fill))
            .show(ctx, |ui| {
                let bar = ui.available_rect_before_wrap();
                let painter = ui.painter();
                let controls_w = 124.0;

                // Drag region (everything except the window controls).
                let drag_rect = Rect::from_min_max(
                    bar.min,
                    Pos2::new(bar.right() - controls_w, bar.bottom()),
                );
                let drag = ui.interact(
                    drag_rect,
                    ui.id().with("titlebar_drag"),
                    Sense::click_and_drag(),
                );
                if drag.drag_started() {
                    ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
                }
                if drag.double_clicked() {
                    let maximized = ctx.input(|i| i.viewport().maximized).unwrap_or(false);
                    ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(!maximized));
                }

                // Title.
                painter.text(
                    Pos2::new(bar.min.x + 12.0, bar.center().y),
                    Align2::LEFT_CENTER,
                    "eqvis",
                    FontId::proportional(13.0),
                    text,
                );

                // Minimize / Maximize / Close as GNOME-style circular buttons.
                let stride = 44.0;
                let close_x = bar.right() - 16.0;
                for (i, dx) in [0.0, stride, 2.0 * stride].into_iter().enumerate() {
                    let center = Pos2::new(close_x - dx, bar.center().y);
                    let rect = Rect::from_center_size(center, Vec2::splat(28.0));
                    let id = match i {
                        0 => ui.id().with("win_close"),
                        1 => ui.id().with("win_maximize"),
                        _ => ui.id().with("win_minimize"),
                    };
                    let resp = ui.interact(rect, id, Sense::click());
                    let hovered = resp.hovered();

                    if i == 0 {
                        // Close: fills red on hover, ghost circle otherwise.
                        let c = if hovered {
                            Color32::from_rgb(238, 50, 58)
                        } else {
                            fill
                        };
                        painter.circle_filled(center, 12.5, c);
                        painter.circle_stroke(center, 12.5, Stroke::new(1.4_f32, text));
                        painter.line_segment(
                            [center + Vec2::new(-4.0, 4.0), center + Vec2::new(4.0, -4.0)],
                            Stroke::new(1.6_f32, Color32::WHITE),
                        );
                        painter.line_segment(
                            [center + Vec2::new(-4.0, -4.0), center + Vec2::new(4.0, 4.0)],
                            Stroke::new(1.6_f32, Color32::WHITE),
                        );
                    } else {
                        let c = if hovered { hover_fill } else { fill };
                        painter.circle_filled(center, 12.5, c);
                        painter.circle_stroke(center, 12.5, Stroke::new(1.4_f32, text));
                        if i == 1 {
                            // Maximize / restore glyph.
                            let g = Stroke::new(1.5_f32, text);
                            painter.rect_stroke(
                                Rect::from_center_size(center, Vec2::new(9.0, 8.0)),
                                1.0,
                                g,
                            );
                        } else {
                            // Minimize glyph.
                            painter.line_segment(
                                [center + Vec2::new(-6.0, 0.0), center + Vec2::new(6.0, 0.0)],
                                Stroke::new(1.5_f32, text),
                            );
                        }
                    }

                    if resp.clicked() {
                        match i {
                            0 => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
                            1 => {
                                let maximized =
                                    ctx.input(|i| i.viewport().maximized).unwrap_or(false);
                                ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(!maximized));
                            }
                            _ => ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true)),
                        };
                    }
                }
            });
    }
}

impl eframe::App for EqApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        ctx.request_repaint(); // keep meters/curve live

        // Apply the chosen theme every frame so the toggle takes effect live.
        ctx.set_visuals(if self.theme_dark {
            egui::Visuals::dark()
        } else {
            egui::Visuals::light()
        });

        #[cfg(target_os = "linux")]
        self.draw_titlebar(ctx);

        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            ui.add_space(2.0);
            ui.horizontal(|ui| {
                ui.heading("eqvis");
                ui.separator();

                if ui.button("Settings").clicked() {
                    self.show_settings = !self.show_settings;
                }
                if ui.button("Flatten").clicked() {
                    if self.console_mode {
                        self.state.apply_console_mode(true);
                    } else {
                        self.state.flatten();
                    }
                }
                ui.separator();
                ui.label("Bands:");
                if ui
                    .add_enabled(!self.console_mode, egui::Button::new("-"))
                    .on_hover_text("Remove the last band and spread the rest")
                    .clicked()
                {
                    self.state.remove_band();
                    self.state.evenly_spread_bands();
                    self.selected = self.selected.filter(|s| *s < self.state.num_bands());
                    save_settings(self);
                }
                ui.label(format!("{}", self.state.num_bands()));
                if ui
                    .add_enabled(!self.console_mode, egui::Button::new("+"))
                    .on_hover_text("Add a band and spread them evenly")
                    .clicked()
                {
                    self.state.add_band();
                    self.state.evenly_spread_bands();
                    save_settings(self);
                }
                ui.separator();
                if self._engine.is_none() {
                    if ui.button("Start Audio").clicked() {
                        self.show_device_selection = true;
                        self.selected_device = Some((None, String::new()));
                        self.refresh_device_lists();
                    }
                    if ui.button("Load File").clicked() {
                        self.load_file();
                    }
                } else {
                    if ui.button(if self.paused { "Unpause" } else { "Pause" }).clicked() {
                        self.paused = !self.paused;
                        if let Some(engine) = &self._engine {
                            engine.set_paused(self.paused);
                        }
                    }
                    if ui.button("Stop Audio").clicked() {
                        self.paused = false;
                        self._engine = None;
                        self.state.reset_transport();
                        self.seek_drag = false;
                        self.audio_status = "audio: disabled".to_string();
                    }
                }

                ui.separator();
                ui.label(&self.audio_status);
                ui.separator();

                let in_peak = f32::from_bits(self.state.input_peak.load(Ordering::Relaxed));
                let out_peak = f32::from_bits(self.state.output_peak.load(Ordering::Relaxed));
                ui.label(format!("in {:>5.1} dB", lin_to_db(in_peak)));
                ui.label(format!("out {:>5.1} dB", lin_to_db(out_peak)));
                if in_peak > 0.0001 {
                    ui.colored_label(Color32::GREEN, " lve");
                } else {
                    ui.colored_label(Color32::GRAY, " silent");
                }
            });
            ui.add_space(2.0);
        });

        self.device_selection_window(ctx);
        self.settings_window(ctx);

        egui::TopBottomPanel::bottom("bands")
            .resizable(true)
            .default_height(216.0)
            .min_height(120.0)
            .max_height(340.0)
            .show(ctx, |ui| {
            ui.add_space(4.0);
            if self.console_mode || self.knob_ui {
                let knob_mode = self.knob_ui;
                let scrollable = !self.console_mode && knob_mode;
                let add_strip = |ui: &mut egui::Ui, app: &mut EqApp| {
                    ui.horizontal(|ui| {
                        for i in 0..app.state.num_bands() {
                            app.knob_band_strip(ui, i);
                        }
                    });
                };
                if scrollable {
                    egui::ScrollArea::horizontal()
                        .auto_shrink([false, false])
                        .show(ui, |ui| add_strip(ui, self));
                } else {
                    add_strip(ui, self);
                }
            } else {
                egui::ScrollArea::horizontal()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            for i in 0..self.state.num_bands() {
                                self.band_strip(ui, i);
                            }
                        });
                    });
            }
            ui.add_space(4.0);
        });

        egui::SidePanel::right("volume")
            .resizable(false)
            .default_width(72.0)
            .show(ctx, |ui| {
                self.volume_fader(ui);
            });

        egui::TopBottomPanel::bottom("transport")
            .resizable(false)
            .default_height(104.0)
            .show(ctx, |ui| {
                self.transport_bar(ui);
            });

        egui::CentralPanel::default().show(ctx, |ui| {
            let rect = ui.available_rect_before_wrap();
            let response = ui.allocate_rect(rect, Sense::click_and_drag());
            let painter = ui.painter_at(rect);

            painter.rect_filled(rect, 0.0, self.graph_bg());

            // Translucent shade under the response line, then the analyzer on
            // top so the bars never get tinted yellow.
            self.draw_response_fill(&painter, rect);
            self.update_visualizer();
            self.draw_spectrum(ui, &painter, rect);
            self.draw_grid(&painter, rect);
            self.draw_response_curve(&painter, rect);
            self.handle_graph_input(ui, &response, rect);
            let now = ui.input(|i| i.time as f32);
            self.draw_nodes(&painter, rect, now);
        });
    }

    /// Persist settings (theme, animation, dB floor/roof) on shutdown.
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        save_settings(self);
    }
}

fn lin_to_db(x: f32) -> f32 {
    if x <= 0.0001 {
        -80.0
    } else {
        20.0 * x.log10()
    }
}

fn fmt_freq(f: f32) -> String {
    if f >= 1000.0 {
        format!("{:.2}k", f / 1000.0)
    } else {
        format!("{:.2}", f)
    }
}

impl EqApp {
    fn draw_grid(&self, painter: &egui::Painter, rect: Rect) {
        let grid_stroke = Stroke::new(1.0_f32, self.grid_line());
        for freq in [20.0, 50.0, 100.0, 200.0, 500.0, 1000.0, 2000.0, 5000.0, 10000.0, 20000.0] {
            let x = Self::freq_to_x(freq, rect);
            painter.line_segment(
                [Pos2::new(x, rect.top()), Pos2::new(x, rect.bottom())],
                grid_stroke,
            );
            let label = if freq >= 1000.0 {
                format!("{:.0}k", freq / 1000.0)
            } else {
                format!("{:.0}", freq)
            };
            painter.text(
                Pos2::new(x + 2.0, rect.bottom() - 14.0),
                Align2::LEFT_BOTTOM,
                label,
                FontId::proportional(10.0),
                self.grid_label(),
            );
        }
        for db in self.gridlines() {
            let y = self.db_to_y(db, rect);
            let stroke = if db == 0.0 {
                Stroke::new(1.5_f32, self.zero_line())
            } else {
                grid_stroke
            };
            painter.line_segment(
                [Pos2::new(rect.left(), y), Pos2::new(rect.right(), y)],
                stroke,
            );
            painter.text(
                Pos2::new(rect.left() + 2.0, y),
                Align2::LEFT_BOTTOM,
                format!("{:+.0}", db),
                FontId::proportional(10.0),
                self.grid_label(),
            );
        }
    }

    /// Translucent shade from the response line straight down to the bottom of
    /// the graph, built from one convex vertical slice per curve sample. Each
    /// slice can never self-intersect, so no brighter overlapping regions
    /// appear no matter how the curve wiggles. Drawn before the analyzer so
    /// the bars stay on top and never get tinted yellow.
    fn draw_response_fill(&self, painter: &egui::Painter, rect: Rect) {
        let points = self.response_curve_points(rect);
        if points.len() < 2 {
            return;
        }

        let fill = Color32::from_rgba_unmultiplied(255, 200, 60, 22);
        let bottom = rect.bottom();
        let mut shapes: Vec<egui::Shape> = Vec::with_capacity(points.len());
        for w in points.windows(2) {
            let p0 = w[0];
            let p1 = w[1];
            if p0.x == p1.x {
                continue;
            }
            shapes.push(egui::Shape::convex_polygon(
                vec![
                    p0,
                    p1,
                    Pos2::new(p1.x, bottom),
                    Pos2::new(p0.x, bottom),
                ],
                fill,
                Stroke::NONE,
            ));
        }
        painter.extend(shapes);
    }

    fn draw_response_curve(&self, painter: &egui::Painter, rect: Rect) {
        let points = self.response_curve_points(rect);

        painter.add(egui::Shape::line(
            points,
            Stroke::new(2.5_f32, Color32::from_rgb(255, 200, 60)),
        ));
    }

    /// The real frequency response: sum of every enabled band's biquad magnitude
    /// (in dB) sampled at log-spaced frequencies. A band's Q is what widens or
    /// narrows its bump/notch - set a low gain with a high Q and you get a very
    /// narrow notch, exactly like the strip actually sounds.
    fn response_curve_points(&self, rect: Rect) -> Vec<Pos2> {
        const NPOINTS: usize = 256;
        let sr = self.state.sample_rate().max(1.0);
        let nb = self.state.num_bands();
        let lf = MIN_FREQ.ln();
        let hf = MAX_FREQ.ln();
        let mut pts = Vec::with_capacity(NPOINTS + 1);
        for i in 0..=NPOINTS {
            let freq = (lf + (hf - lf) * i as f32 / NPOINTS as f32).exp();
            let mut db = 0.0f32;
            for band in self.state.bands.iter().take(nb) {
                if band.enabled() {
                    let mut b = Biquad::default();
                    b.set_coeffs(band.kind(), sr, band.freq(), band.gain(), band.q());
                    db += b.magnitude_db(freq, sr);
                }
            }
            let y = self.db_to_y(db.clamp(self.db_min, self.db_max), rect);
            pts.push(Pos2::new(Self::freq_to_x(freq, rect), y));
        }
        pts
    }

    fn handle_graph_input(&mut self, ui: &egui::Ui, response: &egui::Response, rect: Rect) {
        let radius = 8.0;

        // Dots are draggable in both modes. Modern digital consoles - and the
        // touchscreen desks many schools use - work exactly this way: grab a
        // dot and drag it to set gain + frequency.
        if response.clicked() {
            if let Some(pos) = response.interact_pointer_pos() {
                self.selected = self.nearest_node(pos, rect, radius * 3.0);
            }
        }
        if response.drag_started() {
            if let Some(pos) = response.interact_pointer_pos() {
                self.selected = self.nearest_node(pos, rect, radius * 3.0);
            }
        }
        if response.dragged() {
            if let Some(i) = self.selected {
                if let Some(pos) = response.interact_pointer_pos() {
                    let band = &self.state.bands[i];
                    band.set_freq(Self::x_to_freq(pos.x, rect));
                    band.set_gain(self.y_to_db(pos.y, rect).clamp(self.db_min, self.db_max));
                }
            }
        }
        // Scroll wheel, like turning a knob on the desk in front of you.
        if response.hovered() {
            if let Some(pos) = response.hover_pos() {
                let scroll = ui.input(|i| i.raw_scroll_delta.y);
                if scroll.abs() > 0.0 {
                    if let Some(n) = self.nearest_node(pos, rect, radius * 3.0) {
                        let band = &self.state.bands[n];
                        let now = ui.input(|i| i.time as f32);
                        if self.console_mode {
                            // Console knobs: scroll = gain, shift + scroll =
                            // frequency, ctrl + scroll = bandwidth/Q.
                            self.selected = Some(n);
                            let shift = ui.input(|i| i.modifiers.shift);
                            let ctrl = ui.input(|i| i.modifiers.ctrl);
                            if shift {
                                let f = (band.freq() * (MAX_FREQ / MIN_FREQ).powf(scroll * 0.005))
                                    .clamp(MIN_FREQ, MAX_FREQ);
                                band.set_freq(f);
                                self.q_hint = Some((n, fmt_freq(f), now));
                            } else if ctrl {
                                let q =
                                    (band.q() + scroll * 0.06).clamp(CONSOLE_Q_MIN, CONSOLE_Q_MAX);
                                band.set_q(q);
                                self.q_hint = Some((n, format!("Q {:.2}", q), now));
                            } else {
                                let g = (band.gain() + scroll * CONSOLE_GAIN_STEP)
                                    .clamp(CONSOLE_DB_MIN, CONSOLE_DB_MAX);
                                band.set_gain(g);
                                self.q_hint = Some((n, format!("{:+4.2} dB", g), now));
                            }
                        } else {
                            // Bandwidth/Q, like turning a console's bandwidth
                            // knob. A floating badge pops up so the change is
                            // obvious.
                            let q = (band.q() + scroll * 0.02).clamp(0.1, 10.0);
                            band.set_q(q);
                            self.q_hint = Some((n, format!("Q {:.2}", q), now));
                        }
                    }
                }
            }
        }
    }

    fn draw_nodes(&self, painter: &egui::Painter, rect: Rect, now: f32) {
        let radius = 8.0;
        for (i, band) in self.state.bands.iter().take(self.state.num_bands()).enumerate() {
            let node_pos = Pos2::new(
                Self::freq_to_x(band.freq(), rect),
                self.db_to_y(band.gain(), rect),
            );
            let mut color = Self::band_color(i);
            if !band.enabled() {
                color = color.linear_multiply(0.35);
            }
            let r = if self.selected == Some(i) { radius + 2.0 } else { radius };

            // Soft halo so the handles read as crisp "perfect" dots.
            painter.circle_filled(
                node_pos,
                r + 7.0,
                Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), 36),
            );
            // Solid white pinring + colored core.
            painter.circle_filled(node_pos, r, self.pin_ring());
            painter.circle_filled(
                node_pos,
                r - 3.0,
                if self.selected == Some(i) {
                    color
                } else {
                    color.linear_multiply(0.85)
                },
            );
            // Q gauge: a translucent arc that fills the ring of space between
            // the rim of the knob and the outer white boundary ring. The angle
            // of fullness is q/10 * 360°, so a full circle = Q 10 (very
            // narrow), a small arc = wide bandwidth.
            let q = band.q().clamp(0.1, 10.0);
            let sweep = q / 10.0 * std::f32::consts::TAU;
            if sweep > 0.015 {
                let r_in = r + 0.5;
                let r_out = r + 3.4;
                let mid = (r_in + r_out) * 0.5;
                let arc_w = r_out - r_in;
                const STEPS: usize = 48;
                let start = -std::f32::consts::FRAC_PI_2;
                let pts: Vec<Pos2> = (0..=STEPS)
                    .map(|k| {
                        let a = start + sweep * (k as f32 / STEPS as f32);
                        node_pos + Vec2::new(a.cos() * mid, a.sin() * mid)
                    })
                    .collect();
                painter.add(egui::Shape::line(
                    pts,
                    Stroke::new(
                        arc_w,
                        Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), 110),
                    ),
                ));
            }
            // White boundary ring marking the outer edge of the Q gauge.
            painter.circle_stroke(
                node_pos,
                r + 4.0,
                Stroke::new(
                    1.5_f32,
                    self.pin_ring().linear_multiply(if self.selected == Some(i) { 1.0_f32 } else { 0.7_f32 }),
                ),
            );
            painter.text(
                node_pos + Vec2::new(0.0, -r - 10.0),
                Align2::CENTER_BOTTOM,
                format!("{}", i + 1),
                FontId::proportional(11.0),
                color,
            );
        }

        // Floating value badge while scrolling a knob/dot in either mode.
        if let Some((n, ref text, t)) = self.q_hint {
            if now - t < 0.9 && n < self.state.num_bands() {
                let node_pos = Pos2::new(
                    Self::freq_to_x(self.state.bands[n].freq(), rect),
                    self.db_to_y(self.state.bands[n].gain(), rect),
                );
                painter.text(
                    node_pos + Vec2::new(0.0, 22.0),
                    Align2::CENTER_CENTER,
                    text.clone(),
                    FontId::proportional(13.0),
                    Self::band_color(n),
                );
            }
        }
    }

    /// Smooth the raw FFT levels into fast-attack / slow-release display values
    /// and a slow-dropping peak-hold line, so the bars bounce and glide. With
    /// no engine running the bars decay back down to the idle wobble.
    fn update_visualizer(&mut self) {
        let idle = self._engine.is_none() && self.idle_anim;
        for i in 0..NUM_BARS {
            let target = if idle {
                0.0
            } else {
                f32::from_bits(self.state.spectrum[i].load(Ordering::Relaxed))
            };
            let cur = self.vis_levels[i];
            let coeff = if target >= cur { 0.45 } else { 0.09 };
            let next = cur + coeff * (target - cur);
            self.vis_levels[i] = next;
            let p = self.vis_peaks[i];
            self.vis_peaks[i] = if next >= p { next } else { p * 0.95 };
        }
    }

    fn draw_spectrum(&mut self, ui: &egui::Ui, painter: &egui::Painter, rect: Rect) {
        // The analyzer's plot lives below the 0 dB line, the same line the EQ
        // dots sit on. Bars grow up from the bottom of the graph toward it.
        let zero_y = self.db_to_y(0.0, rect);
        let pad = 8.0;
        let top = zero_y + 6.0;
        let bottom = (rect.bottom() - 4.0).max(top + 1.0);
        let plot = Rect::from_min_max(
            Pos2::new(rect.left() + pad, top),
            Pos2::new(rect.right() - pad, bottom),
        );

        painter.rect_filled(plot, 4.0, self.plot_overlay());
        painter.line_segment(
            [Pos2::new(rect.left() + pad, zero_y), Pos2::new(rect.right() - pad, zero_y)],
            Stroke::new(2.0_f32, self.zero_band()),
        );

        let time = ui.input(|i| i.time as f32);
        let max_lvl = self.vis_levels.iter().cloned().fold(0.0f32, f32::max);
        let total = plot.width() / NUM_BARS as f32;
        let bar_w = total * 0.68;
        for i in 0..NUM_BARS {
            let x0 = plot.left() + i as f32 * total + (total - bar_w) * 0.5;
            let x1 = x0 + bar_w;

            let mut level = self.vis_levels[i];
            if self.idle_anim && max_lvl < 0.02 {
                let wobble = 0.04 + 0.03 * (time * 3.2 + i as f32 * 0.55).sin();
                level = level.max(wobble);
            }
            let h = (level * plot.height()).clamp(0.0, plot.height());
            let bar = Rect::from_min_max(
                Pos2::new(x0, plot.bottom() - h),
                Pos2::new(x1, plot.bottom()),
            );

            let slot = Rect::from_min_max(
                Pos2::new(x0, plot.top()),
                Pos2::new(x1, plot.bottom()),
            );
            painter.rect_filled(
                slot,
                bar_w * 0.35,
                self.slot_band(),
            );

            if h > 0.5 {
                let hue = 0.66 - 0.66 * (i as f32 / NUM_BARS as f32);
                let color: Color32 = Hsva::new(hue, 0.85, 0.95, 1.0).into();
                painter.rect_filled(bar, bar_w * 0.35, color);
            }

            let ph = (self.vis_peaks[i] * plot.height()).clamp(0.0, plot.height());
            if self.peak_hold && ph > 2.0 {
                let cy = (plot.bottom() - ph).clamp(plot.top(), plot.bottom());
                painter.line_segment(
                    [Pos2::new(x0 - 1.5, cy), Pos2::new(x1 + 1.5, cy)],
                    Stroke::new(2.0_f32, self.peak_cap()),
                );
            }
        }
    }
}