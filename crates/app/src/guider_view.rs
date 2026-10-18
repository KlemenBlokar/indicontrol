//! Guiding tab: guide image with star selection, error graph, controls, settings.

use crate::theme;
use eframe::egui::{self, Color32, RichText, Stroke};
use egui_plot::{HLine, Line, Plot, PlotPoints};
use protocol::guider::DecMode;
use protocol::{GuideOutput, GuiderSettings, GuiderState, GuiderStatus, ToEngine, UiState};

const RA_COLOR: Color32 = Color32::from_rgb(80, 140, 240);
const DEC_COLOR: Color32 = Color32::from_rgb(230, 80, 80);
const STAR_COLOR: Color32 = Color32::from_rgb(60, 200, 90);
const LOCK_COLOR: Color32 = Color32::from_rgb(240, 200, 60);
const GRAPH_STEPS: usize = 200;

pub struct GuiderView {
    texture: Option<egui::TextureHandle>,
    settings_edit: GuiderSettings,
}

impl GuiderView {
    pub fn new() -> Self {
        Self {
            texture: None,
            settings_edit: GuiderSettings::default(),
        }
    }

    pub fn on_settings(&mut self, s: &GuiderSettings) {
        self.settings_edit = s.clone();
    }

    pub fn on_preview(&mut self, ctx: &egui::Context, jpeg: &[u8]) {
        match image::load_from_memory_with_format(jpeg, image::ImageFormat::Jpeg) {
            Ok(img) => {
                let rgb = img.to_rgb8();
                let size = [rgb.width() as usize, rgb.height() as usize];
                let color = egui::ColorImage::from_rgb(size, rgb.as_raw());
                self.texture = Some(ctx.load_texture("guide", color, egui::TextureOptions::LINEAR));
            }
            Err(e) => eprintln!("bad guide preview: {e}"),
        }
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, state: &UiState, actions: &mut Vec<ToEngine>) {
        egui::Panel::left("guider_controls")
            .resizable(true)
            .default_size(300.0)
            .show(ui, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    controls(ui, state, actions);
                    ui.separator();
                    status_details(ui, &state.guider, actions);
                    ui.separator();
                    self.settings(ui, state, actions);
                });
            });
        egui::Panel::bottom("guide_graph")
            .resizable(true)
            .default_size(240.0)
            .show(ui, |ui| graph(ui, state));
        egui::CentralPanel::default().show(ui, |ui| self.image(ui, &state.guider, actions));
    }

    fn image(&mut self, ui: &mut egui::Ui, g: &GuiderStatus, actions: &mut Vec<ToEngine>) {
        let Some(tex) = &self.texture else {
            ui.centered_and_justified(|ui| {
                ui.label(RichText::new("Press Loop to see the guide camera.").weak());
            });
            return;
        };
        let tex_size = tex.size_vec2();
        let avail = ui.available_size();
        let scale = (avail.x / tex_size.x).min(avail.y / tex_size.y).max(0.05);
        let response = ui
            .add(
                egui::Image::new((tex.id(), tex_size * scale))
                    .tint(theme::image_tint(ui))
                    .sense(egui::Sense::click()),
            )
            .on_hover_text("Click a star to guide on it");
        let rect = response.rect;
        // Frame pixels → screen. The preview may be smaller than the frame.
        let (fw, fh) = g.frame_size.map_or((tex_size.x, tex_size.y), |(w, h)| (w as f32, h as f32));
        let to_screen = |x: f64, y: f64| {
            egui::pos2(
                rect.left() + (x as f32 + 0.5) / fw * rect.width(),
                rect.top() + (y as f32 + 0.5) / fh * rect.height(),
            )
        };
        let (lock_c, star_c) = (theme::c(ui, LOCK_COLOR), theme::c(ui, STAR_COLOR));
        let painter = ui.painter_at(rect);
        if let Some((lx, ly)) = g.lock {
            let p = to_screen(lx, ly);
            let r = 14.0;
            painter.line_segment([p - egui::vec2(r, 0.0), p + egui::vec2(r, 0.0)], Stroke::new(1.0, lock_c));
            painter.line_segment([p - egui::vec2(0.0, r), p + egui::vec2(0.0, r)], Stroke::new(1.0, lock_c));
        }
        if let Some(s) = &g.star {
            let r = (s.hfd as f32 / fw * rect.width() * 1.5).max(8.0);
            painter.circle_stroke(to_screen(s.x, s.y), r, Stroke::new(1.5, star_c));
        }
        // Secondary stars: bright when they count, faint when ignored.
        for m in g.guide_stars.iter().filter(|m| !m.primary) {
            let color = if m.used { star_c.gamma_multiply(0.7) } else { star_c.gamma_multiply(0.25) };
            painter.circle_stroke(to_screen(m.x, m.y), 6.0, Stroke::new(1.0, color));
        }
        if response.clicked()
            && let Some(pos) = response.interact_pointer_pos()
        {
            let x = ((pos.x - rect.left()) / rect.width() * fw) as f64;
            let y = ((pos.y - rect.top()) / rect.height() * fh) as f64;
            actions.push(ToEngine::GuiderSelectStar { x, y });
        }
    }

    fn settings(&mut self, ui: &mut egui::Ui, state: &UiState, actions: &mut Vec<ToEngine>) {
        egui::CollapsingHeader::new("Settings").show(ui, |ui| {
            let s = &mut self.settings_edit;
            egui::Grid::new("guide_settings").num_columns(2).spacing([8.0, 4.0]).show(ui, |ui| {
                ui.label("Exposure");
                ui.add(egui::DragValue::new(&mut s.exposure).range(0.01..=30.0).speed(0.05).suffix(" s"));
                ui.end_row();
                ui.label("Focal length");
                optional(ui, &mut s.focal_length, 240.0, " mm", "From INDI");
                ui.end_row();
                ui.label("Pixel size");
                optional(ui, &mut s.pixel_size, 3.75, " µm", "From INDI");
                ui.end_row();
                ui.label("Pulses via");
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut s.output, GuideOutput::Mount, "Mount");
                    ui.selectable_value(&mut s.output, GuideOutput::GuideCamera, "Camera ST-4");
                });
                ui.end_row();

                ui.label(RichText::new("RA").strong());
                ui.end_row();
                let p = &mut s.params;
                percent(ui, "Aggressiveness", &mut p.ra_aggressiveness);
                percent(ui, "Hysteresis", &mut p.ra_hysteresis);
                ui.label("Min move");
                ui.add(egui::DragValue::new(&mut p.ra_min_move).range(0.0..=5.0).speed(0.01).suffix(" px"));
                ui.end_row();
                ui.label("Max pulse");
                ui.add(egui::DragValue::new(&mut p.max_ra_pulse_ms).range(10.0..=10000.0).suffix(" ms"));
                ui.end_row();

                ui.label(RichText::new("Dec").strong());
                ui.end_row();
                ui.label("Mode");
                ui.horizontal(|ui| {
                    for (m, l) in [(DecMode::Auto, "Auto"), (DecMode::North, "N"), (DecMode::South, "S"), (DecMode::Off, "Off")] {
                        ui.selectable_value(&mut p.dec_mode, m, l);
                    }
                });
                ui.end_row();
                percent(ui, "Aggressiveness", &mut p.dec_aggressiveness);
                ui.label("Min move");
                ui.add(egui::DragValue::new(&mut p.dec_min_move).range(0.0..=5.0).speed(0.01).suffix(" px"));
                ui.end_row();
                ui.label("Max pulse");
                ui.add(egui::DragValue::new(&mut p.max_dec_pulse_ms).range(10.0..=10000.0).suffix(" ms"));
                ui.end_row();

                ui.label(RichText::new("Calibration").strong());
                ui.end_row();
                ui.label("Step");
                ui.add(egui::DragValue::new(&mut s.calibration.step_ms).range(50.0..=10000.0).suffix(" ms"));
                ui.end_row();
                ui.label("Travel");
                ui.add(egui::DragValue::new(&mut s.calibration.distance_px).range(5.0..=100.0).suffix(" px"));
                ui.end_row();
                ui.label("Search radius");
                ui.add(egui::DragValue::new(&mut s.search_radius).range(3.0..=100.0).suffix(" px"));
                ui.end_row();
                ui.label("Multi-star");
                ui.checkbox(&mut s.multi_star, "Use all suitable stars")
                    .on_hover_text("Averages every usable star; falls back to one star when no others qualify");
                ui.end_row();

                ui.label(RichText::new("Dither & settle").strong());
                ui.end_row();
                ui.label("Dither");
                ui.horizontal(|ui| {
                    ui.add(egui::DragValue::new(&mut s.dither_px).range(0.5..=50.0).speed(0.1).suffix(" px"));
                    ui.checkbox(&mut s.dither_ra_only, "RA only");
                });
                ui.end_row();
                ui.label("Settled below");
                ui.add(egui::DragValue::new(&mut s.settle_px).range(0.1..=20.0).speed(0.05).suffix(" px"));
                ui.end_row();
                ui.label("for");
                ui.add(egui::DragValue::new(&mut s.settle_time).range(0.0..=300.0).suffix(" s"));
                ui.end_row();
                ui.label("Give up after");
                ui.add(egui::DragValue::new(&mut s.settle_timeout).range(5.0..=1800.0).suffix(" s"));
                ui.end_row();

                ui.label(RichText::new("Meridian flip").strong());
                ui.end_row();
                ui.label("");
                ui.checkbox(&mut s.flip_calibration, "Reuse calibration (flip it)");
                ui.end_row();
                ui.label("");
                ui.add_enabled(s.flip_calibration, egui::Checkbox::new(&mut s.flip_dec, "Also reverse Dec"))
                    .on_hover_text("Uncheck if your mount reverses Dec itself after a flip");
                ui.end_row();
            });
            let changed = *s != state.guider_settings;
            ui.horizontal(|ui| {
                if ui.add_enabled(changed, egui::Button::new("Apply")).clicked() {
                    actions.push(ToEngine::SetGuiderSettings(s.clone()));
                }
                if ui.add_enabled(changed, egui::Button::new("Revert")).clicked() {
                    *s = state.guider_settings.clone();
                }
            });
        });
    }
}

fn controls(ui: &mut egui::Ui, state: &UiState, actions: &mut Vec<ToEngine>) {
    let g = &state.guider;
    ui.heading("Guider");
    match (&state.roles.guide_camera, &g.camera) {
        (None, _) => {
            ui.colored_label(theme::c(ui, Color32::from_rgb(230, 160, 40)), "Assign a guide camera in Equipment.");
        }
        (Some(c), _) => {
            ui.label(RichText::new(c).strong());
        }
    }
    let state_text = match g.state {
        GuiderState::Idle => "Idle",
        GuiderState::Looping => "Looping",
        GuiderState::Calibrating => "Calibrating",
        GuiderState::Guiding => "Guiding",
    };
    ui.label(format!("{state_text} — {}", g.message));
    let idle = g.state == GuiderState::Idle;
    let busy = matches!(g.state, GuiderState::Calibrating | GuiderState::Guiding);
    ui.horizontal_wrapped(|ui| {
        if ui.add_enabled(idle, egui::Button::new("Loop")).clicked() {
            actions.push(ToEngine::GuiderLoop);
        }
        if ui.add_enabled(!busy, egui::Button::new("Auto-select star")).clicked() {
            actions.push(ToEngine::GuiderAutoSelect);
        }
        if ui.add_enabled(!busy, egui::Button::new("Calibrate")).clicked() {
            actions.push(ToEngine::GuiderCalibrate);
        }
        if ui
            .add_enabled(g.state != GuiderState::Guiding, egui::Button::new(RichText::new("Guide").strong()))
            .on_hover_text("Selects a star and calibrates first if needed")
            .clicked()
        {
            actions.push(ToEngine::GuiderGuide);
        }
        if ui
            .add_enabled(g.state == GuiderState::Guiding && !g.settling, egui::Button::new("Dither"))
            .clicked()
        {
            actions.push(ToEngine::GuiderDither);
        }
        if ui.add_enabled(!idle, egui::Button::new("Stop")).clicked() {
            actions.push(ToEngine::GuiderStop);
        }
    });
    if g.settling {
        ui.horizontal(|ui| {
            ui.add(egui::Spinner::new());
            ui.colored_label(theme::c(ui, LOCK_COLOR), "Settling…");
        });
    }
}

fn status_details(ui: &mut egui::Ui, g: &GuiderStatus, actions: &mut Vec<ToEngine>) {
    let scale = g.pixel_scale;
    let fmt = |px: f64| match scale {
        Some(s) => format!("{:.2}\" ({px:.2} px)", px * s),
        None => format!("{px:.2} px"),
    };
    egui::Grid::new("guide_status").num_columns(2).show(ui, |ui| {
        if g.stats.samples > 0 {
            ui.label("RMS RA");
            ui.label(RichText::new(fmt(g.stats.ra_rms)).color(theme::c(ui, RA_COLOR)));
            ui.end_row();
            ui.label("RMS Dec");
            ui.label(RichText::new(fmt(g.stats.dec_rms)).color(theme::c(ui, DEC_COLOR)));
            ui.end_row();
            ui.label("RMS total");
            ui.label(RichText::new(fmt(g.stats.total_rms)).strong());
            ui.end_row();
        }
        if let Some(s) = &g.star {
            ui.label("Star");
            ui.label(format!("SNR {:.0}, HFD {:.1} px{}", s.snr, s.hfd, if s.saturated { ", saturated!" } else { "" }));
            ui.end_row();
        }
        if !g.guide_stars.is_empty() {
            ui.label("Stars");
            ui.label(match (g.stars_used, g.guide_stars.len()) {
                (_, 1) => "single star".to_owned(),
                (used, n) if used <= 1 => format!("single star ({n} tracked)"),
                (used, n) => format!("{used} of {n} tracked"),
            });
            ui.end_row();
        }
        if let Some(rot) = g.field_rotation {
            ui.label("Field rotation");
            ui.label(format!("{rot:+.2}°"))
                .on_hover_text("How far the field turned since the stars were picked; grows with polar alignment error");
            ui.end_row();
        }
        ui.label("Scale");
        ui.label(scale.map_or("unknown (set focal length / pixel size)".into(), |s| format!("{s:.2}\"/px")));
        ui.end_row();
        match &g.calibration {
            Some(c) => {
                ui.vertical(|ui| {
                    ui.label("Calibration");
                    let idle = matches!(g.state, GuiderState::Idle | GuiderState::Looping);
                    if ui.add_enabled(idle, egui::Button::new("Clear").small()).clicked() {
                        actions.push(ToEngine::GuiderClearCalibration);
                    }
                });
                ui.label(format!(
                    "RA {:.0}° {:.1} px/s\nDec {:.0}° {:.1} px/s\northogonality error {:.1}°{}",
                    c.ra_angle.to_degrees(),
                    c.ra_rate * 1000.0,
                    c.dec_angle.to_degrees(),
                    c.dec_rate * 1000.0,
                    c.orthogonality_error(),
                    c.declination.map_or(String::new(), |d| format!("\nat Dec {d:.0}°")),
                ));
                ui.end_row();
            }
            None => {
                ui.label("Calibration");
                ui.label(RichText::new("none").weak());
                ui.end_row();
            }
        }
    });
}

fn graph(ui: &mut egui::Ui, state: &UiState) {
    let scale = state.guider.pixel_scale;
    let unit = if scale.is_some() { "arcsec" } else { "px" };
    let k = scale.unwrap_or(1.0);
    let steps: Vec<_> = state
        .guide_history
        .iter()
        .rev()
        .take(GRAPH_STEPS)
        .rev()
        .filter(|s| !s.star_lost)
        .collect();
    let ra: PlotPoints = steps.iter().map(|s| [s.frame as f64, s.ra_px * k]).collect();
    let dec: PlotPoints = steps.iter().map(|s| [s.frame as f64, s.dec_px * k]).collect();
    let extent = steps
        .iter()
        .map(|s| s.ra_px.abs().max(s.dec_px.abs()) * k)
        .fold(if scale.is_some() { 2.0 } else { 1.0 }, f64::max);
    let (ra_c, dec_c, zero_c) = (theme::c(ui, RA_COLOR), theme::c(ui, DEC_COLOR), theme::c(ui, Color32::GRAY));
    Plot::new("guide_graph")
        .legend(egui_plot::Legend::default())
        .include_y(-extent)
        .include_y(extent)
        .y_axis_label(unit)
        .allow_scroll(false)
        .show(ui, |plot| {
            plot.hline(HLine::new("", 0.0).color(zero_c));
            plot.line(Line::new("RA", ra).color(ra_c).width(1.5));
            // Dashed so RA and Dec stay distinguishable in the all-red night theme.
            plot.line(
                Line::new("Dec", dec)
                    .color(dec_c)
                    .width(1.5)
                    .style(egui_plot::LineStyle::dashed_dense()),
            );
        });
}

fn percent(ui: &mut egui::Ui, label: &str, v: &mut f64) {
    ui.label(label);
    let mut pct = *v * 100.0;
    ui.add(egui::DragValue::new(&mut pct).range(0.0..=100.0).suffix(" %"));
    *v = pct / 100.0;
    ui.end_row();
}

fn optional(ui: &mut egui::Ui, v: &mut Option<f64>, default: f64, suffix: &str, none_text: &str) {
    ui.horizontal(|ui| match v {
        Some(x) => {
            ui.add(egui::DragValue::new(x).range(0.1..=10000.0).speed(0.1).suffix(suffix));
            if ui.small_button("🗙").on_hover_text(none_text).clicked() {
                *v = None;
            }
        }
        None => {
            ui.label(RichText::new(none_text).weak());
            if ui.small_button("Set").clicked() {
                *v = Some(default);
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::GuideStep;
    use protocol::guider::{Calibration, GuideDir, GuideStarMark, GuideStats, Star};

    #[test]
    fn renders_guiding_state() {
        let ctx = egui::Context::default();
        let mut view = GuiderView::new();
        // A tiny grey JPEG as the guide preview.
        let img = imaging::Image { width: 32, height: 24, channels: 1, data: vec![1000; 32 * 24], cfa: None };
        let a = imaging::analyze(&img, None);
        view.on_preview(&ctx, &imaging::encode_jpeg(&imaging::make_preview(&img, &a, 32), 80));
        let mut state = UiState::default();
        state.guider = GuiderStatus {
            state: GuiderState::Guiding,
            camera: Some("Guide".into()),
            frame_size: Some((64, 48)),
            star: Some(Star { x: 30.0, y: 20.0, flux: 1e4, peak: 3000.0, snr: 80.0, hfd: 3.0, saturated: false }),
            lock: Some((30.2, 19.9)),
            guide_stars: vec![
                GuideStarMark { x: 30.0, y: 20.0, primary: true, used: true },
                GuideStarMark { x: 10.0, y: 40.0, primary: false, used: true },
                GuideStarMark { x: 50.0, y: 8.0, primary: false, used: false },
            ],
            stars_used: 2,
            field_rotation: Some(0.12),
            calibration: Some(Calibration {
                ra_angle: 0.5,
                ra_rate: 0.005,
                dec_angle: 2.07,
                dec_rate: 0.005,
                declination: Some(20.0),
                pier_side: Some("WEST".into()),
            }),
            pixel_scale: Some(3.2),
            stats: GuideStats { ra_rms: 0.2, dec_rms: 0.15, total_rms: 0.25, samples: 50 },
            settling: true,
            message: "Guiding".into(),
        };
        for i in 0..300 {
            state.guide_history.push_back(GuideStep {
                frame: i,
                time: i as f64 * 2.0,
                ra_px: (i as f64 * 0.3).sin() * 0.3,
                dec_px: 0.1,
                ra_pulse: Some((GuideDir::East, 100.0)),
                dec_pulse: None,
                snr: 80.0,
                star_lost: i % 50 == 0,
            });
        }
        let mut actions = Vec::new();
        for s in [&UiState::default(), &state] {
            for _ in 0..3 {
                let _ = ctx.run_ui(egui::RawInput::default(), |ui| view.ui(ui, s, &mut actions));
            }
        }
        assert!(actions.is_empty());
    }
}
