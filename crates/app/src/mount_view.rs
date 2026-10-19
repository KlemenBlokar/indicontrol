//! Mount tab: position, meridian timing, tracking, park and goto.

use crate::object_search::ObjectSearch;
use crate::theme;
use eframe::egui::{self, Color32, RichText};
use indi_link::{format_number, parse_number};
use protocol::{GotoDraft, MountInfo, SharedDoc, SolverKind, SolverSettings, ToEngine, UiState};

const RED: Color32 = Color32::from_rgb(220, 60, 60);
const ORANGE: Color32 = Color32::from_rgb(230, 160, 40);

#[derive(Default)]
pub struct MountView {
    pub goto: SharedDoc<GotoDraft>,
    pub solver_edit: SharedDoc<SolverSettings>,
    pub search: ObjectSearch,
}

/// How close the mount must point to the last found object to be "at" it.
const AT_OBJECT_ARCMIN: f64 = 60.0;

pub fn hms(hours: f64) -> String {
    format_number(hours.rem_euclid(24.0), "%010.6m").trim().to_owned()
}

pub fn dms(deg: f64) -> String {
    let s = format_number(deg, "%010.6m").trim().to_owned();
    if deg >= 0.0 { format!("+{s}") } else { s }
}

impl MountView {
    /// Name of the last found object, if the mount points at it.
    pub fn object_at(&self, m: &MountInfo) -> Option<&str> {
        let o = self.goto.value.object.as_ref()?;
        let off = protocol::mount::separation_arcmin(o.ra_hours, o.dec_deg, m.ra_j2000, m.dec_j2000);
        (off < AT_OBJECT_ARCMIN).then_some(o.name.as_str())
    }


    pub fn ui(&mut self, ui: &mut egui::Ui, state: &UiState, actions: &mut Vec<ToEngine>) {
        let Some(m) = &state.mount else {
            ui.label(RichText::new(match &state.roles.mount {
                None => "Assign a mount in Equipment.",
                Some(_) => "The mount is not connected.",
            })
            .weak());
            return;
        };
        ui.horizontal(|ui| {
            ui.heading(&m.name);
            if m.slewing {
                ui.add(egui::Spinner::new());
                ui.colored_label(theme::c(ui, ORANGE), "Slewing");
            }
            if ui.button(RichText::new("■ Abort motion").color(theme::c(ui, RED))).clicked() {
                actions.push(ToEngine::MountAbort);
            }
        });
        ui.add_space(4.0);
        position(ui, m);
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if let Some(tracking) = m.tracking {
                ui.label("Tracking");
                let unparked = m.parked != Some(true);
                if ui.add_enabled(unparked, egui::Button::selectable(tracking, "On")).clicked() && !tracking {
                    actions.push(ToEngine::MountTracking { on: true });
                }
                if ui.add_enabled(unparked, egui::Button::selectable(!tracking, "Off")).clicked() && tracking {
                    actions.push(ToEngine::MountTracking { on: false });
                }
                ui.separator();
            }
            match m.parked {
                Some(true) => {
                    ui.label("Parked");
                    if ui.button("Unpark").clicked() {
                        actions.push(ToEngine::MountUnpark);
                    }
                }
                Some(false) => {
                    let park = ui.button("Park").on_hover_text("Stops guiding and parks the mount");
                    if park.clicked() {
                        actions.push(ToEngine::MountPark);
                    }
                }
                None => {}
            }
        });
        ui.separator();
        self.goto(ui, m, state, actions);
        ui.separator();
        solve_status(ui, state, actions);
        self.solver_settings(ui, state, actions);
    }

    fn solver_settings(&mut self, ui: &mut egui::Ui, state: &UiState, actions: &mut Vec<ToEngine>) {
        egui::CollapsingHeader::new("Plate solver settings").show(ui, |ui| {
            let s = &mut self.solver_edit.value;
            egui::Grid::new("solver").num_columns(2).spacing([8.0, 4.0]).show(ui, |ui| {
                ui.label("Solver");
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut s.kind, SolverKind::Astap, "ASTAP");
                    ui.selectable_value(&mut s.kind, SolverKind::AstrometryNet, "astrometry.net");
                });
                ui.end_row();
                match s.kind {
                    SolverKind::Astap => {
                        ui.label("ASTAP program");
                        ui.text_edit_singleline(&mut s.astap_path)
                            .on_hover_text("astap or astap_cli, with a path if it is not on PATH (on the engine's computer)");
                    }
                    SolverKind::AstrometryNet => {
                        ui.label("solve-field program");
                        ui.text_edit_singleline(&mut s.astrometry_path)
                            .on_hover_text("Needs index files for your field of view installed");
                    }
                }
                ui.end_row();
                ui.label("Search radius");
                ui.add(egui::DragValue::new(&mut s.search_radius).range(1.0..=180.0).suffix("°"));
                ui.end_row();
                ui.label("Downsample");
                ui.add(egui::DragValue::new(&mut s.downsample).range(0..=8)).on_hover_text("0 = solver default");
                ui.end_row();
                ui.label("Timeout");
                ui.add(egui::DragValue::new(&mut s.timeout).range(5.0..=600.0).suffix(" s"));
                ui.end_row();
                ui.label("Extra arguments");
                ui.text_edit_singleline(&mut s.extra_args);
                ui.end_row();
                ui.label("Exposure");
                ui.add(egui::DragValue::new(&mut s.exposure).range(0.01..=120.0).speed(0.1).suffix(" s"));
                ui.end_row();
                ui.label("Binning");
                let mut bin = s.binning.unwrap_or(1);
                ui.add(egui::DragValue::new(&mut bin).range(1..=4).prefix("×"));
                s.binning = Some(bin);
                ui.end_row();
                ui.label("Centre within");
                ui.add(egui::DragValue::new(&mut s.center_tolerance).range(0.1..=60.0).speed(0.1).suffix("'"));
                ui.end_row();
                ui.label("Max attempts");
                ui.add(egui::DragValue::new(&mut s.center_attempts).range(1..=20));
                ui.end_row();
            });
            let changed = *s != state.solver_settings;
            ui.horizontal(|ui| {
                if ui.add_enabled(changed, egui::Button::new("Apply")).clicked() {
                    actions.push(ToEngine::SetSolverSettings(s.clone()));
                }
                if ui.add_enabled(changed, egui::Button::new("Revert")).clicked() {
                    *s = state.solver_settings.clone();
                }
            });
        });
    }

    fn goto(&mut self, ui: &mut egui::Ui, m: &MountInfo, state: &UiState, actions: &mut Vec<ToEngine>) {
        ui.label(RichText::new("Go to (J2000)").strong());
        if let Some(o) = self.search.ui(ui, actions) {
            self.goto.value.ra = hms(o.ra_hours);
            self.goto.value.dec = dms(o.dec_deg);
            self.goto.value.object = Some(o);
        }
        let ra = parse_number(&self.goto.value.ra);
        let dec = parse_number(&self.goto.value.dec);
        let ra_ok = ra.is_some_and(|r| (0.0..24.0).contains(&r));
        let dec_ok = dec.is_some_and(|d| (-90.0..=90.0).contains(&d));
        let ra_bad = !self.goto.value.ra.is_empty() && !ra_ok;
        let dec_bad = !self.goto.value.dec.is_empty() && !dec_ok;
        // The edits sit in the grid's last column: egui sizes earlier columns from
        // their first-frame width, which would leave the edits only ~40 px wide.
        egui::Grid::new("goto").num_columns(2).show(ui, |ui| {
            ui.label("RA");
            ui.horizontal(|ui| {
                let mut edit = egui::TextEdit::singleline(&mut self.goto.value.ra)
                    .hint_text("05:35:17")
                    .desired_width(120.0);
                if ra_bad {
                    edit = edit.text_color(RED);
                }
                ui.add(edit);
                ui.label(RichText::new("hours").weak());
            });
            ui.end_row();
            ui.label("Dec");
            ui.horizontal(|ui| {
                let mut edit = egui::TextEdit::singleline(&mut self.goto.value.dec)
                    .hint_text("-05:23:28")
                    .desired_width(120.0);
                if dec_bad {
                    edit = edit.text_color(RED);
                }
                ui.add(edit);
                ui.label(RichText::new("degrees").weak());
            });
            ui.end_row();
        });
        ui.horizontal(|ui| {
            let can = ra_ok && dec_ok && m.parked != Some(true);
            if ui
                .add_enabled(can, egui::Button::new("Slew"))
                .on_disabled_hover_text(if m.parked == Some(true) {
                    "Unpark the mount first"
                } else {
                    "Enter RA (h:m:s or hours) and Dec (d:m:s or degrees)"
                })
                .clicked()
            {
                actions.push(ToEngine::MountGoto {
                    ra_hours: ra.expect("checked"),
                    dec_deg: dec.expect("checked"),
                });
            }
            if ui
                .add_enabled(can && !state.solve.busy, egui::Button::new("Slew & center"))
                .on_hover_text("Slew, then plate solve, sync and re-slew until centred")
                .clicked()
            {
                actions.push(ToEngine::CenterOn {
                    ra_hours: ra.expect("checked"),
                    dec_deg: dec.expect("checked"),
                });
            }
            if ui.button("Use current position").clicked() {
                self.goto.value.ra = hms(m.ra_j2000);
                self.goto.value.dec = dms(m.dec_j2000);
                self.search.clear();
            }
        });
    }
}

fn solve_status(ui: &mut egui::Ui, state: &UiState, actions: &mut Vec<ToEngine>) {
    let st = &state.solve;
    ui.horizontal(|ui| {
        ui.label(RichText::new("Plate solving").strong());
        ui.label(RichText::new(format!("({})", state.solver_settings.kind.label())).weak());
        if st.busy {
            ui.add(egui::Spinner::new());
            if ui.button("Abort").clicked() {
                actions.push(ToEngine::AbortSolve);
            }
        } else if ui
            .button("Solve & sync")
            .on_hover_text("Take a frame with the main camera, solve it and sync the mount")
            .clicked()
        {
            actions.push(ToEngine::SolveAndSync);
        }
    });
    if !st.message.is_empty() {
        ui.label(&st.message);
    }
    if let Some(r) = &st.last {
        egui::Grid::new("solution").num_columns(2).show(ui, |ui| {
            ui.label("Centre (J2000)");
            ui.label(RichText::new(format!("{}  {}", hms(r.ra_hours), dms(r.dec_deg))).monospace());
            ui.end_row();
            ui.label("Scale");
            ui.label(format!("{:.2}\"/px, field {:.2}° × {:.2}°", r.pixel_scale, r.width_deg, r.height_deg));
            ui.end_row();
            ui.label("Rotation");
            ui.label(format!("{:.1}°{}", r.rotation, if r.mirrored { " (mirrored)" } else { "" }));
            ui.end_row();
            if let Some(e) = r.pointing_error {
                ui.label("Mount error");
                ui.label(format!("{e:.1}'"));
                ui.end_row();
            }
        });
    }
}

fn position(ui: &mut egui::Ui, m: &MountInfo) {
    egui::Grid::new("mount_pos").num_columns(3).spacing([16.0, 4.0]).show(ui, |ui| {
        ui.label("");
        ui.label(RichText::new("RA").weak());
        ui.label(RichText::new("Dec").weak());
        ui.end_row();
        ui.label("J2000");
        ui.label(RichText::new(hms(m.ra_j2000)).monospace());
        ui.label(RichText::new(dms(m.dec_j2000)).monospace());
        ui.end_row();
        ui.label("JNow");
        ui.label(RichText::new(hms(m.ra)).monospace());
        ui.label(RichText::new(dms(m.dec)).monospace());
        ui.end_row();
    });
    egui::Grid::new("mount_state").num_columns(2).spacing([16.0, 4.0]).show(ui, |ui| {
        if let Some(alt) = m.alt {
            ui.label("Altitude");
            let text = RichText::new(format!("{alt:.1}°"));
            ui.label(if alt < 15.0 { text.color(theme::c(ui, ORANGE)) } else { text });
            ui.end_row();
        }
        if let Some(ha) = m.hour_angle {
            ui.label("Hour angle");
            // Signed, so east (negative) and west of the meridian read at a glance.
            let sign = if ha < 0.0 { "-" } else { "+" };
            ui.label(RichText::new(format!("{sign}{}", hms(ha.abs()))).monospace());
            ui.end_row();
        }
        // A parked or stopped mount never reaches the meridian.
        let moving = m.tracking != Some(false) && m.parked != Some(true);
        if let Some(min) = m.minutes_to_meridian().filter(|_| moving) {
            ui.label("Meridian");
            ui.label(if min >= 0.0 {
                format!("in {}", duration(min))
            } else {
                format!("passed {} ago", duration(-min))
            });
            ui.end_row();
        }
        if let Some(side) = &m.pier_side {
            ui.label("Pier side");
            ui.label(match side.as_str() {
                "WEST" => "West (pointing east)",
                "EAST" => "East (pointing west)",
                other => other,
            });
            ui.end_row();
        }
        if let Some(lst) = m.lst {
            ui.label("Sidereal time");
            ui.label(RichText::new(hms(lst)).monospace());
            ui.end_row();
        }
    });
}

fn duration(minutes: f64) -> String {
    if minutes >= 60.0 {
        format!("{} h {:02} min", (minutes / 60.0).floor(), (minutes % 60.0).round() as u32 % 60)
    } else {
        format!("{minutes:.0} min")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formatting() {
        assert_eq!(hms(5.5), "5:30:00");
        assert_eq!(dms(-5.391), "-5:23:28");
        assert_eq!(dms(41.25), "+41:15:00");
        assert_eq!(duration(23.4), "23 min");
        assert_eq!(duration(135.0), "2 h 15 min");
    }

    #[test]
    fn renders() {
        let ctx = egui::Context::default();
        let mut view = MountView::default();
        let mut state = UiState::default();
        let mut actions = Vec::new();
        for _ in 0..2 {
            let _ = ctx.run_ui(egui::RawInput::default(), |ui| view.ui(ui, &state, &mut actions));
        }
        state.mount = Some(MountInfo {
            name: "Scope".into(),
            ra: 5.6,
            dec: -5.4,
            ra_j2000: 5.58,
            dec_j2000: -5.39,
            alt: Some(30.0),
            hour_angle: Some(-0.5),
            lst: Some(5.1),
            pier_side: Some("WEST".into()),
            tracking: Some(true),
            parked: Some(false),
            slewing: true,
        });
        view.goto.value.ra = "5:35:17".into();
        view.goto.value.dec = "-5:23:28".into();
        for _ in 0..2 {
            let _ = ctx.run_ui(egui::RawInput::default(), |ui| view.ui(ui, &state, &mut actions));
        }
        assert!(actions.is_empty());
    }

    #[test]
    fn object_at_mount_position() {
        let mut view = MountView::default();
        let mut m = MountInfo {
            name: "Scope".into(),
            ra: 0.69,
            dec: 41.8,
            ra_j2000: 0.673,
            dec_j2000: 41.685,
            alt: None,
            hour_angle: None,
            lst: None,
            pier_side: None,
            tracking: Some(true),
            parked: Some(false),
            slewing: false,
        };
        assert_eq!(view.object_at(&m), None);
        view.goto.value.object = Some(protocol::CatalogObject {
            name: "M 110".into(),
            aliases: vec!["NGC 205".into()],
            common_names: vec![],
            kind: "Galaxy".into(),
            ra_hours: 0.6728,
            dec_deg: 41.6853,
            mag: Some(8.15),
        });
        assert_eq!(view.object_at(&m), Some("M 110"));
        // Slewed elsewhere since.
        m.dec_j2000 = 43.0;
        assert_eq!(view.object_at(&m), None);
    }
}
