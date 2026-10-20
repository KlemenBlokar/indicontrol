//! Calibration masters (dark, flat, bias) found in the engine's masters
//! folder, and which of them suit a light step.

use serde::{Deserialize, Serialize};

use crate::equipment::{self, Control};
use crate::{Exposure, FrameType, Sequence, SequenceStep, UiState};

/// Exposure match: within this many seconds, or this fraction.
const EXPOSURE_TOLERANCE: (f64, f64) = (0.01, 0.005);
/// Sensor temperature match, °C.
pub const TEMPERATURE_TOLERANCE: f64 = 2.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MasterKind {
    Dark,
    Flat,
    Bias,
}

impl MasterKind {
    pub const ALL: [MasterKind; 3] = [MasterKind::Dark, MasterKind::Flat, MasterKind::Bias];

    pub fn label(self) -> &'static str {
        match self {
            MasterKind::Dark => "dark",
            MasterKind::Flat => "flat",
            MasterKind::Bias => "bias",
        }
    }

    /// From IMAGETYP/FRAME or a file name: "Dark Frame", "master-flat",
    /// "bias_stacked", "offset" (Siril's older name for bias).
    pub fn recognise(text: &str) -> Option<MasterKind> {
        let t = text.to_lowercase();
        if t.contains("light") {
            return None;
        }
        if t.contains("bias") || t.contains("offset") {
            Some(MasterKind::Bias)
        } else if t.contains("flat") && !t.contains("dark") {
            Some(MasterKind::Flat)
        } else if t.contains("dark") {
            // Dark flats too: darks, matched by the flats' exposure.
            Some(MasterKind::Dark)
        } else {
            None
        }
    }
}

/// A master file and the header values that decide what it suits.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MasterFrame {
    /// Relative to the masters folder.
    pub path: String,
    pub kind: MasterKind,
    pub camera: Option<String>,
    pub width: u32,
    pub height: u32,
    pub binning: Option<u32>,
    pub exposure: Option<f64>,
    pub gain: Option<f64>,
    pub offset: Option<f64>,
    pub temperature: Option<f64>,
    pub filter: Option<String>,
    /// Frames averaged into it (STACKCNT/NCOMBINE), if known.
    pub frames: Option<u32>,
    /// DATE-OBS, else the file's modification time (ISO 8601, UTC).
    pub date: String,
}

/// Which corrections a sequence's lights get, deciding the masters needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CalibrationPlan {
    pub darks: bool,
    pub flats: bool,
    pub flats_with: FlatCalibration,
}

impl Default for CalibrationPlan {
    fn default() -> Self {
        Self {
            darks: true,
            flats: true,
            flats_with: FlatCalibration::Bias,
        }
    }
}

/// What is subtracted from the flats before they are averaged.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum FlatCalibration {
    #[default]
    Bias,
    /// Darks with the flats' exposure.
    DarkFlats,
}

/// Why a master is needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Purpose {
    /// Darks for the lights, or bias when they get no darks.
    Lights,
    /// Bias or dark flats to calibrate the flats.
    Flats,
}

/// One master the sequence needs, and the one that suits it, if any.
#[derive(Debug, Clone, PartialEq)]
pub struct Need {
    pub kind: MasterKind,
    pub purpose: Purpose,
    /// What it must match; `None` while that is not known yet (dark flats
    /// before there is a flat to take the exposure from).
    pub spec: Option<LightSpec>,
    /// "120 s, gain 100, -10 °C, bin 1".
    pub what: String,
    pub found: Option<MasterFrame>,
    /// The (first) light step needing it.
    pub light: SequenceStep,
}

impl Need {
    /// Values the found master's header does not have, so it could not be
    /// checked against them.
    pub fn unverified(&self) -> Vec<&'static str> {
        match (&self.spec, &self.found) {
            (Some(spec), Some(m)) => spec.unknowns(self.kind, m),
            _ => Vec::new(),
        }
    }

    /// The step that takes `frames` frames for this master, or `None` while
    /// what it must match is not known.
    pub fn capture_step(&self, frames: u32) -> Option<SequenceStep> {
        let spec = self.spec.as_ref()?;
        let light = &self.light;
        let base = SequenceStep {
            count: frames,
            binning: light.binning,
            gain: spec.gain,
            offset: spec.offset,
            ..Default::default()
        };
        Some(match (self.kind, self.purpose) {
            (MasterKind::Dark, Purpose::Lights) => SequenceStep {
                frame_type: FrameType::Dark,
                exposure: Exposure::Fixed { seconds: spec.exposure },
                temperature: light.temperature,
                ..base
            },
            // Dark flats: the flat's exposure and gain, at whatever
            // temperature the flats had.
            (MasterKind::Dark, Purpose::Flats) => SequenceStep {
                frame_type: FrameType::Dark,
                exposure: Exposure::Fixed { seconds: spec.exposure },
                ..base
            },
            (MasterKind::Bias, purpose) => SequenceStep {
                frame_type: FrameType::Bias,
                exposure: Exposure::Minimum,
                temperature: if purpose == Purpose::Lights { light.temperature } else { None },
                ..base
            },
            (MasterKind::Flat, _) => SequenceStep {
                frame_type: FrameType::Flat,
                exposure: Exposure::DEFAULT_FLAT,
                filter_slot: light.filter_slot,
                gain: light.gain,
                offset: light.offset,
                ..base
            },
        })
    }

    /// What to do before taking the frames.
    pub fn setup_hint(&self) -> &'static str {
        match self.kind {
            MasterKind::Flat => "Put the flat panel on (or point at an even sky) before starting.",
            _ => "Cover the telescope (lid on, no light leaks) before starting.",
        }
    }
}

/// The masters `seq`'s enabled light steps need under its calibration plan,
/// each once, with the best suitable master from `masters`.
pub fn needs(seq: &Sequence, state: &UiState) -> Vec<Need> {
    let masters = &state.masters.masters;
    let plan = seq.calibration;
    let mut out: Vec<Need> = Vec::new();
    let mut add = |kind, purpose, spec: Option<LightSpec>, what: String, light: &SequenceStep| {
        if out.iter().any(|n| n.kind == kind && n.purpose == purpose && n.what == what) {
            return;
        }
        let found = spec.as_ref().and_then(|s| s.find(kind, masters)).cloned();
        out.push(Need { kind, purpose, spec, what, found, light: light.clone() });
    };
    for step in seq.steps.iter().filter(|s| s.enabled && s.count > 0) {
        let Some(light) = LightSpec::for_step(step, state) else {
            continue;
        };
        if plan.darks {
            add(MasterKind::Dark, Purpose::Lights, Some(light.clone()), light.describe(MasterKind::Dark), step);
        } else {
            // Without darks the lights still need their offset removed.
            add(MasterKind::Bias, Purpose::Lights, Some(light.clone()), light.describe(MasterKind::Bias), step);
        }
        if !plan.flats {
            continue;
        }
        add(MasterKind::Flat, Purpose::Flats, Some(light.clone()), light.describe(MasterKind::Flat), step);
        // The flats' own calibration follows the flat's settings.
        let flat = light.find(MasterKind::Flat, masters);
        let flat_spec = LightSpec {
            gain: flat.and_then(|f| f.gain).or(light.gain),
            offset: flat.and_then(|f| f.offset).or(light.offset),
            temperature: None,
            ..light.clone()
        };
        match plan.flats_with {
            FlatCalibration::Bias => {
                add(MasterKind::Bias, Purpose::Flats, Some(flat_spec.clone()), flat_spec.describe(MasterKind::Bias), step)
            }
            FlatCalibration::DarkFlats => {
                let filter = light.filter.clone().unwrap_or_else(|| "no".into());
                match flat.and_then(|f| f.exposure) {
                    Some(exposure) => {
                        let spec = LightSpec { exposure, ..flat_spec };
                        let what = format!("{} (for the {filter} flat)", spec.describe(MasterKind::Dark));
                        add(MasterKind::Dark, Purpose::Flats, Some(spec), what, step);
                    }
                    None => {
                        let what = match flat {
                            Some(f) => format!("exposure of the {filter} flat (not in {})", f.path),
                            None => format!("exposure of the {filter} flat, once there is one"),
                        };
                        add(MasterKind::Dark, Purpose::Flats, None, what, step)
                    }
                }
            }
        }
    }
    // Grouped: the lights' own, the flats, then the flats' calibration.
    out.sort_by_key(|n| match (n.purpose, n.kind) {
        (Purpose::Lights, _) => 0,
        (_, MasterKind::Flat) => 1,
        _ => 2,
    });
    out
}

/// "120", "1.5", "0.11": no float noise.
fn seconds(s: f64) -> String {
    let text = if s >= 1.0 { format!("{s:.2}") } else { format!("{s:.3}") };
    text.trim_end_matches('0').trim_end_matches('.').to_owned()
}

/// The masters folder as last read by the engine.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MastersStatus {
    /// Absolute path on the engine's machine.
    pub folder: String,
    pub masters: Vec<MasterFrame>,
    /// FITS files that are not masters (or not readable), with the reason.
    pub skipped: Vec<(String, String)>,
    /// The folder itself could not be read.
    pub error: Option<String>,
    /// A master being made: "Stacking 20 dark frames".
    pub creating: Option<String>,
}

/// What a light step's masters must match.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LightSpec {
    pub camera: Option<String>,
    /// Frame size at this binning, if the camera reports its sensor.
    pub size: Option<(u32, u32)>,
    pub binning: u32,
    pub exposure: f64,
    pub gain: Option<f64>,
    pub offset: Option<f64>,
    pub temperature: Option<f64>,
    pub filter: Option<String>,
}

impl LightSpec {
    /// The step as it will be taken with the main camera and wheel in
    /// `state`: settings the step leaves alone are the camera's current ones.
    /// `None` for steps that are not lights with a fixed exposure.
    pub fn for_step(step: &SequenceStep, state: &UiState) -> Option<LightSpec> {
        if step.frame_type != FrameType::Light {
            return None;
        }
        let exposure = step.exposure.seconds()?;
        let camera_name = state.roles.main_camera.clone();
        let camera = camera_name.as_ref().and_then(|c| state.cache.devices.get(c));
        let current = |control| camera.and_then(|c| equipment::control_value(c, control)).map(|v| v.value);
        let binning = step
            .binning
            .or_else(|| camera.and_then(|c| equipment::number(c, "CCD_BINNING", "HOR_BIN")).map(|v| v.value as u32))
            .unwrap_or(1)
            .max(1);
        let size = camera.and_then(|c| {
            let w = equipment::number(c, "CCD_INFO", "CCD_MAX_X")?.value as u32;
            let h = equipment::number(c, "CCD_INFO", "CCD_MAX_Y")?.value as u32;
            (w > 0 && h > 0).then_some((w / binning, h / binning))
        });
        let wheel = state.roles.filter_wheel.as_ref().and_then(|w| state.cache.devices.get(w));
        let filter = wheel.and_then(|w| {
            let slot = step.filter_slot.or_else(|| equipment::current_filter_slot(w))?;
            equipment::filter_name(w, slot)
        });
        Some(LightSpec {
            camera: camera_name,
            size,
            binning,
            exposure,
            gain: step.gain.or_else(|| current(Control::Gain)),
            offset: step.offset.or_else(|| current(Control::Offset)),
            temperature: step.temperature,
            filter,
        })
    }

    /// Why `m` does not suit these lights as a `kind` master, if it does not.
    pub fn mismatch(&self, kind: MasterKind, m: &MasterFrame) -> Option<String> {
        if m.kind != kind {
            return Some(format!("it is a {}", m.kind.label()));
        }
        if let (Some(a), Some(b)) = (&self.camera, &m.camera)
            && !a.eq_ignore_ascii_case(b)
        {
            return Some(format!("taken with {b}"));
        }
        let bin = m.binning.unwrap_or(1);
        if bin != self.binning {
            return Some(format!("binned {bin}×{bin}"));
        }
        if let Some((w, h)) = self.size
            && (m.width, m.height) != (w, h)
        {
            return Some(format!("{}×{} pixels, lights are {w}×{h}", m.width, m.height));
        }
        let differs = |a: Option<f64>, b: Option<f64>| matches!((a, b), (Some(a), Some(b)) if (a - b).abs() >= 0.5);
        let gain_offset = || {
            if differs(self.gain, m.gain) {
                return Some(format!("gain {}", m.gain.unwrap_or_default()));
            }
            if differs(self.offset, m.offset) {
                return Some(format!("offset {}", m.offset.unwrap_or_default()));
            }
            None
        };
        match kind {
            MasterKind::Dark => {
                let Some(e) = m.exposure else {
                    return Some("no exposure time in its header".into());
                };
                let tolerance = EXPOSURE_TOLERANCE.0.max(EXPOSURE_TOLERANCE.1 * self.exposure);
                if (e - self.exposure).abs() > tolerance {
                    return Some(format!("{e} s exposure"));
                }
                if let (Some(a), Some(b)) = (self.temperature, m.temperature)
                    && (a - b).abs() > TEMPERATURE_TOLERANCE
                {
                    return Some(format!("taken at {b:.1} °C"));
                }
                gain_offset()
            }
            MasterKind::Bias => gain_offset(),
            MasterKind::Flat => match (&self.filter, &m.filter) {
                (Some(a), Some(b)) if !same_filter(a, b) => Some(format!("{b} filter")),
                (Some(_), None) => Some("no filter in its header".into()),
                _ => None,
            },
        }
    }

    /// Values that matter for a `kind` master but are missing from `m`'s
    /// header (so `mismatch` could not check them).
    pub fn unknowns(&self, kind: MasterKind, m: &MasterFrame) -> Vec<&'static str> {
        let mut out = Vec::new();
        if self.camera.is_some() && m.camera.is_none() {
            out.push("camera");
        }
        if kind != MasterKind::Flat {
            if self.gain.is_some() && m.gain.is_none() {
                out.push("gain");
            }
            if self.offset.is_some() && m.offset.is_none() {
                out.push("offset");
            }
        }
        if kind == MasterKind::Dark && self.temperature.is_some() && m.temperature.is_none() {
            out.push("temperature");
        }
        out
    }

    /// The `kind` master that suits these lights: fully checked ones first,
    /// then the newest.
    pub fn find<'a>(&self, kind: MasterKind, masters: &'a [MasterFrame]) -> Option<&'a MasterFrame> {
        masters
            .iter()
            .filter(|m| self.mismatch(kind, m).is_none())
            .max_by(|a, b| {
                let unknown = |m| self.unknowns(kind, m).len();
                unknown(b).cmp(&unknown(a)).then(a.date.cmp(&b.date))
            })
    }

    /// "120 s, gain 100, −10 °C, bin 1" style description of what `kind` needs.
    pub fn describe(&self, kind: MasterKind) -> String {
        let mut parts = Vec::new();
        match kind {
            MasterKind::Dark => parts.push(format!("{} s", seconds(self.exposure))),
            MasterKind::Flat => parts.push(self.filter.clone().map_or("no filter".into(), |f| format!("{f} filter"))),
            MasterKind::Bias => {}
        }
        if kind != MasterKind::Flat {
            if let Some(g) = self.gain {
                parts.push(format!("gain {g}"));
            }
            if let Some(o) = self.offset {
                parts.push(format!("offset {o}"));
            }
        }
        if kind == MasterKind::Dark
            && let Some(t) = self.temperature
        {
            parts.push(format!("{t} °C"));
        }
        parts.push(format!("bin {}", self.binning));
        parts.join(", ")
    }
}

fn same_filter(a: &str, b: &str) -> bool {
    let norm = |s: &str| s.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect::<String>();
    norm(a) == norm(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn master(kind: MasterKind) -> MasterFrame {
        MasterFrame {
            path: "m.fits".into(),
            kind,
            camera: Some("ZWO ASI2600MM".into()),
            width: 6248,
            height: 4176,
            binning: Some(1),
            exposure: Some(120.0),
            gain: Some(100.0),
            offset: Some(50.0),
            temperature: Some(-10.2),
            filter: Some("Red".into()),
            frames: Some(30),
            date: "2026-10-01T20:00:00".into(),
        }
    }

    fn spec() -> LightSpec {
        LightSpec {
            camera: Some("ZWO ASI2600MM".into()),
            size: Some((6248, 4176)),
            binning: 1,
            exposure: 120.0,
            gain: Some(100.0),
            offset: Some(50.0),
            temperature: Some(-10.0),
            filter: Some("Red".into()),
        }
    }

    #[test]
    fn recognises_kinds() {
        use MasterKind::*;
        assert_eq!(MasterKind::recognise("Dark Frame"), Some(Dark));
        assert_eq!(MasterKind::recognise("master-dark_120s.fit"), Some(Dark));
        assert_eq!(MasterKind::recognise("pp_flat_stacked"), Some(Flat));
        assert_eq!(MasterKind::recognise("Bias Frame"), Some(Bias));
        assert_eq!(MasterKind::recognise("offset_stacked.fit"), Some(Bias));
        assert_eq!(MasterKind::recognise("darkflat_stacked"), Some(Dark));
        assert_eq!(MasterKind::recognise("Flat Field"), Some(Flat));
        assert_eq!(MasterKind::recognise("Light Frame"), None);
        assert_eq!(MasterKind::recognise("M31_stacked"), None);
    }

    #[test]
    fn darks_match_exposure_temperature_gain() {
        let s = spec();
        assert_eq!(s.mismatch(MasterKind::Dark, &master(MasterKind::Dark)), None);
        let m = MasterFrame { exposure: Some(300.0), ..master(MasterKind::Dark) };
        assert_eq!(s.mismatch(MasterKind::Dark, &m).unwrap(), "300 s exposure");
        let m = MasterFrame { temperature: Some(0.0), ..master(MasterKind::Dark) };
        assert!(s.mismatch(MasterKind::Dark, &m).unwrap().contains("0.0 °C"));
        let m = MasterFrame { gain: Some(0.0), ..master(MasterKind::Dark) };
        assert_eq!(s.mismatch(MasterKind::Dark, &m).unwrap(), "gain 0");
        // Unknown values on either side are not held against it.
        let m = MasterFrame { temperature: None, gain: None, camera: None, ..master(MasterKind::Dark) };
        assert_eq!(s.mismatch(MasterKind::Dark, &m), None);
        let m = MasterFrame { exposure: None, ..master(MasterKind::Dark) };
        assert!(s.mismatch(MasterKind::Dark, &m).is_some());
    }

    #[test]
    fn flats_match_filter_and_size() {
        let s = spec();
        assert_eq!(s.mismatch(MasterKind::Flat, &master(MasterKind::Flat)), None);
        let m = MasterFrame { exposure: Some(1.5), gain: Some(0.0), ..master(MasterKind::Flat) };
        assert_eq!(s.mismatch(MasterKind::Flat, &m), None, "flats need not match exposure or gain");
        let m = MasterFrame { filter: Some("green".into()), ..master(MasterKind::Flat) };
        assert_eq!(s.mismatch(MasterKind::Flat, &m).unwrap(), "green filter");
        let m = MasterFrame { filter: Some("RED".into()), ..master(MasterKind::Flat) };
        assert_eq!(s.mismatch(MasterKind::Flat, &m), None);
        let m = MasterFrame { binning: Some(2), width: 3124, height: 2088, ..master(MasterKind::Flat) };
        assert_eq!(s.mismatch(MasterKind::Flat, &m).unwrap(), "binned 2×2");
        let m = MasterFrame { width: 3000, ..master(MasterKind::Flat) };
        assert!(s.mismatch(MasterKind::Flat, &m).unwrap().contains("pixels"));
        let m = MasterFrame { camera: Some("Other".into()), ..master(MasterKind::Flat) };
        assert_eq!(s.mismatch(MasterKind::Flat, &m).unwrap(), "taken with Other");
    }

    #[test]
    fn picks_the_newest_suitable() {
        let s = spec();
        let masters = vec![
            master(MasterKind::Flat),
            MasterFrame { date: "2026-10-10T20:00:00".into(), path: "new.fits".into(), ..master(MasterKind::Flat) },
            MasterFrame { date: "2026-10-20T20:00:00".into(), filter: Some("Blue".into()), ..master(MasterKind::Flat) },
            master(MasterKind::Bias),
        ];
        assert_eq!(s.find(MasterKind::Flat, &masters).unwrap().path, "new.fits");
        assert!(s.find(MasterKind::Bias, &masters).is_some());
        assert!(s.find(MasterKind::Dark, &masters).is_none());
        assert_eq!(s.describe(MasterKind::Dark), "120 s, gain 100, offset 50, -10 °C, bin 1");
        assert_eq!(s.describe(MasterKind::Flat), "Red filter, bin 1");

        // A fully described master wins over a newer one missing values.
        let masters = vec![
            master(MasterKind::Bias),
            MasterFrame { date: "2026-10-20".into(), offset: None, path: "vague.fits".into(), ..master(MasterKind::Bias) },
        ];
        assert_eq!(s.find(MasterKind::Bias, &masters).unwrap().path, "m.fits");
        assert_eq!(s.unknowns(MasterKind::Bias, &masters[1]), vec!["offset"]);
        assert!(s.unknowns(MasterKind::Flat, &masters[1]).is_empty(), "flats need no offset");
    }

    #[test]
    fn exposures_read_well() {
        assert_eq!(seconds(120.0), "120");
        assert_eq!(seconds(1.5), "1.5");
        assert_eq!(seconds(0.11034314385775865), "0.11");
        assert_eq!(seconds(0.0123), "0.012");
        assert_eq!(seconds(2.0), "2");
        assert_eq!(seconds(0.0), "0");
    }

    mod planning {
        use super::*;
        use crate::{Exposure, Roles};

        /// Lights with the camera's settings unknown: steps set them.
        fn state(masters: Vec<MasterFrame>) -> UiState {
            UiState {
                roles: Roles::default(),
                masters: MastersStatus { masters, ..Default::default() },
                ..UiState::default()
            }
        }

        fn seq(plan: CalibrationPlan, exposures: &[f64]) -> Sequence {
            Sequence {
                calibration: plan,
                steps: exposures
                    .iter()
                    .map(|&seconds| SequenceStep {
                        exposure: Exposure::Fixed { seconds },
                        gain: Some(100.0),
                        offset: Some(50.0),
                        ..Default::default()
                    })
                    .collect(),
                ..Default::default()
            }
        }

        fn kinds(needs: &[Need]) -> Vec<(MasterKind, Purpose)> {
            needs.iter().map(|n| (n.kind, n.purpose)).collect()
        }

        #[test]
        fn darks_flats_and_bias_for_the_flats() {
            use MasterKind::*;
            let n = needs(&seq(CalibrationPlan::default(), &[60.0, 120.0, 60.0]), &state(vec![]));
            assert_eq!(
                kinds(&n),
                vec![(Dark, Purpose::Lights), (Dark, Purpose::Lights), (Flat, Purpose::Flats), (Bias, Purpose::Flats)],
                "one dark per exposure; flat and bias once (same filter)"
            );
            assert!(n.iter().all(|n| n.found.is_none()));
        }

        #[test]
        fn create_takes_matching_frames() {
            use MasterKind::*;
            let mut s = seq(CalibrationPlan::default(), &[60.0]);
            s.steps[0].temperature = Some(-10.0);
            s.steps[0].filter_slot = Some(2);
            let n = needs(&s, &state(vec![]));
            let dark = n[0].capture_step(20).unwrap();
            assert_eq!((dark.frame_type, dark.count, dark.exposure), (FrameType::Dark, 20, Exposure::Fixed { seconds: 60.0 }));
            assert_eq!((dark.gain, dark.offset, dark.temperature), (Some(100.0), Some(50.0), Some(-10.0)));
            assert_eq!(dark.filter_slot, None, "no need to move the wheel");
            let flat = n.iter().find(|n| n.kind == Flat).unwrap().capture_step(25).unwrap();
            assert_eq!((flat.frame_type, flat.filter_slot, flat.temperature), (FrameType::Flat, Some(2), None));
            assert_eq!(flat.exposure, Exposure::DEFAULT_FLAT);
            let bias = n.iter().find(|n| n.kind == Bias).unwrap().capture_step(50).unwrap();
            assert_eq!((bias.frame_type, bias.exposure, bias.temperature), (FrameType::Bias, Exposure::Minimum, None));
            assert!(n[0].setup_hint().contains("Cover"));

            // Dark flats wait for their flat.
            let plan = CalibrationPlan { flats_with: FlatCalibration::DarkFlats, ..Default::default() };
            let n = needs(&seq(plan, &[60.0]), &state(vec![]));
            assert_eq!(n.last().unwrap().capture_step(20), None);
        }

        #[test]
        fn without_darks_the_lights_get_bias() {
            use MasterKind::*;
            let plan = CalibrationPlan { darks: false, flats: false, ..Default::default() };
            let n = needs(&seq(plan, &[60.0, 120.0]), &state(vec![]));
            assert_eq!(kinds(&n), vec![(Bias, Purpose::Lights)]);
        }

        #[test]
        fn dark_flats_take_the_flats_exposure() {
            use MasterKind::*;
            let plan = CalibrationPlan { flats_with: FlatCalibration::DarkFlats, ..Default::default() };
            let s = seq(plan, &[60.0]);
            let n = needs(&s, &state(vec![]));
            assert_eq!(kinds(&n), vec![(Dark, Purpose::Lights), (Flat, Purpose::Flats), (Dark, Purpose::Flats)]);
            assert!(n[2].spec.is_none(), "no flat yet, so no exposure to match");

            let flat = MasterFrame {
                kind: Flat,
                camera: None,
                filter: None,
                exposure: Some(1.5),
                gain: Some(0.0),
                temperature: None,
                width: 6248,
                height: 4176,
                ..master(Flat)
            };
            let dark_flat = MasterFrame { kind: Dark, exposure: Some(1.5), gain: Some(0.0), ..flat.clone() };
            let n = needs(&s, &state(vec![flat, dark_flat]));
            assert_eq!(n[1].found.as_ref().map(|m| m.kind), Some(Flat));
            let df = &n[2];
            assert_eq!(df.spec.as_ref().map(|s| (s.exposure, s.gain)), Some((1.5, Some(0.0))));
            assert!(df.found.is_some(), "matched at the flat's exposure and gain");
            assert!(df.what.contains("for the no flat"), "{}", df.what);
        }
    }
}
