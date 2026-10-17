//! Guide algorithms: turn star displacement into mount corrections.

use serde::{Deserialize, Serialize};

use crate::calibration::Calibration;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum GuideDir {
    North,
    South,
    East,
    West,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum DecMode {
    Off,
    #[default]
    Auto,
    /// Only north pulses (e.g. with deliberate polar misalignment).
    North,
    South,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GuideParams {
    /// Fraction of the measured RA error corrected each frame.
    pub ra_aggressiveness: f64,
    /// Weight of the previous RA correction (smooths seeing).
    pub ra_hysteresis: f64,
    /// RA errors smaller than this (pixels) are ignored.
    pub ra_min_move: f64,
    pub dec_aggressiveness: f64,
    pub dec_min_move: f64,
    pub dec_mode: DecMode,
    /// Consecutive Dec errors on the other side needed to reverse direction.
    pub dec_reversal_frames: u32,
    pub max_ra_pulse_ms: f64,
    pub max_dec_pulse_ms: f64,
}

impl Default for GuideParams {
    fn default() -> Self {
        Self {
            ra_aggressiveness: 0.7,
            ra_hysteresis: 0.1,
            ra_min_move: 0.15,
            dec_aggressiveness: 0.7,
            dec_min_move: 0.15,
            dec_mode: DecMode::Auto,
            dec_reversal_frames: 3,
            max_ra_pulse_ms: 2500.0,
            max_dec_pulse_ms: 2500.0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Correction {
    /// Error along the RA axis in pixels (positive: displaced towards west-pulse motion).
    pub ra_px: f64,
    pub dec_px: f64,
    pub ra: Option<(GuideDir, f64)>,
    pub dec: Option<(GuideDir, f64)>,
}

/// Stateful guide controller (hysteresis in RA, resist-switch in Dec).
#[derive(Debug, Clone, Default)]
pub struct GuideController {
    pub params: GuideParams,
    last_ra: f64,
    dec_sign: Option<f64>,
    dec_pending: u32,
}

impl GuideController {
    pub fn new(params: GuideParams) -> Self {
        Self {
            params,
            ..Default::default()
        }
    }

    pub fn reset(&mut self) {
        self.last_ra = 0.0;
        self.dec_sign = None;
        self.dec_pending = 0;
    }

    /// Correction for a star at `star` that should be at `lock`.
    pub fn compute(
        &mut self,
        cal: &Calibration,
        declination: Option<f64>,
        lock: (f64, f64),
        star: (f64, f64),
    ) -> Correction {
        let (ra_px, dec_px) = cal.to_mount(star.0 - lock.0, star.1 - lock.1);
        let p = self.params.clone();

        // RA: hysteresis filter.
        let mut ra = ((1.0 - p.ra_hysteresis) * ra_px + p.ra_hysteresis * self.last_ra) * p.ra_aggressiveness;
        if ra_px.abs() < p.ra_min_move {
            ra = 0.0;
        }
        self.last_ra = ra;

        // Dec: only reverse after several consistent errors on the other side.
        let dec = self.dec_correction(dec_px);

        let pulse = |px: f64, rate: f64, max: f64, pos: GuideDir, neg: GuideDir| {
            (px != 0.0 && rate > 0.0).then(|| {
                // Star displaced along +axis → pulse the opposite way.
                let dir = if px > 0.0 { neg } else { pos };
                (dir, (px.abs() / rate).min(max))
            })
        };
        Correction {
            ra_px,
            dec_px,
            ra: pulse(
                ra,
                cal.ra_rate_at(declination),
                p.max_ra_pulse_ms,
                GuideDir::West,
                GuideDir::East,
            ),
            dec: pulse(dec, cal.dec_rate, p.max_dec_pulse_ms, GuideDir::North, GuideDir::South),
        }
    }

    fn dec_correction(&mut self, dec_px: f64) -> f64 {
        let p = &self.params;
        if p.dec_mode == DecMode::Off || dec_px.abs() < p.dec_min_move {
            self.dec_pending = 0;
            return 0.0;
        }
        // Positive error is corrected by a South pulse.
        let allowed = match p.dec_mode {
            DecMode::North => dec_px < 0.0,
            DecMode::South => dec_px > 0.0,
            _ => true,
        };
        if !allowed {
            return 0.0;
        }
        let sign = dec_px.signum();
        match self.dec_sign {
            Some(s) if s != sign => {
                self.dec_pending += 1;
                if self.dec_pending < p.dec_reversal_frames {
                    return 0.0;
                }
                self.dec_sign = Some(sign);
                self.dec_pending = 0;
            }
            None => self.dec_sign = Some(sign),
            _ => self.dec_pending = 0,
        }
        dec_px * p.dec_aggressiveness
    }
}

/// Running RMS of recent guide errors.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct GuideStats {
    pub ra_rms: f64,
    pub dec_rms: f64,
    pub total_rms: f64,
    pub samples: usize,
}

impl GuideStats {
    pub fn from_errors(errors: impl Iterator<Item = (f64, f64)>) -> Self {
        let (mut ra, mut dec, mut n) = (0.0, 0.0, 0usize);
        for (r, d) in errors {
            ra += r * r;
            dec += d * d;
            n += 1;
        }
        if n == 0 {
            return Self::default();
        }
        let (ra, dec) = ((ra / n as f64).sqrt(), (dec / n as f64).sqrt());
        Self {
            ra_rms: ra,
            dec_rms: dec,
            total_rms: ra.hypot(dec),
            samples: n,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::calibration::{CalStep, CalibrationSettings, Calibrator};

    /// A mount + guide camera: West pulses move the star along `theta`,
    /// North pulses along `theta + 90°`; RA drifts and has periodic error;
    /// Dec has backlash.
    struct SimMount {
        pos: (f64, f64),
        theta: f64,
        ra_rate: f64,
        dec_rate: f64,
        drift_per_frame: f64,
        pe_amplitude: f64,
        pe_period_frames: f64,
        dec_backlash_ms: f64,
        dec_slack: f64,
        dec_last: Option<GuideDir>,
        frame: u64,
    }

    impl SimMount {
        fn new(theta_deg: f64) -> Self {
            Self {
                pos: (320.0, 240.0),
                theta: theta_deg.to_radians(),
                ra_rate: 0.004,
                dec_rate: 0.0035,
                drift_per_frame: 0.3,
                pe_amplitude: 2.0,
                pe_period_frames: 40.0,
                dec_backlash_ms: 1500.0,
                dec_slack: 1500.0,
                dec_last: None,
                frame: 0,
            }
        }

        fn ra_unit(&self) -> (f64, f64) {
            (self.theta.cos(), self.theta.sin())
        }

        fn dec_unit(&self) -> (f64, f64) {
            let a = self.theta + std::f64::consts::FRAC_PI_2;
            (a.cos(), a.sin())
        }

        fn mv(&mut self, unit: (f64, f64), px: f64) {
            self.pos.0 += unit.0 * px;
            self.pos.1 += unit.1 * px;
        }

        fn pulse(&mut self, dir: GuideDir, ms: f64) {
            match dir {
                GuideDir::West => self.mv(self.ra_unit(), self.ra_rate * ms),
                GuideDir::East => self.mv(self.ra_unit(), -self.ra_rate * ms),
                GuideDir::North | GuideDir::South => {
                    if self.dec_last != Some(dir) {
                        self.dec_slack = self.dec_backlash_ms;
                        self.dec_last = Some(dir);
                    }
                    let take = ms.min(self.dec_slack);
                    self.dec_slack -= take;
                    let eff = ms - take;
                    let s = if dir == GuideDir::North { 1.0 } else { -1.0 };
                    self.mv(self.dec_unit(), s * self.dec_rate * eff);
                }
            }
        }

        /// Advance one frame: drift + periodic error, plus measurement jitter.
        fn expose(&mut self) -> (f64, f64) {
            self.frame += 1;
            let phase = self.frame as f64 * std::f64::consts::TAU / self.pe_period_frames;
            let pe_step = self.pe_amplitude * (phase.sin() - (phase - std::f64::consts::TAU / self.pe_period_frames).sin());
            self.mv(self.ra_unit(), self.drift_per_frame + pe_step);
            let jitter = ((self.frame * 7919 % 101) as f64 / 101.0 - 0.5) * 0.1;
            (self.pos.0 + jitter, self.pos.1 - jitter)
        }
    }

    fn calibrate(sim: &mut SimMount) -> Calibration {
        let mut c = Calibrator::new(CalibrationSettings::default(), Some(20.0), None);
        let mut pos = sim.expose();
        for _ in 0..500 {
            match c.step(pos) {
                CalStep::Pulse(dir, ms) => {
                    sim.pulse(dir, ms);
                    pos = sim.expose();
                }
                CalStep::Done(cal) => return cal,
                CalStep::Failed(e) => panic!("{e}"),
            }
        }
        panic!("calibration did not finish");
    }

    #[test]
    fn calibration_recovers_geometry() {
        for theta in [0.0, 37.0, 160.0, 250.0] {
            let mut sim = SimMount::new(theta);
            sim.pe_amplitude = 0.0;
            sim.drift_per_frame = 0.05;
            let cal = calibrate(&mut sim);
            let ang = |a: f64| a.to_degrees().rem_euclid(360.0);
            let diff = |a: f64, b: f64| ((a - b + 540.0) % 360.0 - 180.0).abs();
            assert!(diff(ang(cal.ra_angle), theta) < 3.0, "ra angle {} vs {theta}", ang(cal.ra_angle));
            assert!(diff(ang(cal.dec_angle), theta + 90.0) < 3.0, "dec angle {}", ang(cal.dec_angle));
            assert!((cal.ra_rate / sim.ra_rate - 1.0).abs() < 0.1, "ra rate {}", cal.ra_rate);
            assert!((cal.dec_rate / sim.dec_rate - 1.0).abs() < 0.1, "dec rate {}", cal.dec_rate);
            assert!(cal.orthogonality_error() < 3.0);
        }
    }

    #[test]
    fn calibration_fails_without_dec() {
        let mut sim = SimMount::new(20.0);
        sim.dec_rate = 0.0;
        let mut c = Calibrator::new(CalibrationSettings::default(), None, None);
        let mut pos = sim.expose();
        let err = loop {
            match c.step(pos) {
                CalStep::Pulse(dir, ms) => {
                    sim.pulse(dir, ms);
                    pos = sim.expose();
                }
                CalStep::Done(_) => panic!("should fail"),
                CalStep::Failed(e) => break e,
            }
        };
        assert!(err.contains("Dec moved only"), "{err}");
    }

    fn run_guiding(sim: &mut SimMount, cal: Option<&Calibration>, frames: usize) -> GuideStats {
        let lock = sim.expose();
        let mut ctl = GuideController::new(GuideParams::default());
        let mut errs = Vec::new();
        for _ in 0..frames {
            let star = sim.expose();
            if let Some(cal) = cal {
                let c = ctl.compute(cal, Some(20.0), lock, star);
                for (dir, ms) in [c.ra, c.dec].into_iter().flatten() {
                    sim.pulse(dir, ms);
                }
            }
            errs.push((star.0 - lock.0, star.1 - lock.1));
        }
        // Skip the settling phase.
        GuideStats::from_errors(errs.iter().skip(frames / 4).copied())
    }

    #[test]
    fn guiding_holds_the_star() {
        let mut sim = SimMount::new(37.0);
        let cal = calibrate(&mut sim);
        let guided = run_guiding(&mut sim, Some(&cal), 200);
        let mut unguided_sim = SimMount::new(37.0);
        let unguided = run_guiding(&mut unguided_sim, None, 200);
        // Each frame the star drifts ~0.3 px plus up to ~0.3 px of periodic
        // error before it is corrected at 70 % aggressiveness, so a residual
        // of a few tenths of a pixel is the expected floor.
        assert!(guided.total_rms < 0.8, "guided rms {guided:?}");
        assert!(unguided.total_rms > 20.0, "unguided rms {unguided:?}");
    }

    #[test]
    fn flipped_calibration_guides_after_meridian_flip() {
        let mut sim = SimMount::new(37.0);
        let cal = calibrate(&mut sim);
        // Meridian flip: the camera turns 180° relative to the sky.
        sim.theta += std::f64::consts::PI;
        let guided = run_guiding(&mut sim, Some(&cal.flipped(true)), 200);
        assert!(guided.total_rms < 0.8, "flipped guiding rms {guided:?}");
        // Using the unflipped calibration would push the star away.
        let mut sim2 = SimMount::new(37.0 + 180.0);
        let wrong = run_guiding(&mut sim2, Some(&cal), 60);
        assert!(wrong.total_rms > 5.0, "unflipped should diverge: {wrong:?}");
    }

    #[test]
    fn dec_resists_switching() {
        let cal = Calibration {
            ra_angle: 0.0,
            ra_rate: 0.01,
            dec_angle: std::f64::consts::FRAC_PI_2,
            dec_rate: 0.01,
            declination: None,
            pier_side: None,
        };
        let mut ctl = GuideController::new(GuideParams::default());
        let lock = (100.0, 100.0);
        // Star north of lock → south pulse.
        let c = ctl.compute(&cal, None, lock, (100.0, 101.0));
        assert_eq!(c.dec.map(|d| d.0), Some(GuideDir::South));
        // Two frames on the other side: no reversal yet.
        for _ in 0..2 {
            assert_eq!(ctl.compute(&cal, None, lock, (100.0, 99.0)).dec, None);
        }
        // Third consecutive frame: reverse.
        let c = ctl.compute(&cal, None, lock, (100.0, 99.0));
        assert_eq!(c.dec.map(|d| d.0), Some(GuideDir::North));
        // Small errors are ignored.
        assert_eq!(ctl.compute(&cal, None, lock, (100.05, 99.95)).ra, None);
        // North-only mode refuses south pulses.
        ctl.params.dec_mode = DecMode::North;
        ctl.reset();
        assert_eq!(ctl.compute(&cal, None, lock, (100.0, 101.0)).dec, None);
    }

    #[test]
    fn pulses_are_capped() {
        let cal = Calibration {
            ra_angle: 0.0,
            ra_rate: 0.001,
            dec_angle: std::f64::consts::FRAC_PI_2,
            dec_rate: 0.001,
            declination: None,
            pier_side: None,
        };
        let mut ctl = GuideController::new(GuideParams::default());
        let c = ctl.compute(&cal, None, (0.0, 0.0), (50.0, 0.0));
        assert_eq!(c.ra, Some((GuideDir::East, 2500.0)));
    }
}
