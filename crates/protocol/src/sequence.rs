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
    /// Identifies the step while it is edited and run (assigned by the
    /// engine; 0 = not yet).
    pub id: u64,
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
    /// Frames already taken; a run continues from here.
    pub done: u32,
}

impl SequenceStep {
    /// Frames still to take.
    pub fn remaining(&self) -> u32 {
        if self.enabled { self.count.saturating_sub(self.done) } else { 0 }
    }
}

impl Default for SequenceStep {
    fn default() -> Self {
        Self {
            id: 0,
            enabled: true,
            frame_type: FrameType::Light,
            filter_slot: None,
            exposure: Exposure::Fixed { seconds: 60.0 },
            count: 10,
            binning: None,
            gain: None,
            offset: None,
            temperature: None,
            done: 0,
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
    /// Corrections the lights get, deciding which masters are needed.
    pub calibration: crate::masters::CalibrationPlan,
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
            calibration: Default::default(),
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

    pub fn remaining_frames(&self) -> u32 {
        self.steps.iter().map(SequenceStep::remaining).sum()
    }

    /// Exposure time still to take, of steps with a known duration.
    pub fn remaining_seconds(&self) -> f64 {
        self.steps
            .iter()
            .filter_map(|s| s.exposure.seconds().map(|e| e * s.remaining() as f64))
            .sum()
    }

    pub fn has_progress(&self) -> bool {
        self.steps.iter().any(|s| s.done > 0)
    }

    pub fn reset_progress(&mut self) {
        for s in &mut self.steps {
            s.done = 0;
        }
    }

    /// Gives steps without one (or sharing one) a fresh id from `next`.
    pub fn assign_step_ids(&mut self, next: &mut u64) {
        let mut seen = std::collections::HashSet::new();
        for s in &mut self.steps {
            if s.id == 0 || !seen.insert(s.id) {
                *next += 1;
                s.id = *next;
                seen.insert(s.id);
            }
        }
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

/// Light frames planned as total time per filter; turned into steps.
pub mod plan {
    use super::*;

    /// One filter (or one exposure of a filter) in the plan.
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    #[serde(default)]
    pub struct PlanRow {
        pub enabled: bool,
        pub filter_slot: Option<u32>,
        pub exposure: f64,
        /// Total exposure time wanted, in minutes.
        pub minutes: f64,
    }

    impl Default for PlanRow {
        fn default() -> Self {
            Self {
                enabled: false,
                filter_slot: None,
                exposure: 120.0,
                minutes: 60.0,
            }
        }
    }

    impl PlanRow {
        /// Frames needed to reach the planned time (rounded up).
        pub fn frames(&self) -> u32 {
            if !self.enabled || self.exposure <= 0.0 {
                return 0;
            }
            // Tolerate float noise: 60 min of 120 s is 30 frames, not 31.
            (self.minutes * 60.0 / self.exposure - 1e-9).ceil().max(0.0) as u32
        }
    }

    /// Camera settings shared by all rows.
    #[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
    #[serde(default)]
    pub struct PlanSettings {
        pub binning: Option<u32>,
        pub gain: Option<f64>,
        pub offset: Option<f64>,
        pub temperature: Option<f64>,
    }

    /// Steps for the plan: one light step per enabled row with frames.
    pub fn steps(rows: &[PlanRow], settings: &PlanSettings) -> Vec<SequenceStep> {
        rows.iter()
            .filter(|r| r.frames() > 0)
            .map(|r| SequenceStep {
                frame_type: FrameType::Light,
                filter_slot: r.filter_slot,
                exposure: Exposure::Fixed { seconds: r.exposure },
                count: r.frames(),
                binning: settings.binning,
                gain: settings.gain,
                offset: settings.offset,
                temperature: settings.temperature,
                ..Default::default()
            })
            .collect()
    }

    /// Same frames: progress on one counts for the other.
    fn same_frames(a: &SequenceStep, b: &SequenceStep) -> bool {
        a.frame_type == b.frame_type
            && a.filter_slot == b.filter_slot
            && a.exposure == b.exposure
            && a.binning == b.binning
            && a.gain == b.gain
            && a.offset == b.offset
    }

    /// Puts the plan into `seq`. `replace` swaps out all light steps (in
    /// place of the first one); frames already taken carry over to the new
    /// step with the same settings, which then plans at least that many.
    /// Otherwise the plan's steps are added at the end.
    pub fn apply(seq: &mut Sequence, rows: &[PlanRow], settings: &PlanSettings, replace: bool) {
        let mut new = steps(rows, settings);
        if !replace {
            seq.steps.extend(new);
            return;
        }
        let at = seq
            .steps
            .iter()
            .position(|s| s.frame_type == FrameType::Light)
            .unwrap_or(seq.steps.len());
        let mut old: Vec<SequenceStep> = Vec::new();
        seq.steps.retain(|s| {
            let light = s.frame_type == FrameType::Light;
            if light {
                old.push(s.clone());
            }
            !light
        });
        for step in &mut new {
            if let Some(i) = old.iter().position(|o| same_frames(o, step)) {
                let o = old.remove(i);
                // Keeping the id lets a running sequence keep counting here.
                step.id = o.id;
                step.done = o.done;
                step.count = step.count.max(o.done);
            }
        }
        seq.steps.splice(at..at, new);
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
        assert_eq!(find_filter(&names(&["L", "R", "G"]), LRGB[3].1), None);
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
        let step = |seconds, count, done| SequenceStep {
            exposure: Exposure::Fixed { seconds },
            count,
            done,
            ..Default::default()
        };
        let mut seq = Sequence {
            steps: vec![step(10.0, 3, 1), step(30.0, 3, 5)],
            ..Default::default()
        };
        assert_eq!(seq.planned_frames(), 6);
        assert_eq!(seq.planned_seconds(), 120.0);
        assert_eq!(seq.remaining_frames(), 2, "over-done steps count as complete");
        assert_eq!(seq.remaining_seconds(), 20.0);
        seq.steps[0].enabled = false;
        assert_eq!(seq.remaining_frames(), 0);
        assert!(seq.has_progress());
        seq.reset_progress();
        assert!(!seq.has_progress());
    }

    #[test]
    fn step_ids_are_unique() {
        let mut seq = Sequence {
            steps: vec![
                SequenceStep { id: 7, ..Default::default() },
                SequenceStep::default(),
                SequenceStep { id: 7, ..Default::default() },
            ],
            ..Default::default()
        };
        let mut next = 100;
        seq.assign_step_ids(&mut next);
        let ids: Vec<u64> = seq.steps.iter().map(|s| s.id).collect();
        assert_eq!(ids, vec![7, 101, 102], "the first of duplicates keeps its id");
        seq.assign_step_ids(&mut next);
        assert_eq!(next, 102, "nothing to do the second time");
    }

    mod planning {
        use super::super::plan::*;
        use super::super::*;

        fn row(filter: u32, exposure: f64, minutes: f64) -> PlanRow {
            PlanRow {
                enabled: true,
                filter_slot: Some(filter),
                exposure,
                minutes,
            }
        }

        #[test]
        fn frames_from_time() {
            assert_eq!(row(1, 120.0, 60.0).frames(), 30);
            assert_eq!(row(1, 180.0, 60.0).frames(), 20);
            assert_eq!(row(1, 300.0, 60.0).frames(), 12);
            assert_eq!(row(1, 7.0, 1.0).frames(), 9, "rounds up to reach the time");
            assert_eq!(PlanRow { enabled: false, ..row(1, 60.0, 60.0) }.frames(), 0);
        }

        #[test]
        fn plan_becomes_light_steps() {
            let settings = PlanSettings {
                gain: Some(100.0),
                temperature: Some(-10.0),
                ..Default::default()
            };
            let rows = [row(1, 60.0, 120.0), row(2, 120.0, 60.0), row(3, 120.0, 0.0)];
            let s = steps(&rows, &settings);
            assert_eq!(s.len(), 2, "rows without frames are left out");
            assert_eq!((s[0].filter_slot, s[0].count), (Some(1), 120));
            assert_eq!((s[1].filter_slot, s[1].count), (Some(2), 30));
            assert!(s.iter().all(|s| s.gain == Some(100.0) && s.temperature == Some(-10.0)));
        }

        #[test]
        fn add_appends() {
            let mut seq = Sequence {
                steps: vec![SequenceStep::default()],
                ..Default::default()
            };
            apply(&mut seq, &[row(1, 60.0, 60.0)], &PlanSettings::default(), false);
            assert_eq!(seq.steps.len(), 2);
            assert_eq!(seq.steps[1].count, 60);
        }

        #[test]
        fn replace_keeps_progress_and_other_frames() {
            let settings = PlanSettings::default();
            let mut seq = Sequence::default();
            seq.steps.push(super::super::presets::bias(10, None));
            apply(&mut seq, &[row(1, 60.0, 60.0), row(2, 120.0, 60.0)], &settings, true);
            assert_eq!(seq.steps.len(), 3, "added after the bias");
            seq.steps[1].id = 11;
            seq.steps[1].done = 40;
            seq.steps[2].id = 12;
            seq.steps[2].done = 30;

            // More L, R dropped to half an hour, B new.
            let rows = [row(1, 60.0, 90.0), row(2, 120.0, 30.0), row(3, 120.0, 60.0)];
            apply(&mut seq, &rows, &settings, true);
            let s = &seq.steps;
            assert_eq!(s.len(), 4);
            assert_eq!(s[0].frame_type, FrameType::Bias, "non-light steps stay");
            assert_eq!((s[1].id, s[1].done, s[1].count), (11, 40, 90));
            assert_eq!((s[2].id, s[2].done, s[2].count), (12, 30, 30), "never plans fewer than taken");
            assert_eq!((s[3].id, s[3].done, s[3].count), (0, 0, 30));

            // A different exposure is different frames: no progress carried.
            apply(&mut seq, &[row(1, 30.0, 90.0)], &settings, true);
            assert_eq!(seq.steps.len(), 2);
            assert_eq!((seq.steps[1].done, seq.steps[1].count), (0, 180));
        }

        #[test]
        fn replace_puts_lights_where_they_were() {
            let mut seq = Sequence::default();
            apply(&mut seq, &[row(1, 60.0, 60.0)], &PlanSettings::default(), true);
            seq.steps.extend(super::super::presets::flats(&[Some(1)], 20));
            apply(&mut seq, &[row(1, 60.0, 60.0), row(2, 60.0, 60.0)], &PlanSettings::default(), true);
            let types: Vec<FrameType> = seq.steps.iter().map(|s| s.frame_type).collect();
            assert_eq!(types, vec![FrameType::Light, FrameType::Light, FrameType::Flat]);
        }
    }
}
