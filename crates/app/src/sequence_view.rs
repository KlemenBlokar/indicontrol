//! Sequence tab: step editor, presets, library, run controls and progress.

use crate::theme;
use eframe::egui::{self, Color32, RichText};
use protocol::equipment;
use protocol::sequence::plan::{self, PlanRow, PlanSettings};
use protocol::sequence::presets;
use protocol::{
    Exposure, FrameType, RunState, Sequence, SequenceDraft, SequenceOrder, SequenceStatus, SharedDoc, ToEngine,
    UiState,
};
use serde::{Deserialize, Serialize};

use crate::imaging_view::ImagingView;

const RED: Color32 = Color32::from_rgb(220, 60, 60);
const ORANGE: Color32 = Color32::from_rgb(230, 160, 40);

/// Inputs of the light planner and calibration presets (persisted with the UI).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PresetForm {
    plan_rows: Vec<PlanRow>,
    plan_settings: PlanSettings,
    /// Apply replaces the light steps (else adds to them).
    plan_replace: bool,
    dark_exposures: String,
    dark_count: u32,
    bias_count: u32,
    calib_temperature: Option<f64>,
    flat_count: u32,
}

impl Default for PresetForm {
    fn default() -> Self {
        Self {
            plan_rows: Vec::new(),
            plan_settings: PlanSettings::default(),
            plan_replace: true,
            dark_exposures: "60, 120, 300".into(),
            dark_count: 30,
            bias_count: 50,
            calib_temperature: Some(-10.0),
            flat_count: 25,
        }
    }
}

pub struct SequenceView {
    pub draft: SharedDoc<SequenceDraft>,
    /// Per UI, like a scratch pad for building steps.
    pub presets: PresetForm,
    library_pick: Option<String>,
    feedback: Option<(String, bool)>,
    plan_feedback: Option<String>,
}

impl SequenceView {
    pub fn new(presets: PresetForm) -> Self {
        Self {
            draft: SharedDoc::default(),
            presets,
            library_pick: None,
            feedback: None,
            plan_feedback: None,
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
                    self.settings(ui);
                    self.target(ui, state, mount_object);
                    ui.separator();
                    self.planner(ui, &filters);
                    ui.separator();
                    self.steps(ui, &filters);
                    ui.separator();
                    self.calibration(ui, &filters);
                });
            });
        egui::CentralPanel::default().show(ui, |ui| imaging.image_panel(ui, state));
    }

    fn run_controls(&mut self, ui: &mut egui::Ui, state: &UiState, actions: &mut Vec<ToEngine>) {
        let Some(status) = &state.sequence else {
            ui.horizontal(|ui| {
                let seq = &self.draft.value.sequence;
                let ready = seq.remaining_frames() > 0;
                let label = if seq.has_progress() { "⏵ Continue sequence" } else { "⏵ Start sequence" };
                if ui
                    .add_enabled(ready, egui::Button::new(RichText::new(label).strong()))
                    .on_disabled_hover_text(if seq.planned_frames() > 0 {
                        "Every step is complete: reset the progress to take them again"
                    } else {
                        "Add steps first"
                    })
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
                self.draft.value.sequence = picked.cloned().expect("enabled only when picked");
                let d = &mut self.draft.value;
                (d.target_ra, d.target_dec) = d
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
            let named = !self.draft.value.sequence.name.trim().is_empty();
            let exists = names.contains(&self.draft.value.sequence.name.as_str());
            if ui
                .add_enabled(named, egui::Button::new(if exists { "Save (replace)" } else { "Save" }))
                .on_disabled_hover_text("Name the sequence first")
                .clicked()
            {
                actions.push(ToEngine::SaveSequence(self.draft.value.sequence.clone()));
                self.library_pick = Some(self.draft.value.sequence.name.clone());
            }
        });
    }

    fn settings(&mut self, ui: &mut egui::Ui) {
        let seq = &mut self.draft.value.sequence;
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
    }

    fn steps(&mut self, ui: &mut egui::Ui, filters: &[String]) {
        let seq = &mut self.draft.value.sequence;
        ui.label(RichText::new("Steps").strong());
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
            .num_columns(12)
            .spacing([6.0, 4.0])
            .show(ui, |ui| {
                for h in ["", "#", "Type", "Filter", "Exposure", "Count", "Done", "Bin", "Gain", "Offset", "Temp °C", ""] {
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
                    let done = RichText::new(step.done.to_string());
                    ui.label(if step.done >= step.count { done.weak() } else { done })
                        .on_hover_text("Frames already taken; the sequence continues from here");
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
                let s = fresh(&seq.steps[i]);
                seq.steps.insert(i + 1, s);
            }
            None => {}
        }
        ui.horizontal(|ui| {
            if ui.button("+ Add step").clicked() {
                let step = seq.steps.last().map(fresh).unwrap_or_default();
                seq.steps.push(step);
            }
            if ui.add_enabled(!seq.steps.is_empty(), egui::Button::new("Clear")).clicked() {
                seq.steps.clear();
            }
            if ui
                .add_enabled(seq.has_progress(), egui::Button::new("Reset progress"))
                .on_hover_text("Forget the frames taken, to run every step again")
                .clicked()
            {
                seq.reset_progress();
            }
            ui.label(RichText::new(plan_summary(seq)).weak());
        });
    }

    fn target(&mut self, ui: &mut egui::Ui, state: &UiState, mount_object: Option<&str>) {
        let seq = &mut self.draft.value.sequence;
        ui.horizontal(|ui| {
            ui.label("Target (J2000)");
            ui.add(egui::TextEdit::singleline(&mut self.draft.value.target_ra).hint_text("RA 05:35:17").desired_width(90.0));
            ui.add(egui::TextEdit::singleline(&mut self.draft.value.target_dec).hint_text("Dec -05:23:28").desired_width(90.0));
            if let Some(m) = &state.mount
                && ui
                    .small_button("From mount")
                    .on_hover_text("Also sets the object, if the mount points at the one found in the Mount tab")
                    .clicked()
            {
                self.draft.value.target_ra = crate::mount_view::hms(m.ra_j2000);
                self.draft.value.target_dec = crate::mount_view::dms(m.dec_j2000);
                if let Some(name) = mount_object {
                    seq.object = name.to_owned();
                }
            }
            if ui.small_button("Clear").clicked() {
                self.draft.value.target_ra.clear();
                self.draft.value.target_dec.clear();
            }
        });
        let ra = indi_link::parse_number(&self.draft.value.target_ra).filter(|r| (0.0..24.0).contains(r));
        let dec = indi_link::parse_number(&self.draft.value.target_dec).filter(|d| (-90.0..=90.0).contains(d));
        seq.target = ra.zip(dec).map(|(ra_hours, dec_deg)| protocol::Target { ra_hours, dec_deg });
        let has_input = !self.draft.value.target_ra.is_empty() || !self.draft.value.target_dec.is_empty();
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

    /// Light frames as total time per filter, turned into steps on Apply.
    fn planner(&mut self, ui: &mut egui::Ui, filters: &[String]) {
        let p = &mut self.presets;
        sync_plan_rows(&mut p.plan_rows, filters.len());
        ui.label(RichText::new("Plan light frames").strong());
        ui.horizontal(|ui| {
            ui.label(RichText::new("Use:").weak());
            for (label, set) in [("LRGB", &presets::LRGB[..]), ("SHO", &presets::SHO[..])] {
                if ui
                    .small_button(label)
                    .on_hover_text(format!("Use the {label} filters only"))
                    .clicked()
                {
                    let slots: Vec<u32> = set.iter().filter_map(|(_, a)| presets::find_filter(filters, a)).collect();
                    for r in &mut p.plan_rows {
                        r.enabled = r.filter_slot.is_some_and(|s| slots.contains(&s));
                    }
                    self.plan_feedback = (slots.len() < set.len())
                        .then(|| format!("The wheel has only {} of the {label} filters.", slots.len()));
                }
            }
            if ui.small_button("All").clicked() {
                p.plan_rows.iter_mut().for_each(|r| r.enabled = true);
                self.plan_feedback = None;
            }
            if ui.small_button("None").clicked() {
                p.plan_rows.iter_mut().for_each(|r| r.enabled = false);
                self.plan_feedback = None;
            }
        });

        enum RowEdit {
            AddExposure(usize),
            Remove(usize),
        }
        let mut edit = None;
        egui::Grid::new("plan").striped(true).num_columns(6).spacing([8.0, 4.0]).show(ui, |ui| {
            for h in ["", "Filter", "Exposure", "Total time", "Frames", ""] {
                ui.label(RichText::new(h).small().strong());
            }
            ui.end_row();
            let rows = &mut p.plan_rows;
            for i in 0..rows.len() {
                let slot = rows[i].filter_slot;
                let first_of_filter = i == 0 || rows[i - 1].filter_slot != slot;
                let r = &mut rows[i];
                ui.push_id(("use", i), |ui| ui.checkbox(&mut r.enabled, ""));
                let name = match slot {
                    Some(s) => filters.get(s as usize - 1).cloned().unwrap_or(format!("#{s}")),
                    None => "(current)".into(),
                };
                ui.label(if first_of_filter { RichText::new(name) } else { RichText::new(name).weak() });
                ui.add_enabled_ui(r.enabled, |ui| {
                    ui.add(
                        egui::DragValue::new(&mut r.exposure)
                            .range(0.001..=3600.0)
                            .speed(1.0)
                            .max_decimals(3)
                            .suffix(" s"),
                    )
                });
                ui.add_enabled_ui(r.enabled, |ui| {
                    ui.add(
                        egui::DragValue::new(&mut r.minutes)
                            .range(0.0..=6000.0)
                            .speed(1.0)
                            .custom_formatter(|m, _| fmt_duration(m * 60.0))
                            .custom_parser(parse_minutes),
                    )
                    .on_hover_text("Type e.g. 90, 1:30 or 1.5 h")
                });
                let frames = r.frames();
                ui.label(if r.enabled {
                    RichText::new(format!("{frames} × {} s", r.exposure))
                } else {
                    RichText::new("")
                });
                ui.horizontal(|ui| {
                    if ui.small_button("+").on_hover_text("Another exposure with this filter").clicked() {
                        edit = Some(RowEdit::AddExposure(i));
                    }
                    if !first_of_filter && ui.small_button("🗙").clicked() {
                        edit = Some(RowEdit::Remove(i));
                    }
                });
                ui.end_row();
            }
        });
        match edit {
            Some(RowEdit::AddExposure(i)) => {
                let row = PlanRow {
                    enabled: true,
                    ..p.plan_rows[i].clone()
                };
                p.plan_rows.insert(i + 1, row);
            }
            Some(RowEdit::Remove(i)) => {
                p.plan_rows.remove(i);
            }
            None => {}
        }

        ui.horizontal(|ui| {
            let ps = &mut p.plan_settings;
            ui.label("Gain");
            ui.push_id("plan_gain", |ui| optional_value(ui, &mut ps.gain, 100.0, 0.0..=10000.0, 1.0, ""));
            ui.label("Offset");
            ui.push_id("plan_off", |ui| optional_value(ui, &mut ps.offset, 50.0, 0.0..=100000.0, 1.0, ""));
            ui.label("Bin");
            ui.push_id("plan_bin", |ui| {
                let mut v = ps.binning.map(f64::from);
                optional_value(ui, &mut v, 1.0, 1.0..=8.0, 0.05, "");
                ps.binning = v.map(|b| b.round() as u32);
            });
            ui.label("Temperature").on_hover_text("Cool the sensor to this and wait for it before the frames");
            ui.push_id("plan_temp", |ui| optional_value(ui, &mut ps.temperature, -10.0, -60.0..=40.0, 0.5, " °C"));
        });

        let frames: u32 = p.plan_rows.iter().map(PlanRow::frames).sum();
        let seconds: f64 = p.plan_rows.iter().map(|r| r.frames() as f64 * r.exposure).sum();
        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("Total {}, {frames} frames", fmt_duration(seconds))).strong());
            ui.separator();
            ui.checkbox(&mut p.plan_replace, "Replace light steps").on_hover_text(
                "On: the plan replaces the light steps, keeping the frames already taken \
                 with the same settings. Off: the plan's steps are added after the others.",
            );
            if ui.add_enabled(frames > 0, egui::Button::new("Apply")).clicked() {
                let seq = &mut self.draft.value.sequence;
                plan::apply(seq, &p.plan_rows, &p.plan_settings, p.plan_replace);
                let n = p.plan_rows.iter().filter(|r| r.frames() > 0).count();
                self.plan_feedback = Some(if p.plan_replace {
                    format!("Light steps replaced by {n} steps.")
                } else {
                    format!("Added {n} light steps.")
                });
            }
        });
        if let Some(msg) = &self.plan_feedback {
            ui.label(RichText::new(msg).weak());
        }
    }

    fn calibration(&mut self, ui: &mut egui::Ui, filters: &[String]) {
        egui::CollapsingHeader::new("Add calibration frames").show(ui, |ui| {
            let p = &mut self.presets;
            let steps = &mut self.draft.value.sequence.steps;
            let mut feedback = None;
            let all_filters: Vec<Option<u32>> = if filters.is_empty() {
                vec![None]
            } else {
                (1..=filters.len() as u32).map(Some).collect()
            };
            egui::Grid::new("presets").num_columns(2).spacing([8.0, 6.0]).show(ui, |ui| {
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

/// What is left to take (all of it, before the first frame).
fn plan_summary(seq: &Sequence) -> String {
    let frames = seq.remaining_frames();
    let auto = seq
        .steps
        .iter()
        .any(|s| s.remaining() > 0 && s.exposure.seconds().is_none());
    let time = fmt_duration(seq.remaining_seconds());
    let flats = if auto { " + flats" } else { "" };
    if seq.has_progress() {
        let planned = seq.planned_frames();
        format!("{frames} of {planned} frames left, {time} of exposure{flats}")
    } else {
        format!("{frames} frames, {time} of exposure{flats}")
    }
}

/// "45 s", "20 min", "1 h 30 min".
fn fmt_duration(secs: f64) -> String {
    // An empty sum is -0.0.
    let secs = secs.max(0.0) + 0.0;
    let mins = (secs / 60.0).round() as u64;
    if secs < 60.0 {
        format!("{secs:.0} s")
    } else if mins >= 60 {
        format!("{} h {} min", mins / 60, mins % 60)
    } else {
        format!("{mins} min")
    }
}

/// Minutes from "90", "90 min", "1:30", "1.5 h", "1 h 30 min" or "45 s".
fn parse_minutes(text: &str) -> Option<f64> {
    let t = text.trim().to_lowercase();
    if let Some((h, m)) = t.split_once(':') {
        return Some(h.trim().parse::<f64>().ok()? * 60.0 + m.trim().parse::<f64>().ok()?);
    }
    let mut total = 0.0;
    let mut rest = t.as_str();
    let mut any = false;
    while !rest.trim().is_empty() {
        rest = rest.trim_start();
        let n = rest.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(rest.len());
        let value: f64 = rest[..n].parse().ok()?;
        rest = rest[n..].trim_start();
        let u = rest.find(|c: char| !c.is_alphabetic()).unwrap_or(rest.len());
        let scale = match &rest[..u] {
            "" | "m" | "min" | "mins" | "minute" | "minutes" => 1.0,
            "h" | "hr" | "hrs" | "hour" | "hours" => 60.0,
            "s" | "sec" | "secs" | "second" | "seconds" => 1.0 / 60.0,
            _ => return None,
        };
        rest = &rest[u..];
        total += value * scale;
        any = true;
    }
    any.then_some(total)
}

/// One row per filter (or one without a wheel), keeping extra exposures.
fn sync_plan_rows(rows: &mut Vec<PlanRow>, filters: usize) {
    let slots: Vec<Option<u32>> = if filters == 0 {
        vec![None]
    } else {
        (1..=filters as u32).map(Some).collect()
    };
    rows.retain(|r| slots.contains(&r.filter_slot));
    for slot in &slots {
        if !rows.iter().any(|r| r.filter_slot == *slot) {
            rows.push(PlanRow {
                filter_slot: *slot,
                ..Default::default()
            });
        }
    }
    // Stable: extra exposures stay under their filter, in order.
    rows.sort_by_key(|r| r.filter_slot);
}

/// A copy of `step` to edit separately: new id (from the engine), no frames taken.
fn fresh(step: &protocol::SequenceStep) -> protocol::SequenceStep {
    protocol::SequenceStep {
        id: 0,
        done: 0,
        ..step.clone()
    }
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
        let mut part = lights(60.0, 30);
        part.steps[0].done = 10;
        assert_eq!(plan_summary(&part), "20 of 30 frames left, 20 min of exposure");
        assert_eq!(plan_summary(&Sequence::default()), "0 frames, 0 s of exposure");
    }

    #[test]
    fn durations() {
        assert_eq!(parse_minutes("90"), Some(90.0));
        assert_eq!(parse_minutes(" 90 min"), Some(90.0));
        assert_eq!(parse_minutes("1:30"), Some(90.0));
        assert_eq!(parse_minutes("1.5 h"), Some(90.0));
        assert_eq!(parse_minutes("1h30m"), Some(90.0));
        assert_eq!(parse_minutes("1 h 30 min"), Some(90.0));
        assert_eq!(parse_minutes("30 s"), Some(0.5));
        assert_eq!(parse_minutes("abc"), None);
        assert_eq!(parse_minutes("5 parsecs"), None);
        assert_eq!(parse_minutes(""), None);
        // What the field shows parses back to the same value.
        for m in [0.5, 45.0, 60.0, 90.0, 600.0] {
            assert_eq!(parse_minutes(&fmt_duration(m * 60.0)), Some(m));
        }
    }

    #[test]
    fn plan_rows_follow_the_wheel() {
        let mut rows = Vec::new();
        sync_plan_rows(&mut rows, 0);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].filter_slot, None);

        sync_plan_rows(&mut rows, 3);
        let slots: Vec<_> = rows.iter().map(|r| r.filter_slot).collect();
        assert_eq!(slots, vec![Some(1), Some(2), Some(3)]);

        // A second exposure for filter 1 stays with it.
        rows.push(PlanRow {
            filter_slot: Some(1),
            exposure: 10.0,
            ..Default::default()
        });
        sync_plan_rows(&mut rows, 3);
        assert_eq!(rows[1].filter_slot, Some(1));
        assert_eq!(rows[1].exposure, 10.0);

        sync_plan_rows(&mut rows, 2);
        assert_eq!(rows.len(), 3, "rows of a filter no longer in the wheel go");
    }

    #[test]
    fn renders_editor_and_status() {
        let ctx = egui::Context::default();
        let mut view = SequenceView::new(PresetForm::default());
        let draft = &mut view.draft.value;
        draft.sequence.steps = presets::darks(&[60.0, 120.0], 10, Some(-10.0));
        draft.sequence.steps.extend(presets::flats(&[Some(1), None], 5));
        draft.sequence.steps.push(presets::bias(20, None));
        let draft = draft.clone();
        let mut imaging = ImagingView::new(true);
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
