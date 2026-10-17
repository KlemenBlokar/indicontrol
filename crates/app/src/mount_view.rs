//! Mount tab: position, meridian timing, tracking, park and goto.

use eframe::egui::{self, Color32, RichText};
use indi_link::{format_number, parse_number};
use protocol::{MountInfo, ToEngine, UiState};

const RED: Color32 = Color32::from_rgb(220, 60, 60);
const ORANGE: Color32 = Color32::from_rgb(230, 160, 40);

#[derive(Default)]
pub struct MountView {
    ra_input: String,
    dec_input: String,
}

fn hms(hours: f64) -> String {
    format_number(hours.rem_euclid(24.0), "%010.6m").trim().to_owned()
}

fn dms(deg: f64) -> String {
    let s = format_number(deg, "%010.6m").trim().to_owned();
    if deg >= 0.0 { format!("+{s}") } else { s }
}

impl MountView {
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
                ui.colored_label(ORANGE, "Slewing");
            }
            if ui.button(RichText::new("■ Abort motion").color(RED)).clicked() {
                actions.push(ToEngine::MountAbort);
            }
        });
        ui.add_space(4.0);
        position(ui, m);
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if let Some(tracking) = m.tracking {
                ui.label("Tracking");
                if ui.selectable_label(tracking, "On").clicked() && !tracking {
                    actions.push(ToEngine::MountTracking { on: true });
                }
                if ui.selectable_label(!tracking, "Off").clicked() && tracking {
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
                    if ui.button("Park").on_hover_text("Stops guiding and parks the mount").clicked() {
                        actions.push(ToEngine::MountPark);
                    }
                }
                None => {}
            }
        });
        ui.separator();
        self.goto(ui, m, actions);
    }

    fn goto(&mut self, ui: &mut egui::Ui, m: &MountInfo, actions: &mut Vec<ToEngine>) {
        ui.label(RichText::new("Go to (J2000)").strong());
        let ra = parse_number(&self.ra_input);
        let dec = parse_number(&self.dec_input);
        let ra_ok = ra.is_some_and(|r| (0.0..24.0).contains(&r));
        let dec_ok = dec.is_some_and(|d| (-90.0..=90.0).contains(&d));
        let ra_bad = !self.ra_input.is_empty() && !ra_ok;
        let dec_bad = !self.dec_input.is_empty() && !dec_ok;
        egui::Grid::new("goto").num_columns(3).show(ui, |ui| {
            ui.label("RA");
            let mut edit = egui::TextEdit::singleline(&mut self.ra_input)
                .hint_text("05:35:17")
                .desired_width(120.0);
            if ra_bad {
                edit = edit.text_color(RED);
            }
            ui.add(edit);
            ui.label(RichText::new("hours").weak());
            ui.end_row();
            ui.label("Dec");
            let mut edit = egui::TextEdit::singleline(&mut self.dec_input)
                .hint_text("-05:23:28")
                .desired_width(120.0);
            if dec_bad {
                edit = edit.text_color(RED);
            }
            ui.add(edit);
            ui.label(RichText::new("degrees").weak());
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
            if ui.button("Use current position").clicked() {
                self.ra_input = hms(m.ra_j2000);
                self.dec_input = dms(m.dec_j2000);
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
            ui.label(if alt < 15.0 { text.color(ORANGE) } else { text });
            ui.end_row();
        }
        if let Some(ha) = m.hour_angle {
            ui.label("Hour angle");
            ui.label(RichText::new(hms(ha + 24.0 * f64::from(ha < 0.0))).monospace());
            ui.end_row();
        }
        if let Some(min) = m.minutes_to_meridian() {
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
        view.ra_input = "5:35:17".into();
        view.dec_input = "-5:23:28".into();
        for _ in 0..2 {
            let _ = ctx.run_ui(egui::RawInput::default(), |ui| view.ui(ui, &state, &mut actions));
        }
        assert!(actions.is_empty());
    }
}
