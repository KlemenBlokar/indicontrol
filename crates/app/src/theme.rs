//! Day/night colours. At night every accent colour becomes a red of similar
//! brightness and images are tinted red, so nothing on screen spoils dark
//! adaptation.

use eframe::egui::{self, Color32, Id};

fn key() -> Id {
    Id::new("indicontrol_night_mode")
}

pub fn set_night(ctx: &egui::Context, night: bool) {
    ctx.data_mut(|d| d.insert_temp(key(), night));
}

pub fn is_night(ctx: &egui::Context) -> bool {
    ctx.data(|d| d.get_temp::<bool>(key())).unwrap_or(false)
}

/// Red of similar brightness (night) or the colour unchanged (day).
pub fn night_red(color: Color32) -> Color32 {
    let lum = 0.299 * color.r() as f32 + 0.587 * color.g() as f32 + 0.114 * color.b() as f32;
    let r = (60.0 + lum * 0.75).min(230.0) as u8;
    Color32::from_rgba_unmultiplied(r, 0, 0, color.a())
}

/// Accent colour for the current mode.
pub fn c(ui: &egui::Ui, color: Color32) -> Color32 {
    if is_night(ui.ctx()) { night_red(color) } else { color }
}

/// Tint for images: neutral by day, red at night.
pub fn image_tint(ui: &egui::Ui) -> Color32 {
    if is_night(ui.ctx()) {
        Color32::from_rgb(210, 0, 0)
    } else {
        Color32::WHITE
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn night_colours_are_red() {
        for c in [Color32::from_rgb(60, 200, 90), Color32::from_rgb(80, 140, 240), Color32::WHITE, Color32::BLACK] {
            let r = night_red(c);
            assert_eq!((r.g(), r.b()), (0, 0), "{c:?}");
            assert!(r.r() >= 60);
        }
        // Brighter colours stay brighter.
        assert!(night_red(Color32::WHITE).r() > night_red(Color32::from_rgb(80, 80, 80)).r());
    }
}
