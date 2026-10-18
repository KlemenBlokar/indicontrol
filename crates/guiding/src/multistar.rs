//! Multi-star guiding: track every usable star and guide on their consensus.
//!
//! Each star keeps an offset from a virtual *anchor*; the anchor is what the
//! guider locks on. Every frame the anchor is estimated from all stars that
//! agree with each other (robust, SNR-weighted), so centroid noise averages
//! down. What is common to all stars is mount error; what differs between
//! them (field rotation from polar misalignment, differential refraction) is
//! absorbed slowly into the offsets, so references never go stale and the
//! guide signal never picks it up.
//!
//! No star count to configure: stars are added when found, dropped when lost
//! or inconsistent, and with a single star left this is plain single-star
//! guiding. Losing the primary star just promotes another one.

use serde::{Deserialize, Serialize};

use crate::star::Star;

/// Most stars tracked at once.
pub const MAX_STARS: usize = 12;
/// Faintest star worth tracking as a secondary.
const MIN_SNR: f64 = 10.0;
/// Beyond this SNR stars count equally (seeing dominates their error).
const SNR_CAP: f64 = 50.0;
/// Per-frame fraction of a star's residual absorbed into its offset.
const ADAPT: f64 = 0.02;
/// Smallest disagreement (px) treated as an outlier.
const MIN_TOLERANCE: f64 = 1.5;
/// Consecutive frames a star may be missing or inconsistent before it is dropped.
const DROP_AFTER: u32 = 5;
/// Look for new stars this often (frames) while below `MAX_STARS`.
const SCAN_EVERY: u64 = 10;
/// Fold the rotation estimate into a running total this often (frames).
const ROTATION_EVERY: u64 = 50;

#[derive(Debug, Clone)]
struct Tracked {
    id: u32,
    /// Position relative to the anchor.
    offset: (f64, f64),
    missing: u32,
    outlier: u32,
}

/// Result of one frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Fix {
    /// Measured anchor position: guide on this.
    pub x: f64,
    pub y: f64,
    /// The primary star as measured in this frame.
    pub primary: Star,
    /// Stars that contributed (1 = single-star guiding).
    pub used: usize,
}

/// A tracked star for display.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct GuideStarMark {
    pub x: f64,
    pub y: f64,
    pub primary: bool,
    /// Contributed to the last fix.
    pub used: bool,
}

#[derive(Debug, Clone)]
pub struct StarField {
    stars: Vec<Tracked>,
    anchor: (f64, f64),
    primary: u32,
    next_id: u32,
    frames: u64,
    last_used: Vec<u32>,
    /// Rotation (radians) accumulated up to `snapshot`.
    rotation_base: f64,
    snapshot: Vec<(u32, (f64, f64))>,
}

impl StarField {
    /// Start tracking on `primary`; the anchor starts on it.
    pub fn new(primary: &Star) -> Self {
        let first = Tracked {
            id: 0,
            offset: (0.0, 0.0),
            missing: 0,
            outlier: 0,
        };
        Self {
            snapshot: vec![(0, (0.0, 0.0))],
            stars: vec![first],
            anchor: (primary.x, primary.y),
            primary: 0,
            next_id: 1,
            frames: 0,
            last_used: vec![0],
            rotation_base: 0.0,
        }
    }

    pub fn anchor(&self) -> (f64, f64) {
        self.anchor
    }

    pub fn len(&self) -> usize {
        self.stars.len()
    }

    pub fn is_empty(&self) -> bool {
        self.stars.is_empty()
    }

    /// Where each tracked star should be in the next frame.
    pub fn predictions(&self) -> Vec<(f64, f64)> {
        self.stars
            .iter()
            .map(|s| (self.anchor.0 + s.offset.0, self.anchor.1 + s.offset.1))
            .collect()
    }

    /// Tracked stars with their predicted positions, for display.
    pub fn marks(&self) -> Vec<GuideStarMark> {
        self.stars
            .iter()
            .map(|s| GuideStarMark {
                x: self.anchor.0 + s.offset.0,
                y: self.anchor.1 + s.offset.1,
                primary: s.id == self.primary,
                used: self.last_used.contains(&s.id),
            })
            .collect()
    }

    /// Whether the next frame should be searched for new stars.
    pub fn wants_candidates(&self) -> bool {
        self.stars.len() < MAX_STARS && self.frames.is_multiple_of(SCAN_EVERY)
    }

    /// Update with this frame's measurements, one per `predictions()` entry.
    /// `None` when no star was found at all.
    pub fn update(&mut self, measured: &[Option<Star>]) -> Option<Fix> {
        assert_eq!(measured.len(), self.stars.len(), "one measurement per tracked star");
        self.frames += 1;
        // Usable measurements: (index, anchor estimate, weight).
        let obs: Vec<(usize, (f64, f64), f64)> = measured
            .iter()
            .enumerate()
            .filter_map(|(i, m)| {
                let m = m.as_ref().filter(|m| !m.saturated)?;
                let o = self.stars[i].offset;
                Some((i, (m.x - o.0, m.y - o.1), m.snr.min(SNR_CAP).powi(2)))
            })
            .collect();
        if obs.is_empty() {
            // Nothing seen (clouds, a slew): keep the star set as it is.
            self.last_used.clear();
            return None;
        }

        let primary_idx = self.stars.iter().position(|s| s.id == self.primary);
        let inliers = self.consensus(&obs, primary_idx);
        let wsum: f64 = inliers.iter().map(|&k| obs[k].2).sum();
        let anchor = inliers.iter().fold((0.0, 0.0), |a, &k| {
            let (_, p, w) = obs[k];
            (a.0 + p.0 * w / wsum, a.1 + p.1 * w / wsum)
        });
        self.anchor = anchor;

        // Bookkeeping, and absorb each star's slow drift relative to the rest.
        let multi = inliers.len() >= 2;
        for s in &mut self.stars {
            s.missing += 1;
        }
        for (k, &(i, p, _)) in obs.iter().enumerate() {
            let s = &mut self.stars[i];
            s.missing = 0;
            if inliers.contains(&k) {
                s.outlier = 0;
                if multi {
                    s.offset.0 += ADAPT * (p.0 - anchor.0);
                    s.offset.1 += ADAPT * (p.1 - anchor.1);
                }
            } else {
                s.outlier += 1;
            }
        }
        self.last_used = inliers.iter().map(|&k| self.stars[obs[k].0].id).collect();

        // Primary: keep it while it is used, otherwise promote the best star.
        if !self.last_used.contains(&self.primary) {
            let best = inliers.iter().max_by(|&&a, &&b| obs[a].2.total_cmp(&obs[b].2)).expect("non-empty");
            self.primary = self.stars[obs[*best].0].id;
        }
        let primary_idx = self.stars.iter().position(|s| s.id == self.primary).expect("tracked");
        let primary = measured[primary_idx].expect("primary was measured");
        let used = inliers.len();

        let keep = self.primary;
        self.stars
            .retain(|s| s.id == keep || (s.missing < DROP_AFTER && s.outlier < DROP_AFTER));
        self.update_rotation();

        Some(Fix {
            x: anchor.0,
            y: anchor.1,
            primary,
            used,
        })
    }

    /// Indices into `obs` of the measurements that agree with each other.
    fn consensus(&self, obs: &[(usize, (f64, f64), f64)], primary_idx: Option<usize>) -> Vec<usize> {
        let all: Vec<usize> = (0..obs.len()).collect();
        if obs.len() == 1 {
            return all;
        }
        // Robust centre: the median for three or more stars; with two, trust
        // the primary (or the brighter star) if they disagree.
        let centre = if obs.len() >= 3 {
            (median(obs.iter().map(|o| o.1.0)), median(obs.iter().map(|o| o.1.1)))
        } else {
            let k = obs
                .iter()
                .position(|o| Some(o.0) == primary_idx)
                .unwrap_or(if obs[0].2 >= obs[1].2 { 0 } else { 1 });
            obs[k].1
        };
        let dist: Vec<f64> = obs.iter().map(|o| (o.1.0 - centre.0).hypot(o.1.1 - centre.1)).collect();
        let tolerance = if obs.len() >= 3 {
            (4.0 * 1.4826 * median(dist.iter().copied())).max(MIN_TOLERANCE)
        } else {
            MIN_TOLERANCE
        };
        let inliers: Vec<usize> = all.into_iter().filter(|&k| dist[k] <= tolerance).collect();
        if inliers.is_empty() {
            // Everything disagrees: fall back to the star closest to the centre.
            let k = (0..obs.len()).min_by(|&a, &b| dist[a].total_cmp(&dist[b])).expect("non-empty");
            vec![k]
        } else {
            inliers
        }
    }

    /// Start tracking suitable new stars from a full-frame detection.
    /// `separation` is the search radius: stars closer than that to another
    /// star (or to the frame edge) could be confused and are skipped.
    pub fn add_candidates(&mut self, candidates: &[Star], separation: f64, frame: (usize, usize)) {
        let (w, h) = (frame.0 as f64, frame.1 as f64);
        let before = self.stars.len();
        let mut taken = self.predictions();
        for (i, c) in candidates.iter().enumerate() {
            if self.stars.len() >= MAX_STARS {
                break;
            }
            let crowded = candidates
                .iter()
                .enumerate()
                .any(|(j, o)| j != i && (o.x - c.x).hypot(o.y - c.y) < separation)
                || taken.iter().any(|t| (t.0 - c.x).hypot(t.1 - c.y) < separation);
            let inside = c.x >= separation && c.y >= separation && c.x < w - separation && c.y < h - separation;
            if c.saturated || c.snr < MIN_SNR || crowded || !inside {
                continue;
            }
            self.stars.push(Tracked {
                id: self.next_id,
                offset: (c.x - self.anchor.0, c.y - self.anchor.1),
                missing: 0,
                outlier: 0,
            });
            self.next_id += 1;
            taken.push((c.x, c.y));
        }
        if self.stars.len() > before {
            // New stars measure rotation from now on.
            self.rebase_rotation();
        }
    }

    /// Field rotation (radians, counter-clockwise in pixel coordinates) since
    /// tracking began, estimated from how the star offsets turned.
    pub fn rotation(&self) -> Option<f64> {
        Some(self.rotation_base + self.rotation_since_snapshot()?)
    }

    fn update_rotation(&mut self) {
        if self.frames.is_multiple_of(ROTATION_EVERY) {
            self.rebase_rotation();
        }
    }

    fn rebase_rotation(&mut self) {
        if let Some(delta) = self.rotation_since_snapshot() {
            self.rotation_base += delta;
        }
        // Stars that left are forgotten; new ones count from now on.
        self.snapshot = self.stars.iter().map(|s| (s.id, s.offset)).collect();
    }

    fn rotation_since_snapshot(&self) -> Option<f64> {
        let pairs: Vec<((f64, f64), (f64, f64))> = self
            .snapshot
            .iter()
            .filter_map(|&(id, then)| Some((then, self.stars.iter().find(|s| s.id == id)?.offset)))
            .collect();
        if pairs.len() < 2 {
            return None;
        }
        let n = pairs.len() as f64;
        let sum = pairs.iter().fold([0.0; 4], |s, (a, b)| [s[0] + a.0, s[1] + a.1, s[2] + b.0, s[3] + b.1]);
        let [a0, a1, b0, b1] = sum.map(|v| v / n);
        let (mut cross, mut dot, mut spread) = (0.0, 0.0, 0.0);
        for (a, b) in &pairs {
            let (ax, ay, bx, by) = (a.0 - a0, a.1 - a1, b.0 - b0, b.1 - b1);
            cross += ax * by - ay * bx;
            dot += ax * bx + ay * by;
            spread += ax * ax + ay * ay;
        }
        // Stars bunched together say nothing about rotation.
        (spread / n > 20.0 * 20.0).then(|| cross.atan2(dot))
    }
}

fn median(values: impl Iterator<Item = f64>) -> f64 {
    let mut v: Vec<f64> = values.collect();
    v.sort_by(f64::total_cmp);
    let n = v.len();
    if n % 2 == 1 { v[n / 2] } else { (v[n / 2 - 1] + v[n / 2]) / 2.0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic Gaussian-ish noise (sum of uniforms), σ ≈ 1.
    struct Noise(u64);

    impl Noise {
        fn uniform(&mut self) -> f64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 % 1_000_000) as f64 / 1_000_000.0 - 0.5
        }

        fn gauss(&mut self) -> f64 {
            (0..12).map(|_| self.uniform()).sum()
        }
    }

    fn star(x: f64, y: f64, snr: f64) -> Star {
        Star {
            x,
            y,
            flux: snr * 100.0,
            peak: snr * 10.0,
            snr,
            hfd: 3.0,
            saturated: false,
        }
    }

    /// A sky of stars that moves (mount error) and rotates about `pivot`.
    struct Sky {
        stars: Vec<(f64, f64, f64)>,
        pivot: (f64, f64),
    }

    impl Sky {
        fn grid(n: usize) -> Self {
            let stars = (0..n)
                .map(|i| (100.0 + (i % 4) as f64 * 130.0, 80.0 + (i / 4) as f64 * 110.0, 40.0 + 7.0 * i as f64))
                .collect();
            Self {
                stars,
                pivot: (320.0, 240.0),
            }
        }

        /// True positions after shifting by `shift` and rotating by `theta`.
        fn positions(&self, shift: (f64, f64), theta: f64) -> Vec<(f64, f64)> {
            let (s, c) = theta.sin_cos();
            self.stars
                .iter()
                .map(|&(x, y, _)| {
                    let (dx, dy) = (x - self.pivot.0, y - self.pivot.1);
                    (self.pivot.0 + c * dx - s * dy + shift.0, self.pivot.1 + s * dx + c * dy + shift.1)
                })
                .collect()
        }
    }

    /// Track a field: measurements are matched to predictions by proximity,
    /// like the search box in the real guider.
    fn observe(field: &StarField, truth: &[(f64, f64)], snrs: &[f64], noise: &mut Noise, sigma: f64) -> Vec<Option<Star>> {
        field
            .predictions()
            .iter()
            .map(|p| {
                let (i, t) = truth
                    .iter()
                    .enumerate()
                    .min_by(|a, b| (a.1.0 - p.0).hypot(a.1.1 - p.1).total_cmp(&(b.1.0 - p.0).hypot(b.1.1 - p.1)))?;
                ((t.0 - p.0).hypot(t.1 - p.1) < 10.0)
                    .then(|| star(t.0 + sigma * noise.gauss(), t.1 + sigma * noise.gauss(), snrs[i]))
            })
            .collect()
    }

    fn start(sky: &Sky) -> StarField {
        let truth = sky.positions((0.0, 0.0), 0.0);
        let stars: Vec<Star> = truth.iter().zip(&sky.stars).map(|(t, s)| star(t.0, t.1, s.2)).collect();
        let mut field = StarField::new(&stars[0]);
        field.add_candidates(&stars[1..], 15.0, (640, 480));
        field
    }

    #[test]
    fn single_star_is_plain_centroid() {
        let s = star(100.0, 50.0, 30.0);
        let mut field = StarField::new(&s);
        let fix = field.update(&[Some(star(101.5, 49.0, 30.0))]).expect("fix");
        assert_eq!((fix.x, fix.y, fix.used), (101.5, 49.0, 1));
        assert!(field.update(&[None]).is_none());
        assert_eq!(field.len(), 1, "the only star is never dropped");
    }

    #[test]
    fn many_stars_average_down_centroid_noise() {
        let sky = Sky::grid(8);
        let snrs: Vec<f64> = vec![50.0; 8];
        let mut field = start(&sky);
        assert_eq!(field.len(), 8);
        let mut noise = Noise(7);
        let anchor0 = field.anchor();
        let (mut multi, mut single) = (0.0, 0.0);
        let n = 400;
        for f in 0..n {
            let shift = ((f as f64 * 0.1).sin() * 2.0, (f as f64 * 0.07).cos());
            let truth = sky.positions(shift, 0.0);
            let fix = field.update(&observe(&field, &truth, &snrs, &mut noise, 0.3)).expect("fix");
            assert_eq!(fix.used, 8);
            let expect = (anchor0.0 + shift.0, anchor0.1 + shift.1);
            multi += (fix.x - expect.0).powi(2) + (fix.y - expect.1).powi(2);
            let p = (fix.primary.x - truth[0].0, fix.primary.y - truth[0].1);
            single += p.0 * p.0 + p.1 * p.1;
        }
        let (multi, single) = ((multi / n as f64).sqrt(), (single / n as f64).sqrt());
        // Independent noise: about 1/√8 of a single star's error.
        assert!(multi < single * 0.5, "multi {multi:.3} vs single {single:.3}");
    }

    #[test]
    fn field_rotation_does_not_leak_into_guiding() {
        // 1° polar error at 2 s frames: ~0.26°/h; run 4 hours.
        let sky = Sky::grid(8);
        let snrs: Vec<f64> = vec![40.0; 8];
        let mut field = start(&sky);
        assert_eq!(field.rotation(), Some(0.0), "measurable from the start");
        let mut noise = Noise(3);
        let frames = 7200;
        let rate = 0.26f64.to_radians() / 1800.0;
        // Guided: the anchor is held fixed, so the true field translates by
        // whatever keeps it there; check that the estimate never wanders.
        let mut worst: f64 = 0.0;
        let anchor0 = field.anchor();
        for f in 0..frames {
            let theta = rate * f as f64;
            // Where the anchor point physically is, with the field rotating
            // about the pivot and no mount error.
            let truth = sky.positions((0.0, 0.0), theta);
            let fix = field.update(&observe(&field, &truth, &snrs, &mut noise, 0.2)).expect("fix");
            assert_eq!(fix.used, 8, "frame {f}: a star was rejected");
            // The anchor moves only as the stars' weighted centre moves
            // under rotation; guiding sees no growing error beyond that.
            let centre = truth.iter().fold((0.0, 0.0), |a, t| (a.0 + t.0 / 8.0, a.1 + t.1 / 8.0));
            let centre0 = sky.positions((0.0, 0.0), 0.0).iter().fold((0.0, 0.0), |a, t| (a.0 + t.0 / 8.0, a.1 + t.1 / 8.0));
            let expect = (anchor0.0 + centre.0 - centre0.0, anchor0.1 + centre.1 - centre0.1);
            worst = worst.max((fix.x - expect.0).hypot(fix.y - expect.1));
        }
        assert!(worst < 0.4, "anchor wandered {worst:.2} px");
        let rot = field.rotation().expect("rotation").to_degrees();
        assert!((rot - 1.04).abs() < 0.1, "rotation {rot:.3}°");
    }

    #[test]
    fn fixed_references_would_drift_under_rotation() {
        // The control: without adapting offsets the stars disagree after a
        // few hours, i.e. naive averaging against the start breaks down.
        let sky = Sky::grid(8);
        let start_pos = sky.positions((0.0, 0.0), 0.0);
        let later = sky.positions((0.0, 0.0), 1.04f64.to_radians());
        let spread = start_pos
            .iter()
            .zip(&later)
            .map(|(a, b)| (b.0 - a.0).hypot(b.1 - a.1))
            .fold(0.0, f64::max);
        assert!(spread > 3.0, "rotation moved stars by {spread:.1} px");
    }

    #[test]
    fn losing_the_primary_promotes_another_without_a_jump() {
        let sky = Sky::grid(6);
        let snrs = vec![60.0; 6];
        let mut field = start(&sky);
        let mut noise = Noise(11);
        let truth = sky.positions((0.0, 0.0), 0.0);
        let mut last = None;
        for _ in 0..20 {
            last = field.update(&observe(&field, &truth, &snrs, &mut noise, 0.05));
        }
        let before = last.expect("fix");
        let first_primary = field.primary;
        // The primary (star 0) disappears; others stay.
        for f in 0..10 {
            let mut m = observe(&field, &truth, &snrs, &mut noise, 0.05);
            let p = field.stars.iter().position(|s| s.id == first_primary);
            if let Some(p) = p {
                m[p] = None;
            }
            let fix = field.update(&m).expect("guiding continues");
            assert!((fix.x - before.x).hypot(fix.y - before.y) < 0.1, "jump at frame {f}");
            assert_ne!(field.primary, first_primary);
        }
        assert!(field.stars.iter().all(|s| s.id != first_primary), "lost primary dropped");
    }

    #[test]
    fn falls_back_to_single_star_and_back() {
        let sky = Sky::grid(4);
        let snrs = vec![60.0; 4];
        let mut field = start(&sky);
        let mut noise = Noise(5);
        let truth = sky.positions((1.0, -1.0), 0.0);
        let fix = field.update(&observe(&field, &truth, &snrs, &mut noise, 0.05)).expect("fix");
        assert_eq!(fix.used, 4);
        // Only one star survives (thin clouds).
        let mut m = observe(&field, &truth, &snrs, &mut noise, 0.05);
        for x in m.iter_mut().skip(1) {
            *x = None;
        }
        let single = field.update(&m).expect("fix");
        assert_eq!(single.used, 1);
        assert!((single.x - fix.x).hypot(single.y - fix.y) < 0.3);
        // They come back.
        let fix = field.update(&observe(&field, &truth, &snrs, &mut noise, 0.05)).expect("fix");
        assert_eq!(fix.used, 4);
    }

    #[test]
    fn outliers_are_ignored_then_dropped() {
        let sky = Sky::grid(6);
        let snrs = vec![60.0; 6];
        let mut field = start(&sky);
        let mut noise = Noise(9);
        let truth = sky.positions((0.0, 0.0), 0.0);
        let good = field.update(&observe(&field, &truth, &snrs, &mut noise, 0.05)).expect("fix");
        for f in 0..DROP_AFTER {
            let mut m = observe(&field, &truth, &snrs, &mut noise, 0.05);
            // Star 3 latches onto something 4 px away (a satellite, a hot pixel).
            if let Some(Some(s)) = m.get_mut(3) {
                s.x += 4.0;
            }
            let fix = field.update(&m).expect("fix");
            assert!((fix.x - good.x).hypot(fix.y - good.y) < 0.1, "frame {f}");
            assert_eq!(fix.used, 5);
        }
        assert_eq!(field.len(), 5, "inconsistent star dropped");
    }

    #[test]
    fn two_disagreeing_stars_trust_the_primary() {
        let a = star(100.0, 100.0, 30.0);
        let mut field = StarField::new(&a);
        field.add_candidates(&[star(300.0, 200.0, 80.0)], 15.0, (640, 480));
        let fix = field
            .update(&[Some(star(101.0, 100.0, 30.0)), Some(star(306.0, 200.0, 80.0))])
            .expect("fix");
        assert_eq!((fix.x, fix.used), (101.0, 1));
    }

    #[test]
    fn candidates_must_be_isolated_unsaturated_and_inside() {
        let mut field = StarField::new(&star(100.0, 100.0, 50.0));
        let mut sat = star(300.0, 300.0, 90.0);
        sat.saturated = true;
        field.add_candidates(
            &[
                star(105.0, 100.0, 50.0), // next to the primary
                star(200.0, 200.0, 50.0), // a close pair
                star(208.0, 200.0, 50.0),
                star(5.0, 300.0, 50.0), // at the edge
                star(400.0, 100.0, 5.0),  // too faint
                sat,
                star(500.0, 400.0, 30.0), // good
            ],
            15.0,
            (640, 480),
        );
        assert_eq!(field.predictions(), vec![(100.0, 100.0), (500.0, 400.0)]);
        // Already tracked stars are not added twice.
        field.add_candidates(&[star(500.5, 400.0, 30.0)], 15.0, (640, 480));
        assert_eq!(field.len(), 2);
    }

    #[test]
    fn star_count_is_capped() {
        let mut field = StarField::new(&star(20.0, 20.0, 50.0));
        let many: Vec<Star> = (0..40).map(|i| star(50.0 + (i % 8) as f64 * 70.0, 50.0 + (i / 8) as f64 * 80.0, 40.0)).collect();
        field.add_candidates(&many, 15.0, (640, 480));
        assert_eq!(field.len(), MAX_STARS);
        assert!(!field.wants_candidates());
    }
}
