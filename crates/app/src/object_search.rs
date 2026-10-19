//! Find-as-you-type lookup in the engine's object catalogue; picking a
//! match gives its J2000 coordinates.

use eframe::egui::{self, RichText};
use protocol::{CatalogObject, ToEngine};

#[derive(Default)]
pub struct ObjectSearch {
    query: String,
    /// Last query sent to the engine.
    asked: String,
    /// Matches, and the query they answer.
    results: Vec<CatalogObject>,
    answered: Option<String>,
    /// Name of the object picked last; hides the matches until the query changes.
    picked: Option<String>,
}

impl ObjectSearch {
    pub fn on_results(&mut self, query: &str, objects: &[CatalogObject]) {
        if query == self.asked {
            self.results = objects.to_vec();
            self.answered = Some(query.to_owned());
        }
    }

    pub fn clear(&mut self) {
        *self = Self::default();
    }

    /// The search field with its matches below; returns the object picked
    /// (click, or Enter for the best match).
    pub fn ui(&mut self, ui: &mut egui::Ui, actions: &mut Vec<ToEngine>) -> Option<CatalogObject> {
        let edit = ui
            .horizontal(|ui| {
                ui.label("Find");
                ui.add(
                    egui::TextEdit::singleline(&mut self.query)
                        .hint_text("M 110, NGC 7000, Orion Nebula")
                        .desired_width(220.0),
                )
            })
            .inner;
        let q = self.query.trim().to_owned();
        if q.is_empty() {
            self.asked.clear();
            self.results.clear();
            self.answered = None;
        } else if q != self.asked {
            actions.push(ToEngine::FindObjects { query: q.clone() });
            self.asked.clone_from(&q);
        }
        let current = self.answered.as_deref() == Some(q.as_str());
        let mut pick = None;
        if edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) && current {
            pick = self.results.first().cloned();
        }
        if current && self.picked.as_deref() != Some(q.as_str()) {
            if self.results.is_empty() {
                ui.label(RichText::new("Not in the catalogue (NGC, IC, Messier and others).").weak());
            }
            for o in &self.results {
                if ui.add(egui::Button::selectable(false, label(o))).clicked() {
                    pick = Some(o.clone());
                }
            }
        }
        if let Some(o) = &pick {
            self.query.clone_from(&o.name);
            self.asked.clone_from(&o.name);
            self.picked = Some(o.name.clone());
        }
        pick
    }
}

/// "M 110 (NGC 205): Galaxy, mag 8.2", with common names if any.
fn label(o: &CatalogObject) -> String {
    let mut s = o.name.clone();
    if !o.aliases.is_empty() {
        s += &format!(" ({})", o.aliases.join(", "));
    }
    if let Some(name) = o.common_names.first() {
        s += &format!(" {name}");
    }
    s += &format!(": {}", o.kind);
    if let Some(mag) = o.mag {
        s += &format!(", mag {mag:.1}");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m110() -> CatalogObject {
        CatalogObject {
            name: "M 110".into(),
            aliases: vec!["NGC 205".into()],
            common_names: vec![],
            kind: "Galaxy".into(),
            ra_hours: 0.673,
            dec_deg: 41.685,
            mag: Some(8.15),
        }
    }

    fn run(ctx: &egui::Context, search: &mut ObjectSearch, actions: &mut Vec<ToEngine>) -> Option<CatalogObject> {
        let mut picked = None;
        for _ in 0..2 {
            let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
                if let Some(p) = search.ui(ui, actions) {
                    picked = Some(p);
                }
            });
        }
        picked
    }

    #[test]
    fn asks_once_per_query_and_keeps_only_current_results() {
        let ctx = egui::Context::default();
        let mut search = ObjectSearch::default();
        let mut actions = Vec::new();
        assert!(run(&ctx, &mut search, &mut actions).is_none());
        assert!(actions.is_empty(), "no query, no search");

        search.query = "m11".into();
        run(&ctx, &mut search, &mut actions);
        assert_eq!(actions, [ToEngine::FindObjects { query: "m11".into() }]);

        // Typing on: a late answer to the old query is ignored.
        search.query = "m110 ".into();
        run(&ctx, &mut search, &mut actions);
        assert_eq!(actions.len(), 2);
        search.on_results("m11", &[]);
        assert!(search.answered.is_none());
        search.on_results("m110", &[m110()]);
        assert_eq!(search.answered.as_deref(), Some("m110"));
        // 8.15 as f32 is just below 8.15.
        assert_eq!(label(&search.results[0]), "M 110 (NGC 205): Galaxy, mag 8.1");
    }
}
