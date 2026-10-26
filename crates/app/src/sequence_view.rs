//! Sequence tab: light plan, calibration masters, library, run controls and progress.

use crate::theme;
use eframe::egui::{self, Color32, RichText};
use protocol::equipment;
use protocol::sequence::lights::{self, Row};
use protocol::sequence::presets;
use protocol::{
    Exposure, FlatCalibration, FrameType, MasterFrame, MasterKind, Need, Purpose, RunState, Sequence, SequenceDraft, SequenceOrder,
    SequenceStatus, Setup, SharedDoc, ToEngine, UiState,
};
use protocol::masters::MasterCounts;
use serde::{Deserialize, Serialize};

use crate::imaging_view::ImagingView;

const RED: Color32 = Color32::from_rgb(220, 60, 60);
const ORANGE: Color32 = Color32::from_rgb(230, 160, 40);
const GREEN: Color32 = Color32::from_rgb(60, 180, 75);
const YELLOW: Color32 = Color32::from_rgb(220, 200, 60);

/// How many frames each master is made of (kept per UI).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PresetForm {
    dark_count: u32,
    bias_count: u32,
    /// Flats and dark flats.
    flat_count: u32,
}

impl Default for PresetForm {
    fn default() -> Self {
        Self {
            dark_count: 30,
            bias_count: 50,
            flat_count: 25,
        }
    }
}

pub struct SequenceView {
    pub draft: SharedDoc<SequenceDraft>,
    /// Per UI, like a scratch pad for building steps.
    pub presets: PresetForm,
    library_pick: Option<String>,
    plan_feedback: Option<String>,
    /// Commands that run the engine's draft: sent after this frame's edits
    /// of it, so the engine has them (and has given new steps ids).
    pub after_edits: Vec<ToEngine>,
}

impl SequenceView {
    pub fn new(presets: PresetForm) -> Self {
        Self {
            draft: SharedDoc::default(),
            presets,
            library_pick: None,
            plan_feedback: None,
            after_edits: Vec::new(),
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
                let warnings = self.warnings(state);
                if !warnings.is_empty() {
                    egui::Panel::bottom("sequence_warnings").show(ui, |ui| {
                        for w in &warnings {
                            ui.colored_label(theme::c(ui, ORANGE), format!("• {w}"));
                        }
                    });
                }
                egui::ScrollArea::vertical().show(ui, |ui| {
                    self.run_controls(ui, state, actions);
                    ui.separator();
                    self.library(ui, state, actions);
                    ui.separator();
                    self.settings(ui);
                    self.target(ui, state, mount_object);
                    ui.separator();
                    self.lights(ui, &filters);
                    ui.separator();
                    self.masters(ui, state, actions);
                });
            });
        egui::CentralPanel::default().show(ui, |ui| imaging.image_panel(ui, state));
    }

    fn run_controls(&mut self, ui: &mut egui::Ui, state: &UiState, actions: &mut Vec<ToEngine>) {
        let Some(status) = &state.sequence else {
            ui.horizontal_wrapped(|ui| {
                for setup in [Setup::Covered, Setup::Panel, Setup::Sky] {
                    self.run_button(ui, state, setup);
                }
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

    /// The draft with the steps for `setup` worked out again from the
    /// masters still missing (lights are left as they are).
    fn prepared(&self, state: &UiState, setup: Setup) -> Sequence {
        let mut seq = self.draft.value.sequence.clone();
        if setup != Setup::Sky {
            let needs = protocol::masters::needs(&seq, state);
            seq.put_calibration(setup, protocol::masters::missing_steps(&needs, setup, self.counts()));
        }
        seq
    }

    fn counts(&self) -> MasterCounts {
        let p = &self.presets;
        MasterCounts {
            dark: p.dark_count,
            bias: p.bias_count,
            flat: p.flat_count,
        }
    }

    /// Runs the steps for `setup`, the missing masters' first.
    fn run_button(&mut self, ui: &mut egui::Ui, state: &UiState, setup: Setup) {
        let seq = self.prepared(state, setup);
        let run = seq.only(setup);
        let what = match setup {
            Setup::Sky => "lights",
            Setup::Covered => "covered frames",
            Setup::Panel => "flats",
        };
        let verb = match (setup, run.has_progress()) {
            (_, true) => "Continue",
            (Setup::Sky, false) => "Start",
            _ => "Take",
        };
        let left = run.remaining_frames();
        let label = if left == 0 {
            format!("⏵ {verb} {what}")
        } else if setup == Setup::Panel {
            format!("⏵ {verb} {what} ({left})")
        } else {
            format!("⏵ {verb} {what} ~{}", fmt_duration(run.remaining_seconds()))
        };
        let hover = match setup {
            Setup::Sky => "The light steps; they get the masters listed under Calibration",
            Setup::Covered => "Darks, bias and dark flats the lights are missing, and any added by hand. Cover the telescope.",
            Setup::Panel => "Flats the lights are missing, and any added by hand. Turn on the flat panel.",
        };
        let text = if setup == Setup::Sky { RichText::new(label).strong() } else { RichText::new(label) };
        if ui
            .add_enabled(left > 0, egui::Button::new(text))
            .on_hover_text(format!("{hover}\n{}", plan_summary(&run)))
            .on_disabled_hover_text(match setup {
                Setup::Sky if run.planned_frames() > 0 => "Every light step is complete: reset the progress to take them again",
                Setup::Sky => "Plan the light frames first",
                _ => "Nothing missing",
            })
            .clicked()
        {
            self.draft.value.sequence = seq;
            self.after_edits.push(ToEngine::StartDraft { setup });
        }
    }

    /// Why the lights will not be fully calibrated, and masters that could
    /// not be checked. Shown below the editor.
    fn warnings(&self, state: &UiState) -> Vec<String> {
        let seq = &self.draft.value.sequence;
        let mut out = Vec::new();
        for n in protocol::masters::needs(seq, state) {
            let name = need_name(&n).to_lowercase();
            match (&n.found, n.purpose) {
                (None, _) if n.spec.is_none() => {
                    out.push(format!("{} for the flats waits for them: it needs the {}", capitalised(&name), n.what))
                }
                (None, Purpose::Lights) => {
                    out.push(format!("No master {name} for {}: the lights go without it until it is taken", n.what))
                }
                (None, Purpose::Flats) if n.kind == MasterKind::Flat => {
                    out.push(format!("No flat for {}: those lights are not flat-fielded until it is taken", n.what))
                }
                (None, Purpose::Flats) => out.push(format!("No master {name} {} to calibrate the flat", n.what)),
                (Some(m), _) => {
                    let unknown = n.unverified();
                    if !unknown.is_empty() {
                        out.push(format!("{}: no {} in its header, so it could not be checked", m.path, unknown.join(" or ")));
                    }
                }
            }
        }
        out
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

    /// The light steps as a plan: per filter an exposure and a total time,
    /// with the camera settings shared by all of them.
    fn lights(&mut self, ui: &mut egui::Ui, filters: &[String]) {
        let seq = &mut self.draft.value.sequence;
        ui.label(RichText::new("Lights").strong());
        let rows = lights::rows(seq, filters.len());
        let slot_of = |seq: &Sequence, r: Row| match r {
            Row::Step(i) => seq.steps[i].filter_slot,
            Row::Unused(slot) => slot,
        };
        // Steps first: adding one for an unused filter moves the others.
        let tick = |seq: &mut Sequence, on: &dyn Fn(Option<u32>) -> bool| {
            for &r in rows.iter().filter(|r| matches!(r, Row::Step(_))) {
                lights::set_enabled(seq, r, on(slot_of(seq, r)));
            }
            for &r in rows.iter().filter(|r| matches!(r, Row::Unused(_))) {
                lights::set_enabled(seq, r, on(slot_of(seq, r)));
            }
        };
        ui.horizontal(|ui| {
            ui.label(RichText::new("Use:").weak());
            for (label, set) in [("LRGB", &presets::LRGB[..]), ("SHO", &presets::SHO[..])] {
                if ui.small_button(label).on_hover_text(format!("Use the {label} filters only")).clicked() {
                    let slots: Vec<u32> = set.iter().filter_map(|(_, a)| presets::find_filter(filters, a)).collect();
                    tick(seq, &|slot| slot.is_some_and(|s| slots.contains(&s)));
                    self.plan_feedback = (slots.len() < set.len())
                        .then(|| format!("The wheel has only {} of the {label} filters.", slots.len()));
                }
            }
            if ui.small_button("All").clicked() {
                tick(seq, &|_| true);
                self.plan_feedback = None;
            }
            if ui.small_button("None").clicked() {
                tick(seq, &|_| false);
                self.plan_feedback = None;
            }
        });

        enum Edit {
            Enable(Row, bool),
            Exposure(usize, f64),
            Minutes(usize, f64),
            AddExposure(usize),
            Remove(usize),
        }
        let mut edit = None;
        egui::Grid::new("lights").striped(true).num_columns(7).spacing([8.0, 4.0]).show(ui, |ui| {
            for h in ["", "Filter", "Exposure", "Total time", "Frames", "Done", ""] {
                ui.label(RichText::new(h).small().strong());
            }
            ui.end_row();
            for (k, &row) in rows.iter().enumerate() {
                let slot = slot_of(seq, row);
                let first_of_filter = k == 0 || slot_of(seq, rows[k - 1]) != slot;
                let name = match slot {
                    Some(s) => filters.get(s as usize - 1).cloned().unwrap_or(format!("#{s}")),
                    None => "(current)".into(),
                };
                let Row::Step(i) = row else {
                    let mut on = false;
                    if ui.push_id(("use", k), |ui| ui.checkbox(&mut on, "")).inner.changed() {
                        edit = Some(Edit::Enable(row, true));
                    }
                    ui.label(RichText::new(name).weak());
                    ui.end_row();
                    continue;
                };
                let s = &seq.steps[i];
                let mut on = s.enabled;
                if ui.push_id(("use", k), |ui| ui.checkbox(&mut on, "")).inner.changed() {
                    edit = Some(Edit::Enable(row, on));
                }
                ui.label(if first_of_filter { RichText::new(name) } else { RichText::new(name).weak() });
                let mut exposure = s.exposure.seconds().unwrap_or(0.0);
                let mut minutes = lights::minutes(s);
                ui.add_enabled_ui(s.enabled, |ui| {
                    let e = egui::DragValue::new(&mut exposure).range(0.001..=3600.0).speed(1.0).max_decimals(3).suffix(" s");
                    if ui.push_id(("exp", k), |ui| ui.add(e)).inner.changed() {
                        edit = Some(Edit::Exposure(i, exposure));
                    }
                });
                ui.add_enabled_ui(s.enabled, |ui| {
                    let m = egui::DragValue::new(&mut minutes)
                        .range(0.0..=6000.0)
                        .speed(1.0)
                        .custom_formatter(|m, _| fmt_duration(m * 60.0))
                        .custom_parser(parse_minutes);
                    if ui.push_id(("min", k), |ui| ui.add(m)).inner.on_hover_text("Type e.g. 90, 1:30 or 1.5 h").changed() {
                        edit = Some(Edit::Minutes(i, minutes));
                    }
                });
                ui.label(if s.enabled { RichText::new(format!("{} × {}", s.count, exposure_text(&s.exposure))) } else { RichText::new("") });
                let done = RichText::new(if s.done > 0 { s.done.to_string() } else { String::new() });
                ui.label(if s.done >= s.count { done.weak() } else { done })
                    .on_hover_text("Frames already taken; the sequence continues from here");
                ui.horizontal(|ui| {
                    if ui.push_id(("add", k), |ui| ui.small_button("+")).inner.on_hover_text("Another exposure with this filter").clicked() {
                        edit = Some(Edit::AddExposure(i));
                    }
                    if !first_of_filter && ui.push_id(("rm", k), |ui| ui.small_button("🗙")).inner.clicked() {
                        edit = Some(Edit::Remove(i));
                    }
                });
                ui.end_row();
            }
        });
        match edit {
            Some(Edit::Enable(row, on)) => lights::set_enabled(seq, row, on),
            Some(Edit::Exposure(i, e)) => lights::set_exposure(seq, i, e),
            Some(Edit::Minutes(i, m)) => lights::set_minutes(seq, i, m),
            Some(Edit::AddExposure(i)) => lights::add_exposure(seq, i),
            Some(Edit::Remove(i)) => {
                seq.steps.remove(i);
            }
            None => {}
        }

        let has_lights = seq.steps.iter().any(lights::is_light);
        ui.add_enabled_ui(has_lights, |ui| {
            ui.horizontal(|ui| {
                let before = lights::settings(seq);
                let mut set = before.clone();
                ui.label("Gain");
                ui.push_id("plan_gain", |ui| optional_value(ui, &mut set.gain, 100.0, 0.0..=10000.0, 1.0, ""));
                ui.label("Offset");
                ui.push_id("plan_off", |ui| optional_value(ui, &mut set.offset, 50.0, 0.0..=100000.0, 1.0, ""));
                ui.label("Bin");
                ui.push_id("plan_bin", |ui| {
                    let mut v = set.binning.map(f64::from);
                    optional_value(ui, &mut v, 1.0, 1.0..=8.0, 0.05, "");
                    set.binning = v.map(|b| b.round() as u32);
                });
                ui.label("Temperature").on_hover_text("Cool the sensor to this and wait for it before the frames");
                ui.push_id("plan_temp", |ui| optional_value(ui, &mut set.temperature, -10.0, -60.0..=40.0, 0.5, " °C"));
                if set != before {
                    lights::set_settings(seq, &set);
                }
            });
        });

        let mut own = seq.only(Setup::Sky);
        own.steps.retain(lights::is_light);
        ui.horizontal(|ui| {
            ui.label(RichText::new(plan_summary(&own)).strong());
            if ui
                .add_enabled(own.has_progress(), egui::Button::new("Reset progress"))
                .on_hover_text("Forget the light frames taken, to take them all again")
                .clicked()
            {
                for s in seq.steps.iter_mut().filter(|s| lights::is_light(s)) {
                    s.done = 0;
                    s.frames.clear();
                }
            }
        });
        if let Some(msg) = &self.plan_feedback {
            ui.label(RichText::new(msg).weak());
        }
    }

    /// Which corrections the lights get, the masters that takes, and what
    /// the masters folder has.
    fn masters(&mut self, ui: &mut egui::Ui, state: &UiState, actions: &mut Vec<ToEngine>) {
        ui.label(RichText::new("Calibration").strong());
        let plan = &mut self.draft.value.sequence.calibration;
        ui.horizontal(|ui| {
            ui.checkbox(&mut plan.darks, "Darks")
                .on_hover_text("Subtract a master dark matching each light's exposure, gain, offset and temperature");
            ui.checkbox(&mut plan.flats, "Flats").on_hover_text("Divide by a master flat for each filter");
            if plan.flats {
                ui.separator();
                ui.label("flats calibrated with");
                ui.radio_value(&mut plan.flats_with, FlatCalibration::Bias, "Bias");
                ui.radio_value(&mut plan.flats_with, FlatCalibration::DarkFlats, "Dark flats")
                    .on_hover_text("Darks at the flats' exposure, often better than bias on CMOS cameras");
            }
        });
        if !plan.darks {
            ui.label(RichText::new("Without darks the lights still need their offset removed: a master bias.").weak());
        }
        ui.horizontal(|ui| {
            ui.label("Save");
            let raw = ui
                .checkbox(&mut plan.save_raw, "raw frames")
                .on_hover_text("The lights as taken: keep them to calibrate again later, e.g. in Siril");
            let calibrated = ui.checkbox(&mut plan.save_calibrated, "calibrated frames").on_hover_text(
                "Also write each light with its masters applied (32-bit float) into a \"calibrated\" folder next to the raw ones",
            );
            // Something is always saved.
            if raw.changed() && !plan.save_raw {
                plan.save_calibrated = true;
            }
            if calibrated.changed() && !plan.save_calibrated {
                plan.save_raw = true;
            }
        });
        if !plan.save_raw {
            ui.colored_label(
                theme::c(ui, ORANGE),
                "Without raw frames the calibration cannot be redone. Frames that get no master are still saved raw.",
            );
        }
        ui.horizontal(|ui| {
            let p = &mut self.presets;
            ui.label("Frames per master: darks");
            ui.add(egui::DragValue::new(&mut p.dark_count).range(1..=500));
            ui.label("bias");
            ui.add(egui::DragValue::new(&mut p.bias_count).range(1..=500));
            ui.label("flats");
            ui.add(egui::DragValue::new(&mut p.flat_count).range(1..=500))
                .on_hover_text("Also the dark flats");
        });

        let m = &state.masters;
        let needs = protocol::masters::needs(&self.draft.value.sequence, state);
        if needs.is_empty() {
            ui.label(RichText::new("No enabled light steps, so no masters needed.").weak());
        } else {
            let seq = &self.draft.value.sequence;
            let counts = self.counts();
            egui::Grid::new("masters_needed").striped(true).num_columns(4).show(ui, |ui| {
                for n in &needs {
                    // The step taking its frames, while it is missing.
                    let taking = [Setup::Covered, Setup::Panel]
                        .into_iter()
                        .flat_map(|setup| protocol::masters::missing_steps(std::slice::from_ref(n), setup, counts))
                        .next()
                        .and_then(|step| seq.matching(&step));
                    ui.label(need_name(n));
                    ui.label(RichText::new(match n.purpose {
                        Purpose::Lights => "for the lights",
                        Purpose::Flats => "for the flats",
                    })
                    .weak());
                    ui.label(&n.what);
                    need_status(ui, n, taking);
                    ui.end_row();
                }
            });
            if needs.iter().any(|n| n.found.is_none()) {
                ui.label(
                    RichText::new("Take covered frames and Take flats at the top make the missing ones.").weak(),
                );
            }
        }
        if let Some(text) = &m.creating {
            ui.colored_label(theme::c(ui, YELLOW), format!("{text}…"));
        }
        self.hand_made(ui);
        if let Some(e) = &m.error {
            ui.colored_label(theme::c(ui, ORANGE), e);
        }

        let title = format!("Masters folder: {} masters", m.masters.len());
        egui::CollapsingHeader::new(title).id_salt("masters_folder").show(ui, |ui| {
            ui.horizontal(|ui| {
                if ui.small_button("Rescan").on_hover_text("Read the folder again now").clicked() {
                    actions.push(ToEngine::RescanMasters);
                }
                // Long paths would widen the panel.
                ui.add(egui::Label::new(RichText::new(&m.folder).monospace()).truncate())
                    .on_hover_text(format!(
                        "{}\nSet it under Imaging, Output files. It is read every minute; subfolders count too.",
                        m.folder
                    ));
            });
            if !m.masters.is_empty() {
                egui::Grid::new("masters_all_grid").striped(true).num_columns(3).show(ui, |ui| {
                    for f in &m.masters {
                        ui.label(f.kind.label());
                        ui.label(&f.path);
                        ui.label(RichText::new(master_details(f)).weak());
                        ui.end_row();
                    }
                });
            }
            if !m.skipped.is_empty() {
                ui.label(RichText::new(format!("Not used ({})", m.skipped.len())).strong());
                egui::Grid::new("masters_skipped_grid").striped(true).num_columns(2).show(ui, |ui| {
                    for (path, why) in &m.skipped {
                        ui.label(path);
                        ui.label(RichText::new(why).weak());
                        ui.end_row();
                    }
                });
            }
        });
    }

    /// Calibration steps of older sequences, made by hand: they run with
    /// Take, and can be removed here.
    fn hand_made(&mut self, ui: &mut egui::Ui) {
        let steps = &mut self.draft.value.sequence.steps;
        if !steps.iter().any(|s| s.frame_type != FrameType::Light && !s.auto) {
            return;
        }
        ui.label(RichText::new("Calibration steps added by hand (taken with the Take buttons)").weak());
        let mut remove = None;
        egui::Grid::new("hand_made").striped(true).num_columns(4).show(ui, |ui| {
            for (i, s) in steps.iter().enumerate().filter(|(_, s)| s.frame_type != FrameType::Light && !s.auto) {
                ui.label(s.frame_type.label());
                ui.label(exposure_text(&s.exposure));
                ui.label(format!("{} / {}", s.done.min(s.count), s.count));
                if ui.push_id(("hand", i), |ui| ui.small_button("🗙")).inner.on_hover_text("Remove").clicked() {
                    remove = Some(i);
                }
                ui.end_row();
            }
        });
        if let Some(i) = remove {
            steps.remove(i);
        }
    }

}

fn capitalised(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
}

fn need_name(n: &Need) -> &'static str {
    match (n.kind, n.purpose) {
        (MasterKind::Dark, Purpose::Flats) => "Dark flat",
        (MasterKind::Dark, _) => "Dark",
        (MasterKind::Flat, _) => "Flat",
        (MasterKind::Bias, _) => "Bias",
    }
}

/// The master found: green when it fully matches, yellow when its header
/// lacks values it could not be checked against; orange when missing.
fn need_status(ui: &mut egui::Ui, n: &Need, taking: Option<&protocol::SequenceStep>) {
    let Some(found) = &n.found else {
        if let Some(step) = taking.filter(|s| s.done > 0) {
            ui.colored_label(theme::c(ui, YELLOW), format!("{} / {} taken", step.done.min(step.count), step.count))
                .on_hover_text("Take continues with the frames still missing; the master is made when they are all in");
        } else if n.spec.is_none() {
            ui.label(RichText::new("after the flat").weak());
        } else {
            ui.colored_label(theme::c(ui, ORANGE), "missing");
        }
        return;
    };
    let unknown = n.unverified();
    let details = master_details(found);
    if unknown.is_empty() {
        ui.colored_label(theme::c(ui, GREEN), &found.path).on_hover_text(details);
    } else {
        let why = format!("{} unknown", unknown.join(", "));
        ui.colored_label(theme::c(ui, YELLOW), format!("{} ? ({why})", found.path))
            .on_hover_text(format!("{details}\nThe header has no {}, so that could not be checked.", unknown.join(" or ")));
    }
}

/// "120 s, gain 100, −10.1 °C, bin 1, Red, 30 frames, 2026-10-01".
fn master_details(m: &MasterFrame) -> String {
    let mut parts = Vec::new();
    if m.kind == MasterKind::Dark
        && let Some(e) = m.exposure
    {
        parts.push(format!("{e} s"));
    }
    if let Some(f) = &m.filter {
        parts.push(f.clone());
    }
    if let Some(g) = m.gain {
        parts.push(format!("gain {g}"));
    }
    if let Some(o) = m.offset {
        parts.push(format!("offset {o}"));
    }
    if let Some(t) = m.temperature {
        parts.push(format!("{t:.1} °C"));
    }
    parts.push(format!("bin {}", m.binning.unwrap_or(1)));
    parts.push(format!("{}×{}", m.width, m.height));
    if let Some(n) = m.frames {
        parts.push(format!("{n} frames"));
    }
    parts.push(m.date.get(..10).unwrap_or(&m.date).to_owned());
    parts.join(", ")
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
    fn renders_editor_and_status() {
        let ctx = egui::Context::default();
        let mut view = SequenceView::new(PresetForm::default());
        let draft = &mut view.draft.value;
        draft.sequence.steps = presets::darks(&[60.0, 120.0], 10, Some(-10.0));
        draft.sequence.steps.extend(presets::flats(&[Some(1), None], 5));
        draft.sequence.steps.push(presets::bias(20, None));
        draft.sequence.steps.push(SequenceStep::default());
        let draft = draft.clone();
        let mut imaging = ImagingView::new(true);
        let dark = MasterFrame {
            path: "dark.fits".into(),
            kind: MasterKind::Dark,
            camera: None,
            width: 10,
            height: 10,
            binning: None,
            exposure: Some(60.0),
            gain: None,
            offset: None,
            temperature: None,
            filter: None,
            frames: Some(20),
            date: "2026-10-01".into(),
        };
        let idle = UiState {
            masters: protocol::MastersStatus {
                masters: vec![dark],
                skipped: vec![("raw.fits".into(), "a single dark frame, not a master".into())],
                ..Default::default()
            },
            ..UiState::default()
        };
        let running = UiState {
            sequence: Some(SequenceStatus {
                sequence: draft.sequence.clone(),
                state: RunState::Running,
                current_step: Some(1),
                done: vec![10, 3, 0, 0, 0, 0],
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
