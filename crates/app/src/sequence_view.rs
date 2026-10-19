//! Sequence tab: step editor, presets, library, run controls and progress.

use crate::theme;
use eframe::egui::{self, Color32, RichText};
use protocol::equipment;
use protocol::sequence::presets;
use protocol::{
    Exposure, FrameType, RunState, Sequence, SequenceOrder, SequenceStatus, ToEngine, UiState,
};
use serde::{Deserialize, Serialize};

use crate::imaging_view::ImagingView;

const RED: Color32 = Color32::from_rgb(220, 60, 60);
const ORANGE: Color32 = Color32::from_rgb(230, 160, 40);

/// Inputs of the preset builders (persisted with the UI).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PresetForm {
    light_exposure: f64,
    light_count: u32,
    bracket_filter: Option<u32>,
    bracket_exposures: String,
    bracket_count: u32,
    dark_exposures: String,
    dark_count: u32,
    bias_count: u32,
    calib_temperature: Option<f64>,
    flat_count: u32,
}

impl Default for PresetForm {
    fn default() -> Self {
        Self {
            light_exposure: 300.0,
            light_count: 20,
            bracket_filter: None,
            bracket_exposures: "10, 30, 60, 120, 300".into(),
            bracket_count: 3,
            dark_exposures: "60, 120, 300".into(),
            dark_count: 30,
            bias_count: 50,
            calib_temperature: Some(-10.0),
            flat_count: 25,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SequenceDraft {
    pub sequence: Sequence,
    pub presets: PresetForm,
}

pub struct SequenceView {
    pub draft: SequenceDraft,
    library_pick: Option<String>,
    feedback: Option<(String, bool)>,
    target_ra: String,
    target_dec: String,
}

impl SequenceView {
    pub fn new(draft: SequenceDraft) -> Self {
        let (target_ra, target_dec) = draft
            .sequence
            .target
            .map(|t| (crate::mount_view::hms(t.ra_hours), crate::mount_view::dms(t.dec_deg)))
            .unwrap_or_default();
        Self {
            draft,
            library_pick: None,
            feedback: None,
            target_ra,
            target_dec,
        }
    }

    /// `mount_object`: the object found in the Mount tab, if the mount points at it.
    pub fn ui(
        &mut self,
        ui: &mut egui::Ui,
        state: &UiState,
        mount_object: Option<&str>,
        imaging: &mut ImagingView,
        actions: &mut Vec<ToEngine>,
    ) {
        let filters = state
            .roles
            .filter_wheel
            .as_ref()
            .and_then(|w| state.cache.devices.get(w))
            .map(equipment::filter_names)
            .unwrap_or_default();
        egui::Panel::left("sequence_editor")
            .resizable(true)
            .default_size(720.0)
            .show(ui, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    self.run_controls(ui, state, actions);
                    ui.separator();
                    self.library(ui, state, actions);
                    ui.separator();
                    self.editor(ui, &filters);
                    self.target(ui, state, mount_object);
                    ui.separator();
                    self.presets(ui, &filters);
                });
            });
        egui::CentralPanel::default().show(ui, |ui| imaging.image_panel(ui, state));
    }

    fn run_controls(&mut self, ui: &mut egui::Ui, state: &UiState, actions: &mut Vec<ToEngine>) {
        let Some(status) = &state.sequence else {
            ui.horizontal(|ui| {
                let seq = &self.draft.sequence;
                let ready = seq.planned_frames() > 0;
                if ui
                    .add_enabled(ready, egui::Button::new(RichText::new("⏵ Start sequence").strong()))
                    .clicked()
                {
                    actions.push(ToEngine::StartSequence(seq.clone()));
                }
                ui.label(plan_summary(seq));
            });
            return;
        };
        ui.horizontal(|ui| {
            match status.state {
                RunState::Running => {
                    if ui.button("⏸ Pause").on_hover_text("Pause after the current frame").clicked() {
                        actions.push(ToEngine::PauseSequence);
                    }
                }
                RunState::Pausing | RunState::Paused => {
                    if ui.button("⏵ Resume").clicked() {
                        actions.push(ToEngine::ResumeSequence);
                    }
                }
            }
            if ui.button(RichText::new("■ Abort").color(theme::c(ui, RED))).clicked() {
                actions.push(ToEngine::AbortSequence);
            }
            let name = if status.sequence.name.is_empty() { "Sequence" } else { &status.sequence.name };
            let state_text = match status.state {
                RunState::Running => "running",
                RunState::Pausing => "pausing after this frame",
                RunState::Paused => "paused",
            };
            ui.label(RichText::new(format!("{name} — {state_text}")).strong());
        });
        let planned = status.sequence.planned_frames().max(1);
        ui.add(
            egui::ProgressBar::new(status.frames_done() as f32 / planned as f32)
                .text(format!("{} / {} frames", status.frames_done(), planned)),
        );
        ui.label(&status.message);
        progress_table(ui, status);
    }

    fn library(&mut self, ui: &mut egui::Ui, state: &UiState, actions: &mut Vec<ToEngine>) {
        ui.horizontal(|ui| {
            ui.label("Saved:");
            let names: Vec<&str> = state.library.iter().map(|s| s.name.as_str()).collect();
            if self.library_pick.as_ref().is_some_and(|p| !names.contains(&p.as_str())) {
                self.library_pick = None;
            }
            egui::ComboBox::from_id_salt("library")
                .selected_text(self.library_pick.as_deref().unwrap_or("—"))
                .width(180.0)
                .show_ui(ui, |ui| {
                    for n in &names {
                        ui.selectable_value(&mut self.library_pick, Some(n.to_string()), *n);
                    }
                });
            let picked = self
                .library_pick
                .as_ref()
                .and_then(|p| state.library.iter().find(|s| &s.name == p));
            if ui.add_enabled(picked.is_some(), egui::Button::new("Load")).clicked() {
                self.draft.sequence = picked.cloned().expect("enabled only when picked");
                (self.target_ra, self.target_dec) = self
                    .draft
                    .sequence
                    .target
                    .map(|t| (crate::mount_view::hms(t.ra_hours), crate::mount_view::dms(t.dec_deg)))
                    .unwrap_or_default();
            }
            if ui.add_enabled(picked.is_some(), egui::Button::new("Delete")).clicked() {
                actions.push(ToEngine::DeleteSequence {
                    name: picked.expect("picked").name.clone(),
                });
            }
            ui.separator();
            let named = !self.draft.sequence.name.trim().is_empty();
            let exists = names.contains(&self.draft.sequence.name.as_str());
            if ui
                .add_enabled(named, egui::Button::new(if exists { "Save (replace)" } else { "Save" }))
                .on_disabled_hover_text("Name the sequence first")
                .clicked()
            {
                actions.push(ToEngine::SaveSequence(self.draft.sequence.clone()));
                self.library_pick = Some(self.draft.sequence.name.clone());
            }
        });
    }

    fn editor(&mut self, ui: &mut egui::Ui, filters: &[String]) {
        let seq = &mut self.draft.sequence;
        ui.horizontal(|ui| {
            ui.label("Name");
            ui.add(egui::TextEdit::singleline(&mut seq.name).desired_width(160.0));
            ui.label("Object");
            ui.add(egui::TextEdit::singleline(&mut seq.object).hint_text("e.g. M 31").desired_width(160.0));
        });
        ui.horizontal(|ui| {
            ui.label("Dither every");
            ui.add(egui::DragValue::new(&mut seq.dither_every).range(0..=100));
            ui.label(if seq.dither_every == 0 { "(off)" } else { "light frames, when guiding" });
            ui.separator();
            ui.checkbox(&mut seq.meridian_flip, "Meridian flip");
            ui.add_enabled(
                seq.meridian_flip,
                egui::DragValue::new(&mut seq.flip_minutes_past).range(0.0..=60.0).suffix(" min past"),
            );
        });
        ui.horizontal(|ui| {
            let af = &mut seq.autofocus;
            ui.label("Autofocus:");
            ui.checkbox(&mut af.on_start, "at start");
            ui.checkbox(&mut af.on_filter_change, "on filter change");
            ui.checkbox(&mut af.after_flip, "after flip");
            ui.label("every");
            ui.add(egui::DragValue::new(&mut af.every_frames).range(0..=500));
            ui.label("lights");
            let mut temp = af.temperature_delta.is_some();
            if ui.checkbox(&mut temp, "on ΔT").changed() {
                af.temperature_delta = temp.then_some(1.0);
            }
            if let Some(d) = &mut af.temperature_delta {
                ui.add(egui::DragValue::new(d).range(0.1..=10.0).speed(0.1).suffix(" °C"));
            }
        });
        ui.horizontal(|ui| {
            ui.label("Order");
            ui.selectable_value(&mut seq.order, SequenceOrder::ByStep, "Step by step")
                .on_hover_text("All frames of a step, then the next step");
            ui.selectable_value(&mut seq.order, SequenceOrder::Interleaved, "Interleaved")
                .on_hover_text("One frame of each step in turn (LRGBLRGB…)");
        });

        enum Edit {
            Up(usize),
            Down(usize),
            Remove(usize),
            Duplicate(usize),
        }
        let mut edit = None;
        let n = seq.steps.len();
        egui::Grid::new("steps")
            .striped(true)
            .num_columns(11)
            .spacing([6.0, 4.0])
            .show(ui, |ui| {
                for h in ["", "#", "Type", "Filter", "Exposure", "Count", "Bin", "Gain", "Offset", "Temp °C", ""] {
                    ui.label(RichText::new(h).small().strong());
                }
                ui.end_row();
                for (i, step) in seq.steps.iter_mut().enumerate() {
                    ui.push_id(i, |ui| ui.checkbox(&mut step.enabled, ""));
                    ui.label(format!("{}", i + 1));
                    ui.push_id(("type", i), |ui| {
                        egui::ComboBox::from_id_salt("t")
                            .selected_text(step.frame_type.label())
                            .width(60.0)
                            .show_ui(ui, |ui| {
                                for t in FrameType::ALL {
                                    ui.selectable_value(&mut step.frame_type, t, t.label());
                                }
                            });
                    });
                    ui.push_id(("filter", i), |ui| filter_combo(ui, &mut step.filter_slot, filters));
                    ui.push_id(("exp", i), |ui| exposure_editor(ui, &mut step.exposure));
                    ui.add(egui::DragValue::new(&mut step.count).range(0..=9999));
                    ui.push_id(("bin", i), |ui| {
                        let mut v = step.binning.map(f64::from);
                        optional_value(ui, &mut v, 1.0, 1.0..=8.0, 0.05, "");
                        step.binning = v.map(|b| b.round() as u32);
                    });
                    ui.push_id(("gain", i), |ui| optional_value(ui, &mut step.gain, 100.0, 0.0..=10000.0, 1.0, ""));
                    ui.push_id(("off", i), |ui| optional_value(ui, &mut step.offset, 50.0, 0.0..=100000.0, 1.0, ""));
                    ui.push_id(("temp", i), |ui| {
                        optional_value(ui, &mut step.temperature, -10.0, -60.0..=40.0, 0.5, "")
                    });
                    ui.horizontal(|ui| {
                        if ui.add_enabled(i > 0, egui::Button::new("⏶").small()).clicked() {
                            edit = Some(Edit::Up(i));
                        }
                        if ui.add_enabled(i + 1 < n, egui::Button::new("⏷").small()).clicked() {
                            edit = Some(Edit::Down(i));
                        }
                        if ui.small_button("🗐").on_hover_text("Duplicate").clicked() {
                            edit = Some(Edit::Duplicate(i));
                        }
                        if ui.small_button("🗙").on_hover_text("Remove").clicked() {
                            edit = Some(Edit::Remove(i));
                        }
                    });
                    ui.end_row();
                }
            });
        match edit {
            Some(Edit::Up(i)) => seq.steps.swap(i, i - 1),
            Some(Edit::Down(i)) => seq.steps.swap(i, i + 1),
            Some(Edit::Remove(i)) => {
                seq.steps.remove(i);
            }
            Some(Edit::Duplicate(i)) => {
                let s = seq.steps[i].clone();
                seq.steps.insert(i + 1, s);
            }
            None => {}
        }
        ui.horizontal(|ui| {
            if ui.button("+ Add step").clicked() {
                let step = seq.steps.last().cloned().unwrap_or_default();
                seq.steps.push(step);
            }
            if ui.add_enabled(!seq.steps.is_empty(), egui::Button::new("Clear")).clicked() {
                seq.steps.clear();
            }
            ui.label(RichText::new(plan_summary(seq)).weak());
        });
    }

    fn target(&mut self, ui: &mut egui::Ui, state: &UiState, mount_object: Option<&str>) {
        let seq = &mut self.draft.sequence;
        ui.horizontal(|ui| {
            ui.label("Target (J2000)");
            ui.add(egui::TextEdit::singleline(&mut self.target_ra).hint_text("RA 05:35:17").desired_width(90.0));
            ui.add(egui::TextEdit::singleline(&mut self.target_dec).hint_text("Dec -05:23:28").desired_width(90.0));
            if let Some(m) = &state.mount
                && ui
                    .small_button("From mount")
                    .on_hover_text("Also sets the object, if the mount points at the one found in the Mount tab")
                    .clicked()
            {
                self.target_ra = crate::mount_view::hms(m.ra_j2000);
                self.target_dec = crate::mount_view::dms(m.dec_j2000);
                if let Some(name) = mount_object {
                    seq.object = name.to_owned();
                }
            }
            if ui.small_button("Clear").clicked() {
                self.target_ra.clear();
                self.target_dec.clear();
            }
        });
        let ra = indi_link::parse_number(&self.target_ra).filter(|r| (0.0..24.0).contains(r));
        let dec = indi_link::parse_number(&self.target_dec).filter(|d| (-90.0..=90.0).contains(d));
        seq.target = ra.zip(dec).map(|(ra_hours, dec_deg)| protocol::Target { ra_hours, dec_deg });
        let has_input = !self.target_ra.is_empty() || !self.target_dec.is_empty();
        if has_input && seq.target.is_none() {
            ui.colored_label(theme::c(ui, ORANGE), "Enter RA (h:m:s) and Dec (d:m:s) to use a target.");
        }
        ui.add_enabled_ui(seq.target.is_some(), |ui| {
            ui.horizontal(|ui| {
                ui.checkbox(&mut seq.center_on_start, "Slew & centre at start");
                ui.checkbox(&mut seq.center_after_flip, "Re-centre after meridian flip");
            });
        });
    }

    fn presets(&mut self, ui: &mut egui::Ui, filters: &[String]) {
        egui::CollapsingHeader::new("Add from preset").default_open(true).show(ui, |ui| {
            let p = &mut self.draft.presets;
            let steps = &mut self.draft.sequence.steps;
            let mut feedback = None;
            let all_filters: Vec<Option<u32>> = if filters.is_empty() {
                vec![None]
            } else {
                (1..=filters.len() as u32).map(Some).collect()
            };
            egui::Grid::new("presets").num_columns(2).spacing([8.0, 6.0]).show(ui, |ui| {
                ui.label(RichText::new("Lights").strong());
                ui.horizontal(|ui| {
                    ui.add(egui::DragValue::new(&mut p.light_exposure).range(0.001..=7200.0).suffix(" s"));
                    ui.label("×");
                    ui.add(egui::DragValue::new(&mut p.light_count).range(1..=9999));
                    for (label, set) in [("LRGB", &presets::LRGB[..]), ("SHO", &presets::SHO[..])] {
                        if ui.button(label).clicked() {
                            let (new, missing) = presets::filter_set(filters, set, p.light_exposure, p.light_count);
                            feedback = Some(added(label, new.len(), &missing));
                            steps.extend(new);
                        }
                    }
                });
                ui.end_row();

                ui.label(RichText::new("Bracketing").strong());
                ui.horizontal(|ui| {
                    filter_combo(ui, &mut p.bracket_filter, filters);
                    ui.add(egui::TextEdit::singleline(&mut p.bracket_exposures).desired_width(140.0))
                        .on_hover_text("Exposure times in seconds, e.g. 10, 30, 60");
                    ui.label("×");
                    ui.add(egui::DragValue::new(&mut p.bracket_count).range(1..=999));
                    if ui.button("Add").clicked() {
                        feedback = Some(match presets::parse_exposures(&p.bracket_exposures) {
                            Ok(e) => {
                                steps.extend(presets::bracketing(p.bracket_filter, &e, p.bracket_count));
                                (format!("Added {} bracketing steps.", e.len()), false)
                            }
                            Err(e) => (e, true),
                        });
                    }
                });
                ui.end_row();

                ui.label(RichText::new("Calibration").strong());
                ui.horizontal(|ui| {
                    ui.label("Sensor at");
                    optional_value(ui, &mut p.calib_temperature, -10.0, -60.0..=40.0, 0.5, " °C");
                    ui.label(RichText::new("(darks & bias)").weak());
                });
                ui.end_row();

                ui.label("Darks");
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut p.dark_exposures).desired_width(140.0))
                        .on_hover_text("Exposure times in seconds");
                    ui.label("×");
                    ui.add(egui::DragValue::new(&mut p.dark_count).range(1..=999));
                    if ui.button("Add").clicked() {
                        feedback = Some(match presets::parse_exposures(&p.dark_exposures) {
                            Ok(e) => {
                                steps.extend(presets::darks(&e, p.dark_count, p.calib_temperature));
                                (format!("Added {} dark steps.", e.len()), false)
                            }
                            Err(e) => (e, true),
                        });
                    }
                });
                ui.end_row();

                ui.label("Bias");
                ui.horizontal(|ui| {
                    ui.add(egui::DragValue::new(&mut p.bias_count).range(1..=999));
                    if ui.button("Add").clicked() {
                        steps.push(presets::bias(p.bias_count, p.calib_temperature));
                        feedback = Some(("Added a bias step.".into(), false));
                    }
                });
                ui.end_row();

                ui.label("Flats");
                ui.horizontal(|ui| {
                    ui.add(egui::DragValue::new(&mut p.flat_count).range(1..=999));
                    ui.label("per filter");
                    if ui.button("Add flats").on_hover_text("Auto exposure to ~45 % of full scale").clicked() {
                        steps.extend(presets::flats(&all_filters, p.flat_count));
                        feedback = Some((format!("Added {} flat steps.", all_filters.len()), false));
                    }
                    if ui.button("Add dark flats").on_hover_text("Same exposure as the matching flats").clicked() {
                        steps.extend(presets::dark_flats(&all_filters, p.flat_count));
                        feedback = Some((format!("Added {} dark-flat steps.", all_filters.len()), false));
                    }
                });
                ui.end_row();
            });
            if feedback.is_some() {
                self.feedback = feedback;
            }
            if let Some((msg, error)) = &self.feedback {
                ui.colored_label(theme::c(ui, if *error { ORANGE } else { ui.visuals().weak_text_color() }), msg);
            }
        });
    }
}

fn added(label: &str, n: usize, missing: &[&str]) -> (String, bool) {
    if missing.is_empty() {
        (format!("Added {n} {label} steps."), false)
    } else {
        (
            format!(
                "Added {n} {label} steps; no filter named {} in the wheel.",
                missing.join(", ")
            ),
            true,
        )
    }
}

fn plan_summary(seq: &Sequence) -> String {
    let frames = seq.planned_frames();
    let secs = seq.planned_seconds();
    let auto = seq
        .steps
        .iter()
        .any(|s| s.enabled && s.exposure.seconds().is_none());
    let total_mins = (secs / 60.0).round() as u64;
    let time = if secs < 60.0 {
        format!("{secs:.0} s")
    } else if total_mins >= 60 {
        format!("{} h {} min", total_mins / 60, total_mins % 60)
    } else {
        format!("{total_mins} min")
    };
    format!(
        "{frames} frames, {time} of exposure{}",
        if auto { " + flats" } else { "" }
    )
}

fn progress_table(ui: &mut egui::Ui, status: &SequenceStatus) {
    egui::Grid::new("seq_progress").striped(true).num_columns(4).show(ui, |ui| {
        for (i, step) in status.sequence.steps.iter().enumerate().filter(|(_, s)| s.enabled) {
            let current = status.current_step == Some(i);
            let marker = if current { "⏵" } else { "" };
            ui.label(marker);
            ui.label(format!("{} {}", i + 1, step.frame_type.label()));
            ui.label(exposure_text(&step.exposure));
            let done = status.done.get(i).copied().unwrap_or(0);
            let text = format!("{done} / {}", step.count);
            ui.label(if done >= step.count { RichText::new(text).weak() } else { RichText::new(text) });
            ui.end_row();
        }
    });
}

fn exposure_text(e: &Exposure) -> String {
    match e {
        Exposure::Fixed { seconds } => format!("{seconds} s"),
        Exposure::AutoFlat { target, .. } => format!("auto ({:.0} %)", target * 100.0),
        Exposure::MatchFlat => "= flats".into(),
        Exposure::Minimum => "minimum".into(),
    }
}

fn filter_combo(ui: &mut egui::Ui, slot: &mut Option<u32>, filters: &[String]) {
    let text = match slot {
        Some(s) => filters
            .get((*s as usize).saturating_sub(1))
            .cloned()
            .unwrap_or(format!("#{s}")),
        None => "(current)".into(),
    };
    egui::ComboBox::from_id_salt("filter")
        .selected_text(text)
        .width(80.0)
        .show_ui(ui, |ui| {
            ui.selectable_value(slot, None, "(current)");
            for (i, n) in filters.iter().enumerate() {
                ui.selectable_value(slot, Some(i as u32 + 1), n);
            }
        });
}

fn exposure_editor(ui: &mut egui::Ui, e: &mut Exposure) {
    ui.horizontal(|ui| {
        let mode = match e {
            Exposure::Fixed { .. } => "Fixed",
            Exposure::AutoFlat { .. } => "Auto flat",
            Exposure::MatchFlat => "Match flats",
            Exposure::Minimum => "Minimum",
        };
        egui::ComboBox::from_id_salt("mode")
            .selected_text(mode)
            .width(80.0)
            .show_ui(ui, |ui| {
                let seconds = e.seconds().filter(|s| *s > 0.0).unwrap_or(60.0);
                if ui.selectable_label(mode == "Fixed", "Fixed").clicked() {
                    *e = Exposure::Fixed { seconds };
                }
                if ui.selectable_label(mode == "Auto flat", "Auto flat").clicked() {
                    *e = Exposure::DEFAULT_FLAT;
                }
                if ui.selectable_label(mode == "Match flats", "Match flats").clicked() {
                    *e = Exposure::MatchFlat;
                }
                if ui.selectable_label(mode == "Minimum", "Minimum").clicked() {
                    *e = Exposure::Minimum;
                }
            });
        match e {
            Exposure::Fixed { seconds } => {
                ui.add(
                    egui::DragValue::new(seconds)
                        .range(0.0..=7200.0)
                        .speed(0.5)
                        .suffix(" s")
                        .max_decimals(3),
                );
            }
            Exposure::AutoFlat { target, .. } => {
                let mut pct = *target * 100.0;
                ui.add(egui::DragValue::new(&mut pct).range(10.0..=80.0).suffix(" %"))
                    .on_hover_text("Target median, as a fraction of full scale");
                *target = pct / 100.0;
            }
            Exposure::MatchFlat | Exposure::Minimum => {}
        }
    });
}

/// "—" (unset, keep the camera's value) or an editable number with a clear button.
fn optional_value(
    ui: &mut egui::Ui,
    value: &mut Option<f64>,
    default: f64,
    range: std::ops::RangeInclusive<f64>,
    speed: f64,
    suffix: &str,
) {
    ui.horizontal(|ui| match value {
        Some(v) => {
            ui.add(egui::DragValue::new(v).range(range).speed(speed).suffix(suffix));
            if ui.small_button("🗙").on_hover_text("Leave unchanged").clicked() {
                *value = None;
            }
        }
        None => {
            if ui.small_button("—").on_hover_text("Unchanged — click to set").clicked() {
                *value = Some(default);
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::SequenceStep;

    #[test]
    fn plan_summary_text() {
        let seq = Sequence {
            steps: vec![
                SequenceStep {
                    exposure: Exposure::Fixed { seconds: 300.0 },
                    count: 20,
                    ..Default::default()
                },
                presets::flats(&[Some(1)], 10).remove(0),
            ],
            ..Default::default()
        };
        assert_eq!(plan_summary(&seq), "30 frames, 1 h 40 min of exposure + flats");
        let lights = |seconds, count| Sequence {
            steps: vec![SequenceStep {
                exposure: Exposure::Fixed { seconds },
                count,
                ..Default::default()
            }],
            ..Default::default()
        };
        assert_eq!(plan_summary(&lights(2.0, 3)), "3 frames, 6 s of exposure");
        assert_eq!(plan_summary(&lights(177.0, 20)), "20 frames, 59 min of exposure");
        assert_eq!(plan_summary(&lights(179.0, 20)), "20 frames, 1 h 0 min of exposure");
        assert_eq!(plan_summary(&lights(359.0, 20)), "20 frames, 2 h 0 min of exposure");
    }

    #[test]
    fn renders_editor_and_status() {
        let ctx = egui::Context::default();
        let mut draft = SequenceDraft::default();
        draft.sequence.steps = presets::darks(&[60.0, 120.0], 10, Some(-10.0));
        draft.sequence.steps.extend(presets::flats(&[Some(1), None], 5));
        draft.sequence.steps.push(presets::bias(20, None));
        let mut view = SequenceView::new(draft.clone());
        let mut imaging = ImagingView::new(Default::default());
        let idle = UiState::default();
        let running = UiState {
            sequence: Some(SequenceStatus {
                sequence: draft.sequence.clone(),
                state: RunState::Running,
                current_step: Some(1),
                done: vec![10, 3, 0, 0, 0],
                message: "Step 2: frame 4 of 10".into(),
            }),
            ..UiState::default()
        };
        for state in [&idle, &running] {
            let mut actions = Vec::new();
            for _ in 0..3 {
                let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
                    view.ui(ui, state, None, &mut imaging, &mut actions)
                });
            }
            assert!(actions.is_empty());
        }
    }
}
