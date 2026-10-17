//! Imaging tab: capture controls, preview, histogram and clipping checks.

use eframe::egui::{self, Color32, RichText};
use egui_plot::{HoverPosition, Line, Plot, PlotPoints, VLine};
use protocol::equipment::{self, Control};
use protocol::{
    CaptureRequest, CaptureState, ChannelStats, FrameInfo, FrameType, ImagingSettings, ToEngine,
    UiState,
};
use serde::{Deserialize, Serialize};

const RED: Color32 = Color32::from_rgb(220, 60, 60);
const ORANGE: Color32 = Color32::from_rgb(230, 160, 40);
const GREEN: Color32 = Color32::from_rgb(60, 180, 75);

/// Capture settings as entered by the user (persisted by the UI).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CaptureForm {
    exposure: f64,
    frame_type: FrameType,
    binning: u32,
    set_gain: bool,
    gain: f64,
    set_offset: bool,
    offset: f64,
    filter_slot: Option<u32>,
    object: String,
    save: bool,
    repeat: bool,
    log_histogram: bool,
}

impl Default for CaptureForm {
    fn default() -> Self {
        Self {
            exposure: 1.0,
            frame_type: FrameType::Light,
            binning: 1,
            set_gain: false,
            gain: 0.0,
            set_offset: false,
            offset: 0.0,
            filter_slot: None,
            object: String::new(),
            save: true,
            repeat: false,
            log_histogram: true,
        }
    }
}

pub struct ImagingView {
    pub form: CaptureForm,
    texture: Option<(u64, egui::TextureHandle)>,
    scene_rect: egui::Rect,
    fit_pending: bool,
    settings_edit: ImagingSettings,
    show_header: bool,
}

impl ImagingView {
    pub fn new(form: CaptureForm) -> Self {
        Self {
            form,
            texture: None,
            scene_rect: egui::Rect::ZERO,
            fit_pending: true,
            settings_edit: ImagingSettings::default(),
            show_header: false,
        }
    }

    /// The engine's imaging settings changed; refresh the edit buffers.
    pub fn on_settings(&mut self, s: &ImagingSettings) {
        self.settings_edit = s.clone();
    }

    pub fn on_preview(&mut self, ctx: &egui::Context, id: u64, jpeg: &[u8]) {
        let decoded = match image::load_from_memory_with_format(jpeg, image::ImageFormat::Jpeg) {
            Ok(img) => img.to_rgb8(),
            Err(e) => {
                eprintln!("bad preview: {e}");
                return;
            }
        };
        let size = [decoded.width() as usize, decoded.height() as usize];
        let color = egui::ColorImage::from_rgb(size, decoded.as_raw());
        let resized = self
            .texture
            .as_ref()
            .is_none_or(|(_, t)| t.size() != size);
        let texture = ctx.load_texture("preview", color, egui::TextureOptions::LINEAR);
        self.texture = Some((id, texture));
        if resized {
            self.fit_pending = true;
        }
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, state: &UiState, actions: &mut Vec<ToEngine>) {
        egui::Panel::left("capture_controls")
            .resizable(true)
            .default_size(260.0)
            .show(ui, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    self.capture_controls(ui, state, actions);
                    ui.separator();
                    self.output_settings(ui, state, actions);
                });
            });
        egui::Panel::right("histogram")
            .resizable(true)
            .default_size(400.0)
            .show(ui, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| match &state.last_frame {
                    Some(frame) => self.analysis_panel(ui, frame),
                    None => {
                        ui.label(RichText::new("No frame yet.").weak());
                    }
                });
            });
        egui::CentralPanel::default().show(ui, |ui| self.image_panel(ui, state));
    }

    fn capture_controls(&mut self, ui: &mut egui::Ui, state: &UiState, actions: &mut Vec<ToEngine>) {
        ui.heading("Capture");
        let devices = &state.cache.devices;
        let camera = state.roles.main_camera.as_ref().and_then(|c| devices.get(c));
        let wheel = state.roles.filter_wheel.as_ref().and_then(|w| devices.get(w));
        match (&state.roles.main_camera, camera) {
            (None, _) => {
                ui.colored_label(ORANGE, "Assign a main camera in Equipment.");
            }
            (Some(name), None) => {
                ui.colored_label(ORANGE, format!("{name} is offline."));
            }
            (Some(name), Some(cam)) => {
                ui.label(RichText::new(name).strong());
                if let Some(t) = equipment::number(cam, "CCD_TEMPERATURE", "CCD_TEMPERATURE_VALUE") {
                    ui.label(RichText::new(format!("Sensor {:.1} °C", t.value)).weak());
                }
            }
        }

        let f = &mut self.form;
        egui::Grid::new("capture_form")
            .num_columns(2)
            .spacing([8.0, 6.0])
            .show(ui, |ui| {
                ui.label("Type");
                ui.horizontal(|ui| {
                    for t in FrameType::ALL {
                        ui.selectable_value(&mut f.frame_type, t, t.label());
                    }
                });
                ui.end_row();

                ui.label("Exposure");
                ui.add(
                    egui::DragValue::new(&mut f.exposure)
                        .range(0.0..=7200.0)
                        .speed(0.1)
                        .suffix(" s")
                        .max_decimals(3),
                );
                ui.end_row();

                if let Some(cam) = camera {
                    if let Some(bin) = equipment::number(cam, "CCD_BINNING", "HOR_BIN") {
                        ui.label("Binning");
                        let max = bin.max.max(1.0) as u32;
                        ui.add(egui::DragValue::new(&mut f.binning).range(1..=max).prefix("×"));
                        ui.end_row();
                    }
                    for (label, control, enabled, value) in [
                        ("Gain", Control::Gain, &mut f.set_gain, &mut f.gain),
                        ("Offset", Control::Offset, &mut f.set_offset, &mut f.offset),
                    ] {
                        if let Some(n) = equipment::control_value(cam, control) {
                            ui.checkbox(enabled, label).on_hover_text(format!(
                                "Unchecked: leave the camera at {}",
                                indi_link::format_number(n.value, &n.format).trim()
                            ));
                            ui.add_enabled(
                                *enabled,
                                egui::DragValue::new(value).range(n.min..=n.max).speed(1.0),
                            );
                            ui.end_row();
                        }
                    }
                }

                if let Some(w) = wheel {
                    ui.label("Filter");
                    let names = equipment::filter_names(w);
                    let current = equipment::current_filter_slot(w)
                        .and_then(|s| equipment::filter_name(w, s))
                        .unwrap_or_default();
                    let text = match f.filter_slot {
                        Some(s) => equipment::filter_name(w, s).unwrap_or(format!("#{s}")),
                        None => format!("(keep {current})"),
                    };
                    egui::ComboBox::from_id_salt("filter")
                        .selected_text(text)
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut f.filter_slot, None, "(keep current)");
                            for (i, n) in names.iter().enumerate() {
                                ui.selectable_value(&mut f.filter_slot, Some(i as u32 + 1), n);
                            }
                        });
                    ui.end_row();
                }

                ui.label("Object");
                ui.add(egui::TextEdit::singleline(&mut f.object).hint_text("e.g. M 31"));
                ui.end_row();
            });
        ui.checkbox(&mut f.save, "Save FITS");
        ui.checkbox(&mut f.repeat, "Loop (framing / focus)");

        ui.add_space(6.0);
        let busy = state.capture != CaptureState::Idle;
        let repeat = f.repeat;
        ui.horizontal(|ui| {
            if busy {
                if ui.button(RichText::new("Abort").color(RED)).clicked() {
                    actions.push(ToEngine::AbortCapture);
                }
            } else {
                let label = if repeat { "Start loop" } else { "Capture" };
                let ready = camera.is_some_and(|c| c.is_connected());
                if ui
                    .add_enabled(ready, egui::Button::new(RichText::new(label).strong()))
                    .clicked()
                {
                    actions.push(ToEngine::Capture(self.request(camera.is_some_and(|c| {
                        c.properties.contains_key("CCD_BINNING")
                    }))));
                }
            }
        });
        capture_progress(ui, state);
    }

    fn request(&self, has_binning: bool) -> CaptureRequest {
        let f = &self.form;
        CaptureRequest {
            exposure: f.exposure,
            frame_type: f.frame_type,
            binning: has_binning.then_some(f.binning),
            gain: f.set_gain.then_some(f.gain),
            offset: f.set_offset.then_some(f.offset),
            filter_slot: f.filter_slot,
            object: f.object.clone(),
            save: f.save,
            repeat: f.repeat,
        }
    }

    fn output_settings(&mut self, ui: &mut egui::Ui, state: &UiState, actions: &mut Vec<ToEngine>) {
        egui::CollapsingHeader::new("Output files").show(ui, |ui| {
            ui.label("Folder (on the engine's computer)");
            ui.add(egui::TextEdit::singleline(&mut self.settings_edit.output_dir).desired_width(f32::INFINITY));
            ui.label("File name template").on_hover_text(
                "Tokens: {object} {type} {filter} {exposure} {gain} {offset} {temp} {bin} \
                 {camera} {date} (night of the session) {time} (UTC) {seq} {seq:04}",
            );
            ui.add(
                egui::TextEdit::multiline(&mut self.settings_edit.file_template)
                    .desired_rows(2)
                    .desired_width(f32::INFINITY),
            );
            let changed = self.settings_edit != state.imaging;
            ui.horizontal(|ui| {
                if ui.add_enabled(changed, egui::Button::new("Apply")).clicked() {
                    actions.push(ToEngine::SetImagingSettings(self.settings_edit.clone()));
                }
                if ui.add_enabled(changed, egui::Button::new("Revert")).clicked() {
                    self.settings_edit = state.imaging.clone();
                }
                if ui.button("Default template").clicked() {
                    self.settings_edit.file_template = protocol::DEFAULT_FILE_TEMPLATE.into();
                }
            });
        });
    }

    fn image_panel(&mut self, ui: &mut egui::Ui, state: &UiState) {
        let Some((id, texture)) = &self.texture else {
            ui.centered_and_justified(|ui| {
                ui.label(RichText::new("Capture a frame to see it here.").weak());
            });
            return;
        };
        let frame = state.last_frame.as_ref().filter(|f| f.id == *id);
        ui.horizontal(|ui| {
            if ui.button("Fit").clicked() {
                self.fit_pending = true;
            }
            if let Some(f) = frame {
                let mut parts = vec![
                    if f.object.is_empty() { f.frame_type.label().to_owned() } else { f.object.clone() },
                    f.frame_type.label().to_owned(),
                ];
                parts.extend(f.filter.clone());
                parts.push(format!("{} s", f.exposure));
                parts.push(format!("{}×{}", f.width, f.height));
                ui.label(parts.join(" · "));
            }
        });
        if let Some(path) = frame.and_then(|f| f.path.as_deref()) {
            ui.label(RichText::new(path).small().weak());
        }
        let size = texture.size_vec2();
        let image_rect = egui::Rect::from_min_size(egui::Pos2::ZERO, size);
        if std::mem::take(&mut self.fit_pending) {
            self.scene_rect = image_rect;
        }
        egui::Frame::canvas(ui.style()).show(ui, |ui| {
            egui::Scene::new()
                .zoom_range(0.05..=16.0)
                .show(ui, &mut self.scene_rect, |ui| {
                    ui.put(image_rect, egui::Image::new((texture.id(), size)));
                });
        });
    }

    fn analysis_panel(&mut self, ui: &mut egui::Ui, frame: &FrameInfo) {
        let a = &frame.analysis;
        ui.horizontal(|ui| {
            ui.heading("Histogram");
            ui.checkbox(&mut self.form.log_histogram, "Log scale");
        });
        let log = self.form.log_histogram;
        let colors = channel_colors(&a.channels, ui.visuals().text_color());
        let half_bin = a.bin_start(1) / 2.0;
        Plot::new("histogram")
            .height(220.0)
            .allow_scroll(false)
            .include_x(0.0)
            .include_x(a.adc_max as f64)
            .include_y(0.0)
            .y_axis_formatter(move |mark, _| {
                if log {
                    format!("1e{}", mark.value.round())
                } else {
                    format!("{}", mark.value)
                }
            })
            .label_formatter(move |hover| {
                let p = match hover {
                    HoverPosition::NearDataPoint { position, .. } => position,
                    HoverPosition::Elsewhere { position } => position,
                };
                let count: f64 = if log { 10f64.powf(p.y) - 1.0 } else { p.y };
                Some(format!("{} ADU\n{:.0} px", p.x.round(), count.max(0.0)))
            })
            .show(ui, |plot| {
                for (i, hist) in a.histograms.iter().enumerate() {
                    let points: PlotPoints = hist
                        .iter()
                        .enumerate()
                        .map(|(b, &n)| {
                            let y = if log { (n as f64 + 1.0).log10() } else { n as f64 };
                            [a.bin_start(b) + half_bin, y]
                        })
                        .collect();
                    let name = a.channels.get(i).map_or("?", |c| c.name.as_str());
                    plot.line(Line::new(name, points).color(colors[i]).width(1.5));
                }
                plot.vline(VLine::new("0", 0.0).color(RED.gamma_multiply(0.6)));
                plot.vline(VLine::new("saturation", a.adc_max as f64).color(RED.gamma_multiply(0.6)));
            });

        ui.add_space(4.0);
        clipping_report(ui, a);
        ui.add_space(4.0);

        egui::Grid::new("stats").striped(true).num_columns(a.channels.len() + 1).show(ui, |ui| {
            ui.label("");
            for (c, color) in a.channels.iter().zip(&colors) {
                ui.label(RichText::new(&c.name).color(*color).strong());
            }
            ui.end_row();
            let row = |ui: &mut egui::Ui, label: &str, f: &dyn Fn(&ChannelStats) -> String| {
                ui.label(label);
                for c in &a.channels {
                    ui.label(RichText::new(f(c)).monospace());
                }
                ui.end_row();
            };
            row(ui, "Min", &|c| c.min.to_string());
            row(ui, "Max", &|c| c.max.to_string());
            row(ui, "Mean", &|c| format!("{:.1}", c.mean));
            row(ui, "Median", &|c| format!("{:.0}", c.median));
            row(ui, "σ (MAD)", &|c| format!("{:.1}", c.sigma()));
            row(ui, "0.1 %ile", &|c| c.p001.to_string());
            row(ui, "At 0", &|c| format!("{} ({})", c.zero, percent(c.zero_fraction())));
            row(ui, "Saturated", &|c| format!("{} ({})", c.saturated, percent(c.saturated_fraction())));
        });
        ui.label(
            RichText::new(format!(
                "Saturation level {} ADU{}",
                a.adc_max,
                if a.step > 1 { format!(", data steps of {}", a.step) } else { String::new() }
            ))
            .small()
            .weak(),
        );

        ui.add_space(6.0);
        egui::CollapsingHeader::new(format!("FITS header ({} cards)", frame.header.len()))
            .default_open(self.show_header)
            .show(ui, |ui| {
                egui::Grid::new("fits_header").striped(true).num_columns(2).show(ui, |ui| {
                    for (k, v) in &frame.header {
                        ui.label(RichText::new(k).monospace());
                        ui.label(RichText::new(v).monospace());
                        ui.end_row();
                    }
                });
            });
    }
}

fn capture_progress(ui: &mut egui::Ui, state: &UiState) {
    let frames = state.frames_captured;
    let suffix = if frames > 0 { format!(" · {frames} done") } else { String::new() };
    match &state.capture {
        CaptureState::Idle => {
            if frames > 0 {
                ui.label(RichText::new(format!("Idle{suffix}")).weak());
            }
        }
        CaptureState::Preparing { what } => {
            ui.horizontal(|ui| {
                ui.add(egui::Spinner::new());
                ui.label(format!("{what}…{suffix}"));
            });
        }
        CaptureState::Exposing { total, remaining } => {
            let done = if *total > 0.0 { 1.0 - remaining / total } else { 1.0 };
            ui.add(
                egui::ProgressBar::new(done.clamp(0.0, 1.0) as f32)
                    .text(format!("Exposing — {remaining:.0} s left{suffix}")),
            );
        }
        CaptureState::Downloading => {
            ui.horizontal(|ui| {
                ui.add(egui::Spinner::new());
                ui.label(format!("Downloading…{suffix}"));
            });
        }
        CaptureState::Processing => {
            ui.horizontal(|ui| {
                ui.add(egui::Spinner::new());
                ui.label(format!("Processing…{suffix}"));
            });
        }
    }
}

/// Plain-language verdict on black clipping, offset headroom and saturation.
fn clipping_report(ui: &mut egui::Ui, a: &protocol::Analysis) {
    let zero: u64 = a.channels.iter().map(|c| c.zero).sum();
    let saturated: u64 = a.channels.iter().map(|c| c.saturated).sum();
    let pixels: u64 = a.channels.iter().map(|c| c.pixels).sum::<u64>().max(1);
    let headroom = a
        .channels
        .iter()
        .filter_map(ChannelStats::headroom_sigma)
        .fold(f64::INFINITY, f64::min);
    let mut ok = true;
    if zero > 0 {
        ok = false;
        ui.colored_label(
            RED,
            format!(
                "⚠ Black clipping: {zero} px at 0 ({}). Raise the offset.",
                percent(zero as f64 / pixels as f64)
            ),
        );
    } else if headroom < 3.0 {
        ok = false;
        ui.colored_label(
            ORANGE,
            format!("Dark end only {headroom:.1}σ above 0 — consider a higher offset."),
        );
    }
    let sat = saturated as f64 / pixels as f64;
    if sat > 0.01 {
        ok = false;
        ui.colored_label(
            RED,
            format!("⚠ {} saturated — shorten the exposure or lower the gain.", percent(sat)),
        );
    } else if sat > 0.001 {
        ok = false;
        ui.colored_label(ORANGE, format!("{} saturated — check bright stars.", percent(sat)));
    } else if saturated > 0 {
        ui.label(
            RichText::new(format!("{saturated} saturated px (typically star cores)."))
                .weak(),
        );
    }
    if ok {
        let note = if headroom.is_finite() {
            format!("✔ No clipping. Dark end {headroom:.0}σ above 0.")
        } else {
            "✔ No clipping.".into()
        };
        ui.colored_label(GREEN, note);
    }
}

fn channel_colors(channels: &[ChannelStats], mono: Color32) -> Vec<Color32> {
    channels
        .iter()
        .map(|c| match c.name.as_str() {
            "R" => Color32::from_rgb(230, 70, 70),
            "G" => Color32::from_rgb(80, 200, 90),
            "B" => Color32::from_rgb(80, 130, 240),
            _ => mono,
        })
        .collect()
}

fn percent(f: f64) -> String {
    let p = f * 100.0;
    if p == 0.0 {
        "0 %".into()
    } else if p < 0.01 {
        "<0.01 %".into()
    } else {
        format!("{p:.2} %")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use imaging::{Cfa, Image, analyze, encode_jpeg, make_preview};

    fn frame_state(cfa: Option<Cfa>, with_clipping: bool) -> (UiState, Vec<u8>) {
        let (w, h) = (120, 80);
        let data: Vec<u16> = (0..w * h)
            .map(|i| match i {
                0..=9 if with_clipping => 0,
                i if with_clipping && i > w * h - 20 => 65535,
                i => 900 + (i * 7919 % 57) as u16,
            })
            .collect();
        let img = Image { width: w, height: h, channels: 1, data, cfa };
        let analysis = analyze(&img, Some(16));
        let jpeg = encode_jpeg(&make_preview(&img, &analysis, 64), 80);
        let state = UiState {
            last_frame: Some(FrameInfo {
                id: 1,
                camera: "Cam".into(),
                frame_type: FrameType::Light,
                exposure: 2.0,
                filter: Some("Ha".into()),
                object: "M 42".into(),
                width: w,
                height: h,
                path: Some("/tmp/x.fits".into()),
                analysis,
                header: vec![("IMAGETYP".into(), "Light Frame".into())],
            }),
            capture: CaptureState::Exposing { total: 10.0, remaining: 4.0 },
            frames_captured: 2,
            ..UiState::default()
        };
        (state, jpeg)
    }

    /// Lay out the imaging tab headlessly (no GPU) to catch panics.
    fn render(view: &mut ImagingView, state: &UiState, ctx: &egui::Context) -> Vec<ToEngine> {
        let mut actions = Vec::new();
        for _ in 0..3 {
            let _ = ctx.run_ui(egui::RawInput::default(), |ui| view.ui(ui, state, &mut actions));
        }
        actions
    }

    #[test]
    fn renders_mono_and_colour_frames() {
        let ctx = egui::Context::default();
        for (cfa, clipping) in [(None, true), (Cfa::parse("RGGB", 0, 0), false)] {
            let (state, jpeg) = frame_state(cfa, clipping);
            let mut view = ImagingView::new(CaptureForm::default());
            view.on_preview(&ctx, 1, &jpeg);
            assert!(view.texture.is_some());
            let actions = render(&mut view, &state, &ctx);
            assert!(actions.is_empty());
        }
    }

    #[test]
    fn renders_without_frame_or_camera() {
        let ctx = egui::Context::default();
        let mut view = ImagingView::new(CaptureForm::default());
        render(&mut view, &UiState::default(), &ctx);
    }

    #[test]
    fn request_reflects_form() {
        let mut view = ImagingView::new(CaptureForm::default());
        view.form.exposure = 120.0;
        view.form.frame_type = FrameType::Flat;
        view.form.set_gain = true;
        view.form.gain = 100.0;
        view.form.filter_slot = Some(3);
        let req = view.request(false);
        assert_eq!(req.exposure, 120.0);
        assert_eq!(req.frame_type, FrameType::Flat);
        assert_eq!(req.gain, Some(100.0));
        assert_eq!(req.offset, None);
        assert_eq!(req.binning, None);
        assert_eq!(req.filter_slot, Some(3));
    }
}
