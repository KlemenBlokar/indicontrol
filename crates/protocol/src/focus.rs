//! Focuser control and autofocus.

use serde::{Deserialize, Serialize};

pub use guiding::StarStats;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FocusSettings {
    /// Focuser steps between samples.
    pub step: u32,
    /// Samples on each side of the start position (2n + 1 in total).
    pub steps_out: u32,
    pub exposure: f64,
    pub binning: Option<u32>,
    pub gain: Option<f64>,
    /// Filter for autofocus frames when run by hand (`None` keeps the current one).
    pub filter_slot: Option<u32>,
    /// Overshoot (steps) so the final approach is always inward.
    pub backlash: u32,
    /// Star measurement aperture radius (px); larger for very defocused stars.
    pub aperture: f64,
    pub min_stars: usize,
    /// Times the sampled range may move when focus lies outside it.
    pub max_shifts: u32,
}

impl Default for FocusSettings {
    fn default() -> Self {
        Self {
            step: 100,
            steps_out: 4,
            exposure: 3.0,
            binning: None,
            gain: None,
            filter_slot: None,
            backlash: 0,
            aperture: 15.0,
            min_stars: 3,
            max_shifts: 4,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AutofocusStatus {
    pub running: bool,
    pub message: String,
    /// Measured (position, HFR px).
    pub points: Vec<(f64, f64)>,
    /// Fitted curve for plotting.
    pub curve: Vec<(f64, f64)>,
    pub best: Option<f64>,
}

/// When a sequence refocuses.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[derive(Default)]
pub struct AutofocusTriggers {
    pub on_start: bool,
    /// Every N light frames (0 = never).
    pub every_frames: u32,
    pub on_filter_change: bool,
    /// When the focuser temperature has changed this much (°C).
    pub temperature_delta: Option<f64>,
    pub after_flip: bool,
}

