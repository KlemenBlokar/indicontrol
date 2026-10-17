//! Guide calibration: measure how guide pulses move the star on the sensor.

use serde::{Deserialize, Serialize};

use crate::guide::GuideDir;

/// Mapping between mount axes and camera pixels.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Calibration {
    /// Direction (radians, camera x→y) the star moves under West pulses.
    pub ra_angle: f64,
    /// Pixels per millisecond of RA pulse.
    pub ra_rate: f64,
    /// Direction the star moves under North pulses.
    pub dec_angle: f64,
    pub dec_rate: f64,
    /// Mount declination during calibration (degrees), if known.
    pub declination: Option<f64>,
    /// Side of pier during calibration ("EAST"/"WEST"), if known.
    pub pier_side: Option<String>,
}

impl Calibration {
    /// Express a camera displacement as (RA, Dec) components in pixels along
    /// the calibrated axes. Positive RA = the way West pulses move the star;
    /// positive Dec = the way North pulses move it.
    pub fn to_mount(&self, dx: f64, dy: f64) -> (f64, f64) {
        let (ux, uy) = (self.ra_angle.cos(), self.ra_angle.sin());
        let (vx, vy) = (self.dec_angle.cos(), self.dec_angle.sin());
        let det = ux * vy - vx * uy;
        if det.abs() < 1e-6 {
            // Degenerate (parallel axes): fall back to plain projection.
            return (dx * ux + dy * uy, dx * vx + dy * vy);
        }
        ((dx * vy - dy * vx) / det, (ux * dy - uy * dx) / det)
    }

    /// Deviation of the axes from perpendicular, in degrees.
    pub fn orthogonality_error(&self) -> f64 {
        let diff = (self.dec_angle - self.ra_angle).rem_euclid(std::f64::consts::TAU);
        let deg = diff.to_degrees();
        (deg - 90.0).abs().min((deg - 270.0).abs())
    }

    /// The same calibration after a meridian flip: the camera has turned 180°
    /// relative to the sky, so both axes point the other way in the image.
    /// `flip_dec = false` is for mounts that already reverse Dec themselves.
    pub fn flipped(&self, flip_dec: bool) -> Calibration {
        let pi = std::f64::consts::PI;
        Calibration {
            ra_angle: (self.ra_angle + pi).rem_euclid(std::f64::consts::TAU),
            dec_angle: if flip_dec {
                (self.dec_angle + pi).rem_euclid(std::f64::consts::TAU)
            } else {
                self.dec_angle
            },
            pier_side: self.pier_side.as_deref().map(|p| match p {
                "EAST" => "WEST".to_owned(),
                "WEST" => "EAST".to_owned(),
                other => other.to_owned(),
            }),
            ..self.clone()
        }
    }

    /// Camera-frame offset for a dither of `ra`/`dec` pixels along the mount axes.
    pub fn mount_to_camera(&self, ra: f64, dec: f64) -> (f64, f64) {
        (
            ra * self.ra_angle.cos() + dec * self.dec_angle.cos(),
            ra * self.ra_angle.sin() + dec * self.dec_angle.sin(),
        )
    }

    /// RA rate adjusted for the current declination (RA moves slower near the pole).
    pub fn ra_rate_at(&self, declination: Option<f64>) -> f64 {
        match (self.declination, declination) {
            (Some(cal), Some(now)) if cal.abs() < 85.0 && now.abs() < 85.0 => {
                self.ra_rate * now.to_radians().cos() / cal.to_radians().cos()
            }
            _ => self.ra_rate,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CalibrationSettings {
    /// Length of each calibration pulse.
    pub step_ms: f64,
    /// Travel needed on each axis for a good measurement.
    pub distance_px: f64,
    pub max_steps: u32,
    /// Movement that shows Dec backlash has been taken up.
    pub backlash_px: f64,
    pub max_backlash_steps: u32,
}

impl Default for CalibrationSettings {
    fn default() -> Self {
        Self {
            step_ms: 750.0,
            distance_px: 25.0,
            max_steps: 60,
            backlash_px: 3.0,
            max_backlash_steps: 20,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum CalStep {
    /// Send this pulse, then call [`Calibrator::step`] with the next star position.
    Pulse(GuideDir, f64),
    Done(Calibration),
    Failed(String),
}

#[derive(Debug, Clone, Copy)]
enum Phase {
    West { steps: u32 },
    East { remaining: u32 },
    Backlash { steps: u32 },
    North { steps: u32 },
    South { remaining: u32 },
    Finished,
}

pub struct Calibrator {
    settings: CalibrationSettings,
    phase: Phase,
    start: Option<(f64, f64)>,
    ra: Option<(f64, f64)>,
    dec: Option<(f64, f64)>,
    declination: Option<f64>,
    pier_side: Option<String>,
}

impl Calibrator {
    pub fn new(settings: CalibrationSettings, declination: Option<f64>, pier_side: Option<String>) -> Self {
        Self {
            settings,
            phase: Phase::West { steps: 0 },
            start: None,
            ra: None,
            dec: None,
            declination,
            pier_side,
        }
    }

    pub fn describe(&self) -> String {
        match self.phase {
            Phase::West { steps } => format!("Calibrating RA: west step {steps}"),
            Phase::East { remaining } => format!("Calibrating RA: returning east ({remaining} left)"),
            Phase::Backlash { steps } => format!("Clearing Dec backlash: step {steps}"),
            Phase::North { steps } => format!("Calibrating Dec: north step {steps}"),
            Phase::South { remaining } => format!("Calibrating Dec: returning south ({remaining} left)"),
            Phase::Finished => "Calibration finished".into(),
        }
    }

    /// Feed the star position measured after the previous pulse.
    pub fn step(&mut self, pos: (f64, f64)) -> CalStep {
        let s = self.settings.clone();
        let dist = |a: (f64, f64), b: (f64, f64)| (a.0 - b.0).hypot(a.1 - b.1);
        loop {
            match self.phase {
                Phase::West { steps } => {
                    let start = *self.start.get_or_insert(pos);
                    let d = dist(pos, start);
                    if steps > 0 && d >= s.distance_px {
                        let ms = steps as f64 * s.step_ms;
                        self.ra = Some(((pos.1 - start.1).atan2(pos.0 - start.0), d / ms));
                        self.phase = Phase::East { remaining: steps };
                        continue;
                    }
                    if steps >= s.max_steps {
                        return self.fail(format!(
                            "RA moved only {d:.1} px after {steps} west pulses — check the guide \
                             connection or use longer calibration steps"
                        ));
                    }
                    self.phase = Phase::West { steps: steps + 1 };
                    return CalStep::Pulse(GuideDir::West, s.step_ms);
                }
                Phase::East { remaining } => {
                    if remaining > 0 {
                        self.phase = Phase::East { remaining: remaining - 1 };
                        return CalStep::Pulse(GuideDir::East, s.step_ms);
                    }
                    self.start = Some(pos);
                    self.phase = Phase::Backlash { steps: 0 };
                }
                Phase::Backlash { steps } => {
                    let origin = self.start.expect("set before backlash");
                    if dist(pos, origin) >= s.backlash_px || steps >= s.max_backlash_steps {
                        self.start = Some(pos);
                        self.phase = Phase::North { steps: 0 };
                        continue;
                    }
                    self.phase = Phase::Backlash { steps: steps + 1 };
                    return CalStep::Pulse(GuideDir::North, s.step_ms);
                }
                Phase::North { steps } => {
                    let start = self.start.expect("set before north");
                    let d = dist(pos, start);
                    if steps > 0 && d >= s.distance_px {
                        let ms = steps as f64 * s.step_ms;
                        self.dec = Some(((pos.1 - start.1).atan2(pos.0 - start.0), d / ms));
                        self.phase = Phase::South { remaining: steps };
                        continue;
                    }
                    if steps >= s.max_steps {
                        return self.fail(format!(
                            "Dec moved only {d:.1} px after {steps} north pulses — large Dec \
                             backlash, or Dec guiding not working"
                        ));
                    }
                    self.phase = Phase::North { steps: steps + 1 };
                    return CalStep::Pulse(GuideDir::North, s.step_ms);
                }
                Phase::South { remaining } => {
                    if remaining > 0 {
                        self.phase = Phase::South { remaining: remaining - 1 };
                        return CalStep::Pulse(GuideDir::South, s.step_ms);
                    }
                    self.phase = Phase::Finished;
                    let (ra_angle, ra_rate) = self.ra.expect("measured");
                    let (dec_angle, dec_rate) = self.dec.expect("measured");
                    return CalStep::Done(Calibration {
                        ra_angle,
                        ra_rate,
                        dec_angle,
                        dec_rate,
                        declination: self.declination,
                        pier_side: self.pier_side.clone(),
                    });
                }
                Phase::Finished => return CalStep::Failed("calibration already finished".into()),
            }
        }
    }

    fn fail(&mut self, msg: String) -> CalStep {
        self.phase = Phase::Finished;
        CalStep::Failed(msg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cal(ra_deg: f64, dec_deg: f64) -> Calibration {
        Calibration {
            ra_angle: ra_deg.to_radians(),
            ra_rate: 0.01,
            dec_angle: dec_deg.to_radians(),
            dec_rate: 0.01,
            declination: Some(0.0),
            pier_side: None,
        }
    }

    #[test]
    fn to_mount_inverts_axes() {
        for (ra, dec) in [(0.0, 90.0), (30.0, 120.0), (200.0, 110.0), (10.0, -80.0)] {
            let c = cal(ra, dec);
            let (a, b) = (3.0, -2.0);
            let dx = a * c.ra_angle.cos() + b * c.dec_angle.cos();
            let dy = a * c.ra_angle.sin() + b * c.dec_angle.sin();
            let (ra_px, dec_px) = c.to_mount(dx, dy);
            assert!((ra_px - a).abs() < 1e-9 && (dec_px - b).abs() < 1e-9, "{ra} {dec}");
        }
        assert!((cal(30.0, 120.0).orthogonality_error()).abs() < 1e-9);
        assert!((cal(30.0, 110.0).orthogonality_error() - 10.0).abs() < 1e-9);
        assert!((cal(10.0, -80.0).orthogonality_error()).abs() < 1e-9);
    }

    #[test]
    fn flip_and_axis_mapping() {
        let mut c = cal(30.0, 120.0);
        c.pier_side = Some("WEST".into());
        let f = c.flipped(true);
        assert!((f.ra_angle.to_degrees() - 210.0).abs() < 1e-9);
        assert!((f.dec_angle.to_degrees() - 300.0).abs() < 1e-9);
        assert_eq!(f.pier_side.as_deref(), Some("EAST"));
        assert!((c.flipped(false).dec_angle - c.dec_angle).abs() < 1e-12);
        assert_eq!(f.flipped(true).pier_side.as_deref(), Some("WEST"));
        let (dx, dy) = c.mount_to_camera(2.0, -1.0);
        let (ra, dec) = c.to_mount(dx, dy);
        assert!((ra - 2.0).abs() < 1e-9 && (dec + 1.0).abs() < 1e-9);
    }

    #[test]
    fn ra_rate_scales_with_declination() {
        let mut c = cal(0.0, 90.0);
        c.declination = Some(0.0);
        assert!((c.ra_rate_at(Some(60.0)) - 0.005).abs() < 1e-12);
        assert_eq!(c.ra_rate_at(None), 0.01);
    }
}
