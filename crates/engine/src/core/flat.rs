//! Flat exposure search: the exposure, and the gain when the exposure would
//! leave the preferred window, until the brightest channel's median is at
//! the target.

/// What the search may do for one flat step.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct Limits {
    pub target: f64,
    pub tolerance: f64,
    /// Shortest and longest exposure allowed (seconds).
    pub min: f64,
    pub max: f64,
    /// Exposures to aim for; outside them the gain changes, if it may.
    pub window: Option<(f64, f64)>,
    /// The camera's gain range, when the search may change the gain.
    pub gain_range: Option<(f64, f64)>,
}

/// One test frame: taken at `exposure` and `gain`, reaching `brightest`
/// (fraction of full scale).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct Sample {
    pub exposure: f64,
    pub gain: Option<f64>,
    pub brightest: f64,
}

impl Sample {
    /// Fraction of full scale per second, when the level is measurable.
    fn rate(&self) -> Option<f64> {
        (self.brightest > 0.01 && self.brightest < 0.95).then(|| self.brightest / self.exposure)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(super) enum Next {
    /// The level is at the target: take the flats like this.
    Done,
    /// Another test frame with these settings.
    Try { exposure: f64, gain: Option<f64> },
    /// Even the limits miss the target; the light source must change.
    /// Test frames go on at these settings.
    Source { exposure: f64, gain: Option<f64>, brighter: bool },
}

/// What to do after `now`; `before` is the previous test frame of the
/// same step, to learn how the gain scales the signal.
pub(super) fn next(l: &Limits, now: Sample, before: Option<Sample>) -> Next {
    let error = (now.brightest - l.target).abs();
    if error <= l.tolerance {
        return Next::Done;
    }
    // The exposure that would reach the target at this gain.
    let wanted = if now.brightest >= 0.95 {
        now.exposure / 4.0
    } else if now.brightest <= 0.01 {
        now.exposure * 8.0
    } else {
        now.exposure * l.target / now.brightest
    };
    let (lo, hi) = l.window.unwrap_or((l.min, l.max));
    if (lo..=hi).contains(&wanted) && (l.min..=l.max).contains(&wanted) {
        return Next::Try { exposure: wanted, gain: now.gain };
    }
    if let (Some((gmin, gmax)), Some(gain)) = (l.gain_range, now.gain) {
        let brighter = wanted > hi;
        let at_limit = if brighter { gain >= gmax } else { gain <= gmin };
        if !at_limit {
            // Aim for the middle of the window (geometric).
            let goal = (lo * hi).sqrt();
            let step = (gmax - gmin) / 4.0;
            let guess = if brighter { gain + step } else { gain - step };
            // Signal ∝ exp(k·gain) (dB-like units) from the last two
            // frames; a camera with other units still converges.
            let fitted = match (before, before.and_then(|b| b.rate()), now.rate()) {
                (Some(b), Some(r0), Some(r1)) => b.gain.and_then(|g0| {
                    let k = (r1.ln() - r0.ln()) / (gain - g0);
                    (k.is_finite() && k > 0.0).then(|| gain + (wanted / goal).ln() / k)
                }),
                _ => None,
            };
            let new_gain = fitted.unwrap_or(guess).clamp(gmin, gmax).round();
            if new_gain != gain {
                // Without a fit the effect of the gain is unknown: keep the
                // exposure, so the next frame measures it.
                let exposure = if fitted.is_some() { goal } else { now.exposure };
                return Next::Try { exposure: exposure.clamp(l.min, l.max), gain: Some(new_gain) };
            }
        }
    }
    // The gain cannot help: leave the window, within the limits.
    if (l.min..=l.max).contains(&wanted) {
        return Next::Try { exposure: wanted, gain: now.gain };
    }
    let exposure = wanted.clamp(l.min, l.max);
    if exposure != now.exposure {
        return Next::Try { exposure, gain: now.gain };
    }
    Next::Source { exposure, gain: now.gain, brighter: wanted > l.max }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A panel and a camera whose gain is in 0.1 dB (ZWO-like): fraction of
    /// full scale per second at gain 0 is `rate`.
    fn shoot(rate: f64, exposure: f64, gain: Option<f64>) -> Sample {
        let g = gain.unwrap_or(0.0);
        let brightest = (0.01 + rate * exposure * 10f64.powf(g / 200.0)).min(1.0);
        Sample { exposure, gain, brightest }
    }

    const LIMITS: Limits = Limits {
        target: 0.45,
        tolerance: 0.05,
        min: 0.01,
        max: 30.0,
        window: Some((1.0, 5.0)),
        gain_range: Some((0.0, 500.0)),
    };

    /// Runs the search from `exposure` and `gain`: the frames it took and
    /// how it ended.
    fn search(l: &Limits, rate: f64, mut exposure: f64, mut gain: Option<f64>) -> (u32, Next, Sample) {
        let mut before = None;
        for frames in 1..=20 {
            let now = shoot(rate, exposure, gain);
            match next(l, now, before) {
                Next::Try { exposure: e, gain: g } => (exposure, gain) = (e, g),
                end => return (frames, end, now),
            }
            before = Some(now);
        }
        panic!("no end after 20 frames");
    }

    #[test]
    fn inside_the_window_only_the_exposure_changes() {
        // 2 s at gain 100.
        let (frames, end, last) = search(&LIMITS, 0.44 / 2.0 / 10f64.powf(0.5), 1.0, Some(100.0));
        assert_eq!(end, Next::Done);
        assert_eq!(last.gain, Some(100.0));
        assert!(frames <= 3, "{frames}");
    }

    #[test]
    fn a_bright_panel_lowers_the_gain() {
        // At gain 100 the target needs 0.3 s, under the window; 1 s at gain 0.
        let rate = 0.44 / 0.3 / 10f64.powf(0.5);
        let (frames, end, last) = search(&LIMITS, rate, 1.0, Some(100.0));
        assert_eq!(end, Next::Done);
        assert!(last.gain.unwrap() < 100.0, "{last:?}");
        assert!((1.0..=5.0).contains(&last.exposure), "{last:?}");
        assert!(frames <= 7, "{frames}");

        // Brighter still: gain 0 and under the window, as close as it gets.
        let (_, end, last) = search(&LIMITS, 0.44 / 0.1 / 10f64.powf(0.5), 1.0, Some(100.0));
        assert_eq!(end, Next::Done);
        assert_eq!(last.gain, Some(0.0));
        assert!(last.exposure < 1.0, "{last:?}");
    }

    #[test]
    fn a_dim_panel_raises_the_gain() {
        // Narrowband: 60 s at gain 0.
        let (_, end, last) = search(&LIMITS, 0.44 / 60.0, 2.0, Some(0.0));
        assert_eq!(end, Next::Done);
        assert!(last.gain.unwrap() > 0.0 && (1.0..=5.0).contains(&last.exposure), "{last:?}");
    }

    #[test]
    fn without_gain_control_the_window_is_left() {
        let l = Limits { gain_range: None, ..LIMITS };
        let (_, end, last) = search(&l, 4.4, 1.0, Some(100.0));
        assert_eq!(end, Next::Done);
        assert!(last.exposure < 1.0 && last.gain == Some(100.0), "{last:?}");
    }

    #[test]
    fn out_of_reach_asks_for_the_light_to_change() {
        let l = Limits { gain_range: None, ..LIMITS };
        let (_, end, last) = search(&l, 1000.0, 1.0, None);
        assert_eq!(end, Next::Source { exposure: 0.01, gain: None, brighter: false });
        assert_eq!(last.exposure, 0.01);
        // Dimmest gain, longest exposure, still too dark.
        let (_, end, _) = search(&LIMITS, 1e-5, 1.0, Some(0.0));
        assert_eq!(end, Next::Source { exposure: 30.0, gain: Some(500.0), brighter: true });
    }
}
