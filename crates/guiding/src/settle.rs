//! Dither offsets and settle detection.

use serde::{Deserialize, Serialize};

use crate::calibration::Calibration;

/// Small deterministic PRNG for dither directions (xorshift64*).
#[derive(Debug, Clone)]
pub struct DitherRng(u64);

impl Default for DitherRng {
    fn default() -> Self {
        Self::new(0x5EED)
    }
}

impl DitherRng {
    pub fn new(seed: u64) -> Self {
        Self(seed | 1)
    }

    /// Uniform in [-1, 1).
    pub fn next_signed(&mut self) -> f64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        let v = self.0.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11;
        v as f64 / (1u64 << 53) as f64 * 2.0 - 1.0
    }
}

/// Random lock-position offset of up to `amount` pixels along each mount axis.
pub fn dither_offset(cal: &Calibration, amount: f64, ra_only: bool, rng: &mut DitherRng) -> (f64, f64) {
    let ra = amount * rng.next_signed();
    let dec = if ra_only { 0.0 } else { amount * rng.next_signed() };
    cal.mount_to_camera(ra, dec)
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum SettleOutcome {
    Settled,
    TimedOut,
}

/// Guiding is settled once the error stays within `max_px` for `time_s`.
#[derive(Debug, Clone)]
pub struct SettleTracker {
    pub max_px: f64,
    pub time_s: f64,
    pub timeout_s: f64,
    started: f64,
    ok_since: Option<f64>,
}

impl SettleTracker {
    pub fn new(max_px: f64, time_s: f64, timeout_s: f64, now: f64) -> Self {
        Self {
            max_px,
            time_s,
            timeout_s,
            started: now,
            ok_since: None,
        }
    }

    /// Feed one guide error (pixels) at time `now` (seconds).
    pub fn update(&mut self, now: f64, error_px: Option<f64>) -> Option<SettleOutcome> {
        match error_px {
            Some(e) if e <= self.max_px => {
                let since = *self.ok_since.get_or_insert(now);
                if now - since >= self.time_s {
                    return Some(SettleOutcome::Settled);
                }
            }
            _ => self.ok_since = None,
        }
        (now - self.started >= self.timeout_s).then_some(SettleOutcome::TimedOut)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rng_is_uniformish() {
        let mut r = DitherRng::new(42);
        let v: Vec<f64> = (0..10_000).map(|_| r.next_signed()).collect();
        assert!(v.iter().all(|x| (-1.0..1.0).contains(x)));
        let mean = v.iter().sum::<f64>() / v.len() as f64;
        assert!(mean.abs() < 0.05, "{mean}");
        assert!(v.iter().any(|x| *x > 0.9) && v.iter().any(|x| *x < -0.9));
    }

    #[test]
    fn dither_follows_axes() {
        let cal = Calibration {
            ra_angle: 0.0,
            ra_rate: 0.01,
            dec_angle: std::f64::consts::FRAC_PI_2,
            dec_rate: 0.01,
            declination: None,
            pier_side: None,
        };
        let mut r = DitherRng::new(7);
        for _ in 0..100 {
            let (dx, dy) = dither_offset(&cal, 3.0, true, &mut r);
            assert!(dx.abs() <= 3.0 && dy.abs() < 1e-12);
        }
    }

    #[test]
    fn settle_detection() {
        let mut s = SettleTracker::new(1.0, 5.0, 30.0, 0.0);
        assert_eq!(s.update(0.0, Some(3.0)), None);
        assert_eq!(s.update(2.0, Some(0.5)), None);
        assert_eq!(s.update(5.0, Some(0.8)), None);
        // A spike restarts the clock.
        assert_eq!(s.update(6.0, Some(1.5)), None);
        assert_eq!(s.update(8.0, Some(0.2)), None);
        assert_eq!(s.update(13.0, Some(0.2)), Some(SettleOutcome::Settled));

        let mut s = SettleTracker::new(1.0, 5.0, 10.0, 0.0);
        assert_eq!(s.update(4.0, None), None);
        assert_eq!(s.update(10.0, Some(2.0)), Some(SettleOutcome::TimedOut));
    }
}
