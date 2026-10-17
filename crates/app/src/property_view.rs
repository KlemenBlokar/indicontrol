//! Generic INDI property editor widgets.
//!
//! Widgets never talk to the engine directly; they push [`ToEngine`] commands
//! which the app sends after drawing.

use std::collections::HashMap;

use eframe::egui::{self, Color32, RichText};
use indi_link::{
    ElementValue, Property, PropertyKind, PropertyState, SwitchRule, format_number, parse_number,
};
use protocol::ToEngine;

/// Pending user input for text/number elements, keyed by (device, property, element).
#[derive(Default)]
pub struct Edits(HashMap<(String, String, String), String>);

impl Edits {
    fn buffer(&mut self, p: &Property, element: &str) -> &mut String {
        self.0
            .entry((p.device.clone(), p.name.clone(), element.to_owned()))
            .or_default()
    }

    fn get(&self, p: &Property, element: &str) -> Option<&str> {
        self.0
            .get(&(p.device.clone(), p.name.clone(), element.to_owned()))
            .map(String::as_str)
            .filter(|s| !s.trim().is_empty())
    }

    fn clear(&mut self, p: &Property) {
        self.0.retain(|(d, n, _), _| d != &p.device || n != &p.name);
    }
}

pub fn state_color(state: PropertyState) -> Color32 {
    match state {
        PropertyState::Idle => Color32::from_gray(120),
        PropertyState::Ok => Color32::from_rgb(60, 180, 75),
        PropertyState::Busy => Color32::from_rgb(230, 190, 40),
        PropertyState::Alert => Color32::from_rgb(220, 50, 50),
    }
}

pub fn state_dot(ui: &mut egui::Ui, state: PropertyState) -> egui::Response {
    let size = ui.text_style_height(&egui::TextStyle::Body);
    let (rect, response) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::hover());
    ui.painter()
        .circle_filled(rect.center(), size * 0.35, state_color(state));
    response.on_hover_text(state.to_string())
}

pub fn show_property(ui: &mut egui::Ui, p: &Property, edits: &mut Edits, actions: &mut Vec<ToEngine>) {
    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            state_dot(ui, p.state);
            ui.label(RichText::new(&p.label).strong()).on_hover_text(format!(
                "{}.{}\nstate: {}  perm: {:?}  timeout: {}s{}",
                p.device,
                p.name,
                p.state,
                p.perm,
                p.timeout,
                p.timestamp
                    .as_deref()
                    .map(|t| format!("\nupdated: {t}"))
                    .unwrap_or_default()
            ));
        });
        match p.kind {
            PropertyKind::Text | PropertyKind::Number => show_editable(ui, p, edits, actions),
            PropertyKind::Switch => show_switches(ui, p, actions),
            PropertyKind::Light => show_lights(ui, p),
            PropertyKind::Blob => show_blobs(ui, p),
        }
    });
}

fn show_editable(ui: &mut egui::Ui, p: &Property, edits: &mut Edits, actions: &mut Vec<ToEngine>) {
    let writable = p.perm.writable();
    let mut submit = false;
    let mut invalid = false;
    egui::Grid::new((&p.device, &p.name))
        .num_columns(if writable { 3 } else { 2 })
        .spacing([12.0, 4.0])
        .show(ui, |ui| {
            for el in &p.elements {
                let current = match &el.value {
                    ElementValue::Text(t) => t.clone(),
                    ElementValue::Number(n) => format_number(n.value, &n.format).trim().to_owned(),
                    _ => continue,
                };
                let label = ui.label(&el.label);
                if let ElementValue::Number(n) = &el.value {
                    label.on_hover_text(format!(
                        "{}\nmin {}  max {}  step {}",
                        el.name, n.min, n.max, n.step
                    ));
                } else {
                    label.on_hover_text(&el.name);
                }
                ui.label(RichText::new(&current).monospace());
                if writable {
                    let is_number = matches!(el.value, ElementValue::Number(_));
                    let buf = edits.buffer(p, &el.name);
                    let bad = is_number && !buf.trim().is_empty() && parse_number(buf).is_none();
                    invalid |= bad;
                    let mut edit = egui::TextEdit::singleline(buf)
                        .hint_text(&current)
                        .desired_width(if is_number { 120.0 } else { 220.0 });
                    if bad {
                        edit = edit.text_color(Color32::from_rgb(220, 50, 50));
                    }
                    let r = ui.add(edit);
                    if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        submit = true;
                    }
                }
                ui.end_row();
            }
        });
    if writable {
        ui.horizontal(|ui| {
            if ui
                .add_enabled(!invalid, egui::Button::new("Set"))
                .on_disabled_hover_text("Fix the highlighted values first")
                .clicked()
            {
                submit = true;
            }
        });
    }
    if !submit || invalid {
        return;
    }
    // Send the whole vector: edited values where given, current values otherwise.
    let device = p.device.clone();
    let name = p.name.clone();
    match p.kind {
        PropertyKind::Text => {
            let values = p
                .elements
                .iter()
                .filter_map(|el| match &el.value {
                    ElementValue::Text(t) => Some((
                        el.name.clone(),
                        edits.get(p, &el.name).unwrap_or(t).to_owned(),
                    )),
                    _ => None,
                })
                .collect();
            actions.push(ToEngine::SetText {
                device,
                name,
                values,
            });
        }
        PropertyKind::Number => {
            let values = p
                .elements
                .iter()
                .filter_map(|el| match &el.value {
                    ElementValue::Number(n) => Some((
                        el.name.clone(),
                        edits
                            .get(p, &el.name)
                            .and_then(parse_number)
                            .unwrap_or(n.value),
                    )),
                    _ => None,
                })
                .collect();
            actions.push(ToEngine::SetNumber {
                device,
                name,
                values,
            });
        }
        _ => {}
    }
    edits.clear(p);
}

fn show_switches(ui: &mut egui::Ui, p: &Property, actions: &mut Vec<ToEngine>) {
    let writable = p.perm.writable();
    ui.horizontal_wrapped(|ui| {
        for el in &p.elements {
            let ElementValue::Switch(on) = el.value else {
                continue;
            };
            let clicked = match p.rule {
                SwitchRule::AnyOfMany => {
                    let mut v = on;
                    ui.add_enabled(writable, egui::Checkbox::new(&mut v, &el.label))
                        .on_hover_text(&el.name)
                        .changed()
                }
                SwitchRule::OneOfMany | SwitchRule::AtMostOne => ui
                    .add_enabled(writable, egui::Button::selectable(on, &el.label))
                    .on_hover_text(&el.name)
                    .clicked(),
            };
            if !clicked {
                continue;
            }
            let values = match p.rule {
                // Clicking the active OneOfMany switch is a no-op.
                SwitchRule::OneOfMany if on => continue,
                // Server enforces exclusivity; sending just the chosen element is enough.
                SwitchRule::OneOfMany => vec![(el.name.clone(), true)],
                SwitchRule::AtMostOne | SwitchRule::AnyOfMany => vec![(el.name.clone(), !on)],
            };
            actions.push(ToEngine::SetSwitch {
                device: p.device.clone(),
                name: p.name.clone(),
                values,
            });
        }
    });
}

fn show_lights(ui: &mut egui::Ui, p: &Property) {
    ui.horizontal_wrapped(|ui| {
        for el in &p.elements {
            if let ElementValue::Light(state) = el.value {
                state_dot(ui, state);
                ui.label(&el.label).on_hover_text(&el.name);
                ui.add_space(8.0);
            }
        }
    });
}

fn show_blobs(ui: &mut egui::Ui, p: &Property) {
    for el in &p.elements {
        if let ElementValue::Blob(b) = &el.value {
            let info = if b.size == 0 {
                "no data received".to_owned()
            } else {
                format!("{} — {}", b.format, human_bytes(b.size))
            };
            ui.horizontal(|ui| {
                ui.label(&el.label).on_hover_text(&el.name);
                ui.label(RichText::new(info).weak());
            });
        }
    }
}

pub fn human_bytes(n: usize) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut v = n as f64;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[unit])
    }
}
