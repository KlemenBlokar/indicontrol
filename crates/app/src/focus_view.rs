//! Focus tab: focuser control, autofocus and its V-curve.

use crate::theme;
use eframe::egui::{self, Color32, RichText};
use egui_plot::{Line, MarkerShape, Plot, PlotPoints, Points, VLine};
use protocol::equipment;
use protocol::{FocusSettings, ToEngine, UiState};

const POINT_COLOR: Color32 = Color32::from_rgb(80, 140, 240);
const FIT_COLOR: Color32 = Color32::from_rgb(230, 160, 40);
const BEST_COLOR: Color32 = Color32::from_rgb(60, 200, 90);

#[derive(Default)]
pub struct FocusView {
    goto_input: String,
    settings_edit: FocusSettings,
}

impl FocusView {
    pub fn on_settings(&mut self, s: &FocusSettings) {
        self.settings_edit = s.clone();
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, state: &UiState, actions: &mut Vec<ToEngine>) {
        egui::Panel::left("focus_controls")
            .resizable(true)
            .default_size(300.0)
            .show(ui, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    self.controls(ui, state, actions);
                    ui.separator();
                    self.settings(ui, state, actions);
                });
            });
        egui::CentralPanel::default().show(ui, |ui| v_curve(ui, state));
    }

    fn controls(&mut self, ui: &mut egui::Ui, state: &UiState, actions: &mut Vec<ToEngine>) {
        ui.heading("Focuser");
        let dev = state.roles.focuser.as_ref().and_then(|f| state.cache.devices.get(f));
        let Some(dev) = dev else {
            ui.colored_label(theme::c(ui, Color32::from_rgb(230, 160, 40)),
                match &state.roles.focuser {
                    None => "Assign a focuser in Equipment.",
                    Some(_) => "The focuser is offline.",
                },
            );
            return;
        };
        let pos = dev.properties.get("ABS_FOCUS_POSITION");
        let moving = pos.is_some_and(|p| p.state == indi_link::PropertyState::Busy);
        ui.horizontal(|ui| {
            ui.label(RichText::new(&dev.name).strong());
            if moving {
                ui.add(egui::Spinner::new());
            }
        });
        if let Some(p) = pos.and_then(|p| p.number("FOCUS_ABSOLUTE_POSITION")) {
            ui.label(RichText::new(format!("Position {p:.0}")).size(18.0));
        }
        if let Some(t) = equipment::number(dev, "FOCUS_TEMPERATURE", "TEMPERATURE") {
            ui.label(format!("Temperature {:.1} °C", t.value));
        }
        let busy = state.autofocus.running;
        ui.add_enabled_ui(!busy, |ui| {
            ui.horizontal(|ui| {
                ui.label("In");
                for steps in [-1000, -100, -10] {
                    if ui.button(format!("{steps}")).clicked() {
                        actions.push(ToEngine::FocuserMove { steps });
                    }
                }
                for steps in [10, 100, 1000] {
                    if ui.button(format!("+{steps}")).clicked() {
                        actions.push(ToEngine::FocuserMove { steps });
                    }
                }
                ui.label("Out");
            });
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut self.goto_input).hint_text("position").desired_width(80.0));
                let target = self.goto_input.trim().parse::<i64>().ok();
                if ui.add_enabled(target.is_some(), egui::Button::new("Go to")).clicked() {
                    actions.push(ToEngine::FocuserGoto { position: target.expect("enabled") });
                }
                if ui.add_enabled(moving, egui::Button::new("Stop")).clicked() {
                    actions.push(ToEngine::FocuserAbort);
                }
            });
        });
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if busy {
                ui.add(egui::Spinner::new());
                if ui.button("Abort autofocus").clicked() {
                    actions.push(ToEngine::AbortAutofocus);
                }
            } else if ui
                .button(RichText::new("Autofocus").strong())
                .on_hover_text("Samples star size across focus with the main camera and moves to the best position")
                .clicked()
            {
                actions.push(ToEngine::Autofocus);
            }
        });
        if !state.autofocus.message.is_empty() {
            ui.label(&state.autofocus.message);
        }
        if let Some(stars) = state.last_frame.as_ref().and_then(|f| f.stars) {
            ui.label(RichText::new(format!("Last frame: {} stars, HFR {:.2} px", stars.count, stars.hfr)).weak());
        }
    }

    fn settings(&mut self, ui: &mut egui::Ui, state: &UiState, actions: &mut Vec<ToEngine>) {
        egui::CollapsingHeader::new("Autofocus settings").show(ui, |ui| {
            let s = &mut self.settings_edit;
            egui::Grid::new("focus_settings").num_columns(2).spacing([8.0, 4.0]).show(ui, |ui| {
                ui.label("Step size");
                ui.add(egui::DragValue::new(&mut s.step).range(1..=100_000));
                ui.end_row();
                ui.label("Samples each side");
                ui.add(egui::DragValue::new(&mut s.steps_out).range(2..=15));
                ui.end_row();
                ui.label("Exposure");
                ui.add(egui::DragValue::new(&mut s.exposure).range(0.01..=120.0).speed(0.1).suffix(" s"));
                ui.end_row();
                ui.label("Backlash overshoot");
                ui.add(egui::DragValue::new(&mut s.backlash).range(0..=100_000));
                ui.end_row();
                ui.label("Star aperture");
                ui.add(egui::DragValue::new(&mut s.aperture).range(4.0..=60.0).suffix(" px"))
                    .on_hover_text("Radius used to measure stars; increase for large defocused stars");
                ui.end_row();
                ui.label("Min stars");
                ui.add(egui::DragValue::new(&mut s.min_stars).range(1..=100));
                ui.end_row();
            });
            let changed = *s != state.focus_settings;
            ui.horizontal(|ui| {
                if ui.add_enabled(changed, egui::Button::new("Apply")).clicked() {
                    actions.push(ToEngine::SetFocusSettings(s.clone()));
                }
                if ui.add_enabled(changed, egui::Button::new("Revert")).clicked() {
                    *s = state.focus_settings.clone();
                }
            });
        });
    }
}

fn v_curve(ui: &mut egui::Ui, state: &UiState) {
    let af = &state.autofocus;
    if af.points.is_empty() {
        ui.centered_and_justified(|ui| {
            ui.label(RichText::new("Run autofocus to see the V-curve.").weak());
        });
        return;
    }
    let (point_c, fit_c, best_c) = (theme::c(ui, POINT_COLOR), theme::c(ui, FIT_COLOR), theme::c(ui, BEST_COLOR));
    let points: PlotPoints = af.points.iter().map(|&(x, y)| [x, y]).collect();
    let curve: PlotPoints = af.curve.iter().map(|&(x, y)| [x, y]).collect();
    Plot::new("v_curve")
        .x_axis_label("focuser position")
        .y_axis_label("HFR (px)")
        .include_y(0.0)
        .legend(egui_plot::Legend::default())
        .show(ui, |plot| {
            plot.points(
                Points::new("samples", points)
                    .shape(MarkerShape::Circle)
                    .radius(4.0)
                    .color(point_c),
            );
            if !af.curve.is_empty() {
                plot.line(
                    Line::new("fit", curve)
                        .color(fit_c)
                        .width(1.5)
                        .style(egui_plot::LineStyle::dashed_dense()),
                );
            }
            if let Some(best) = af.best {
                plot.vline(VLine::new("best", best).color(best_c));
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::AutofocusStatus;

    #[test]
    fn renders() {
        let ctx = egui::Context::default();
        let mut view = FocusView::default();
        let mut state = UiState::default();
        let mut actions = Vec::new();
        for _ in 0..2 {
            let _ = ctx.run_ui(egui::RawInput::default(), |ui| view.ui(ui, &state, &mut actions));
        }
        state.autofocus = AutofocusStatus {
            running: true,
            message: "5000: HFR 1.4 px".into(),
            points: vec![(4800.0, 2.5), (4900.0, 1.8), (5000.0, 1.4), (5100.0, 1.9)],
            curve: vec![(4800.0, 2.4), (5000.0, 1.4), (5100.0, 1.9)],
            best: Some(4995.0),
        };
        for _ in 0..2 {
            let _ = ctx.run_ui(egui::RawInput::default(), |ui| view.ui(ui, &state, &mut actions));
        }
        assert!(actions.is_empty());
    }
}
