//! Imaging sequences: definitions, presets and run status.

use serde::{Deserialize, Serialize};

use crate::FrameType;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "mode")]
pub enum Exposure {
    Fixed { seconds: f64 },
    /// Adjust the exposure until the median reaches `target` (fraction of
    /// full scale, ± `tolerance`), staying within `min..=max` seconds.
    AutoFlat {
        target: f64,
        tolerance: f64,
        min: f64,
        max: f64,
    },
    /// Same exposure as the flats last taken with this filter, binning and gain
    /// (for dark flats).
    MatchFlat,
    /// Shortest exposure the camera supports (for bias frames).
    Minimum,
}

impl Exposure {
    pub const DEFAULT_FLAT: Exposure = Exposure::AutoFlat {
        target: 0.45,
        tolerance: 0.05,
        min: 0.01,
        max: 30.0,
    };

    /// Known duration in seconds, if not decided at run time.
    pub fn seconds(&self) -> Option<f64> {
        match self {
            Exposure::Fixed { seconds } => Some(*seconds),
            Exposure::Minimum => Some(0.0),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SequenceStep {
    pub enabled: bool,
    pub frame_type: FrameType,
    /// 1-based slot; `None` leaves the wheel where it is.
    pub filter_slot: Option<u32>,
    pub exposure: Exposure,
    pub count: u32,
    pub binning: Option<u32>,
    pub gain: Option<f64>,
    pub offset: Option<f64>,
    /// Wait for the sensor to reach this temperature (°C) before the step.
    pub temperature: Option<f64>,
}

impl Default for SequenceStep {
    fn default() -> Self {
        Self {
            enabled: true,
            frame_type: FrameType::Light,
            filter_slot: None,
            exposure: Exposure::Fixed { seconds: 60.0 },
            count: 10,
            binning: None,
            gain: None,
            offset: None,
            temperature: None,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum SequenceOrder {
    /// All frames of a step, then the next step.
    #[default]
    ByStep,
    /// One frame of each step in turn (LRGBLRGB…).
    Interleaved,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Sequence {
    pub name: String,
    pub object: String,
    pub order: SequenceOrder,
    pub steps: Vec<SequenceStep>,
    /// Dither after every N light frames while guiding (0 = never).
    pub dither_every: u32,
    /// Flip the mount when the target passes the meridian.
    pub meridian_flip: bool,
    /// Minutes past the meridian at which to flip.
    pub flip_minutes_past: f64,
    /// Target to slew to and centre (plate solving).
    pub target: Option<crate::Target>,
    /// Slew to and centre the target before the first frame.
    pub center_on_start: bool,
    /// Re-centre the target after a meridian flip.
    pub center_after_flip: bool,
    pub autofocus: crate::AutofocusTriggers,
}

impl Default for Sequence {
    fn default() -> Self {
        Self {
            name: String::new(),
            object: String::new(),
            order: SequenceOrder::ByStep,
            steps: Vec::new(),
            dither_every: 3,
            meridian_flip: true,
            flip_minutes_past: 5.0,
            target: None,
            center_on_start: true,
            center_after_flip: true,
            autofocus: Default::default(),
        }
    }
}

impl Sequence {
    pub fn planned_frames(&self) -> u32 {
        self.steps.iter().filter(|s| s.enabled).map(|s| s.count).sum()
    }

    /// Total exposure time of steps with a known duration.
    pub fn planned_seconds(&self) -> f64 {
        self.steps
            .iter()
            .filter(|s| s.enabled)
            .filter_map(|s| s.exposure.seconds().map(|e| e * s.count as f64))
            .sum()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RunState {
    Running,
    /// Will pause after the current frame.
    Pausing,
    Paused,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SequenceStatus {
    pub sequence: Sequence,
    pub state: RunState,
    pub current_step: Option<usize>,
    /// Frames completed per step.
    pub done: Vec<u32>,
    /// What the sequencer is doing right now.
    pub message: String,
}

impl SequenceStatus {
    pub fn frames_done(&self) -> u32 {
        self.done.iter().sum()
    }
}

/// Flat exposure found by auto-exposure, reused for dark flats.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FlatRecord {
    pub filter: Option<String>,
    pub binning: u32,
    pub gain: Option<i64>,
    pub exposure: f64,
}

/// Ready-made step lists. Filters are found by name in the wheel.
pub mod presets {
    use super::*;

    pub const LRGB: [(&str, &[&str]); 4] = [
        ("L", &["l", "lum", "luminance", "clear"]),
        ("R", &["r", "red"]),
        ("G", &["g", "green"]),
        ("B", &["b", "blue"]),
    ];
    pub const SHO: [(&str, &[&str]); 3] = [
        ("SII", &["sii", "s2", "s-ii", "sulfur", "sulphur"]),
        ("Ha", &["ha", "h-alpha", "halpha", "h_alpha", "hα"]),
        ("OIII", &["oiii", "o3", "o-iii", "oxygen"]),
    ];

    fn normalize(s: &str) -> String {
        s.chars()
            .filter(|c| c.is_alphanumeric() || *c == 'α')
            .flat_map(char::to_lowercase)
            .collect()
    }

    /// 1-based slot of the first filter whose name matches an alias.
    pub fn find_filter(names: &[String], aliases: &[&str]) -> Option<u32> {
        let aliases: Vec<String> = aliases.iter().map(|a| normalize(a)).collect();
        names
            .iter()
            .position(|n| aliases.contains(&normalize(n)))
            .map(|i| i as u32 + 1)
    }

    /// Light steps for a filter set; also returns the filters not found.
    pub fn filter_set(
        names: &[String],
        set: &[(&'static str, &[&str])],
        seconds: f64,
        count: u32,
    ) -> (Vec<SequenceStep>, Vec<&'static str>) {
        let mut steps = Vec::new();
        let mut missing = Vec::new();
        for (label, aliases) in set {
            match find_filter(names, aliases) {
                Some(slot) => steps.push(SequenceStep {
                    filter_slot: Some(slot),
                    exposure: Exposure::Fixed { seconds },
                    count,
                    ..Default::default()
                }),
                None => missing.push(*label),
            }
        }
        (steps, missing)
    }

    pub fn bracketing(filter_slot: Option<u32>, exposures: &[f64], count: u32) -> Vec<SequenceStep> {
        exposures
            .iter()
            .map(|&seconds| SequenceStep {
                filter_slot,
                exposure: Exposure::Fixed { seconds },
                count,
                ..Default::default()
            })
            .collect()
    }

    pub fn darks(exposures: &[f64], count: u32, temperature: Option<f64>) -> Vec<SequenceStep> {
        exposures
            .iter()
            .map(|&seconds| SequenceStep {
                frame_type: FrameType::Dark,
                exposure: Exposure::Fixed { seconds },
                count,
                temperature,
                ..Default::default()
            })
            .collect()
    }

    pub fn bias(count: u32, temperature: Option<f64>) -> SequenceStep {
        SequenceStep {
            frame_type: FrameType::Bias,
            exposure: Exposure::Minimum,
            count,
            temperature,
            ..Default::default()
        }
    }

    pub fn flats(filter_slots: &[Option<u32>], count: u32) -> Vec<SequenceStep> {
        filter_slots
            .iter()
            .map(|&filter_slot| SequenceStep {
                frame_type: FrameType::Flat,
                filter_slot,
                exposure: Exposure::DEFAULT_FLAT,
                count,
                ..Default::default()
            })
            .collect()
    }

    pub fn dark_flats(filter_slots: &[Option<u32>], count: u32) -> Vec<SequenceStep> {
        filter_slots
            .iter()
            .map(|&filter_slot| SequenceStep {
                frame_type: FrameType::Dark,
                filter_slot,
                exposure: Exposure::MatchFlat,
                count,
                ..Default::default()
            })
            .collect()
    }

    /// Parse "10, 30 60;120" into seconds.
    pub fn parse_exposures(s: &str) -> Result<Vec<f64>, String> {
        s.split(|c: char| c == ',' || c == ';' || c.is_whitespace())
            .filter(|p| !p.is_empty())
            .map(|p| match p.trim_end_matches('s').parse::<f64>() {
                Ok(v) if v >= 0.0 && v.is_finite() => Ok(v),
                _ => Err(format!("'{p}' is not an exposure time")),
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::presets::*;
    use super::*;

    fn names(n: &[&str]) -> Vec<String> {
        n.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn finds_filters_by_alias() {
        let wheel = names(&["Luminance", "Red", "Green", "Blue", "H-Alpha", "O III", "S2"]);
        assert_eq!(find_filter(&wheel, &["l", "lum", "luminance"]), Some(1));
        assert_eq!(find_filter(&wheel, LRGB[3].1), Some(4));
        assert_eq!(find_filter(&wheel, SHO[1].1), Some(5));
        assert_eq!(find_filter(&wheel, SHO[2].1), Some(6));
        assert_eq!(find_filter(&wheel, SHO[0].1), Some(7));

        let (steps, missing) = filter_set(&names(&["L", "R", "G"]), &LRGB, 120.0, 20);
        assert_eq!(steps.len(), 3);
        assert_eq!(missing, vec!["B"]);
        assert_eq!(steps[2].filter_slot, Some(3));
        assert_eq!(steps[0].exposure, Exposure::Fixed { seconds: 120.0 });
    }

    #[test]
    fn calibration_presets() {
        let d = darks(&[60.0, 300.0], 30, Some(-10.0));
        assert_eq!(d.len(), 2);
        assert!(d.iter().all(|s| s.frame_type == FrameType::Dark && s.temperature == Some(-10.0)));
        assert_eq!(bias(50, None).exposure, Exposure::Minimum);
        let f = flats(&[Some(1), Some(2)], 25);
        assert!(matches!(f[0].exposure, Exposure::AutoFlat { .. }));
        assert_eq!(dark_flats(&[Some(2)], 25)[0].exposure, Exposure::MatchFlat);
    }

    #[test]
    fn exposure_lists_and_totals() {
        assert_eq!(parse_exposures("10, 30 60;120s").unwrap(), vec![10.0, 30.0, 60.0, 120.0]);
        assert!(parse_exposures("10, x").is_err());
        let seq = Sequence {
            steps: bracketing(Some(1), &[10.0, 30.0], 3),
            ..Default::default()
        };
        assert_eq!(seq.planned_frames(), 6);
        assert_eq!(seq.planned_seconds(), 120.0);
    }
}
