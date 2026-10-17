//! Star detection and sub-pixel centroiding on guide frames.

use imaging::Image;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Star {
    pub x: f64,
    pub y: f64,
    /// Background-subtracted total flux (ADU).
    pub flux: f64,
    /// Background-subtracted peak (ADU).
    pub peak: f64,
    pub snr: f64,
    /// Half-flux diameter in pixels.
    pub hfd: f64,
    pub saturated: bool,
}

/// Robust background level and noise of a frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Background {
    pub level: f64,
    pub sigma: f64,
    pub saturation: f64,
}

/// Greyscale view of a frame (CFA data is used as is: fine for guiding).
pub struct Frame<'a> {
    pub width: usize,
    pub height: usize,
    pub data: &'a [u16],
    pub bg: Background,
    /// Measurement aperture radius (px). Larger for defocused stars.
    pub aperture: f64,
}

impl<'a> Frame<'a> {
    pub fn new(img: &'a Image) -> Frame<'a> {
        let analysis = imaging::analyze(img, None);
        let c = &analysis.channels[0];
        let data = img.plane(0);
        Frame {
            width: img.width,
            height: img.height,
            data,
            bg: Background {
                level: c.median,
                sigma: c.sigma().max(1.0),
                saturation: analysis.adc_max as f64 * 0.95,
            },
            aperture: 7.0,
        }
    }

    pub fn with_aperture(mut self, aperture: f64) -> Self {
        self.aperture = aperture.max(3.0);
        self
    }

    fn at(&self, x: usize, y: usize) -> f64 {
        self.data[y * self.width + x] as f64
    }

    /// Measure the star whose peak is inside a box of `radius` around (x, y).
    pub fn measure_near(&self, x: f64, y: f64, radius: usize) -> Option<Star> {
        let (cx, cy) = (x.round() as i64, y.round() as i64);
        let r = radius as i64;
        // Brightest pixel in the search box, smoothed over 3×3 against hot pixels.
        let mut best = None;
        for py in (cy - r).max(1)..=(cy + r).min(self.height as i64 - 2) {
            for px in (cx - r).max(1)..=(cx + r).min(self.width as i64 - 2) {
                let (px, py) = (px as usize, py as usize);
                let mut sum = 0.0;
                for dy in 0..3 {
                    for dx in 0..3 {
                        sum += self.at(px + dx - 1, py + dy - 1);
                    }
                }
                if best.is_none_or(|(s, _, _)| sum > s) {
                    best = Some((sum, px, py));
                }
            }
        }
        let (_, px, py) = best?;
        self.measure_at(px as f64, py as f64)
    }

    /// Centroid and photometry around an approximate position.
    pub fn measure_at(&self, x: f64, y: f64) -> Option<Star> {
        let aperture = self.aperture;
        let (mut cx, mut cy) = (x, y);
        let mut result = None;
        // Re-centre the aperture a few times for sub-pixel accuracy.
        for _ in 0..4 {
            let m = self.moments(cx, cy, aperture)?;
            let moved = (m.x - cx).hypot(m.y - cy);
            cx = m.x;
            cy = m.y;
            result = Some(m);
            if moved < 0.01 {
                break;
            }
        }
        let star = result?;
        let threshold = 3.0 * self.bg.sigma;
        (star.peak > threshold && star.snr > 5.0).then_some(star)
    }

    fn moments(&self, cx: f64, cy: f64, aperture: f64) -> Option<Star> {
        let (x0, x1) = ((cx - aperture).floor().max(0.0) as usize, (cx + aperture).ceil() as usize);
        let (y0, y1) = ((cy - aperture).floor().max(0.0) as usize, (cy + aperture).ceil() as usize);
        if x1 >= self.width || y1 >= self.height {
            return None;
        }
        let r2 = aperture * aperture;
        let (mut sum, mut sx, mut sy, mut peak, mut npix) = (0.0, 0.0, 0.0, 0.0f64, 0usize);
        let mut raw_peak = 0.0f64;
        for y in y0..=y1 {
            for x in x0..=x1 {
                let (dx, dy) = (x as f64 - cx, y as f64 - cy);
                let d2 = dx * dx + dy * dy;
                if d2 > r2 {
                    continue;
                }
                let raw = self.at(x, y);
                raw_peak = raw_peak.max(raw);
                let v = (raw - self.bg.level).max(0.0);
                sum += v;
                sx += v * x as f64;
                sy += v * y as f64;
                peak = peak.max(v);
                npix += 1;
            }
        }
        if sum <= 0.0 {
            return None;
        }
        let (x, y) = (sx / sum, sy / sum);
        // Half-flux radius: grow a circle around the centroid until it holds
        // half of the total flux. Background noise is not clipped here, so it
        // averages out instead of inflating the size of big apertures.
        let mut ring: Vec<(f64, f64)> = Vec::with_capacity(npix);
        for yy in y0..=y1 {
            for xx in x0..=x1 {
                let d = (xx as f64 - x).hypot(yy as f64 - y);
                if d * d <= r2 {
                    ring.push((d, self.at(xx, yy) - self.bg.level));
                }
            }
        }
        ring.sort_by(|a, b| a.0.total_cmp(&b.0));
        let total: f64 = ring.iter().map(|p| p.1).sum();
        if total <= 0.0 {
            return None;
        }
        let mut acc = 0.0;
        let mut hfr = aperture;
        let mut prev_d = 0.0;
        for &(d, v) in &ring {
            if acc + v >= total / 2.0 && v > 0.0 {
                // Interpolate within this pixel's contribution.
                let f = ((total / 2.0 - acc) / v).clamp(0.0, 1.0);
                hfr = prev_d + (d - prev_d) * f;
                break;
            }
            acc += v;
            prev_d = d;
        }
        let noise = self.bg.sigma * (npix as f64).sqrt();
        Some(Star {
            x,
            y,
            flux: total,
            peak,
            snr: total / noise.max(1e-9),
            hfd: 2.0 * hfr,
            saturated: raw_peak >= self.bg.saturation,
        })
    }

    /// Candidate guide stars, best first.
    pub fn detect(&self, max: usize) -> Vec<Star> {
        let threshold = self.bg.level + 5.0 * self.bg.sigma;
        let margin = self.aperture.ceil() as usize + 5;
        let mut peaks: Vec<(usize, usize)> = Vec::new();
        if self.width <= 2 * margin || self.height <= 2 * margin {
            return Vec::new();
        }
        for y in margin..self.height - margin {
            for x in margin..self.width - margin {
                let v = self.at(x, y);
                if v < threshold {
                    continue;
                }
                // Strict local maximum in 5×5 (ties broken by position).
                let mut is_max = true;
                'n: for dy in -2i64..=2 {
                    for dx in -2i64..=2 {
                        if dx == 0 && dy == 0 {
                            continue;
                        }
                        let n = self.at((x as i64 + dx) as usize, (y as i64 + dy) as usize);
                        if n > v || (n == v && (dy < 0 || (dy == 0 && dx < 0))) {
                            is_max = false;
                            break 'n;
                        }
                    }
                }
                if is_max {
                    peaks.push((x, y));
                }
            }
        }
        let mut stars: Vec<Star> = peaks
            .into_iter()
            .filter_map(|(x, y)| self.measure_at(x as f64, y as f64))
            // Hot pixels are sharper than any real star.
            .filter(|s| s.hfd > 1.2)
            .collect();
        // Merge duplicates that converged on the same star.
        let mut unique: Vec<Star> = Vec::new();
        for s in stars.drain(..) {
            if !unique.iter().any(|u| (u.x - s.x).hypot(u.y - s.y) < self.aperture.max(3.0)) {
                unique.push(s);
            }
        }
        // Unsaturated stars first (centroids of clipped stars are unreliable).
        unique.sort_by(|a, b| a.saturated.cmp(&b.saturated).then(score(b).total_cmp(&score(a))));
        unique.truncate(max);
        unique
    }
}

/// Guide star preference among equally (un)saturated stars: bright, compact.
fn score(s: &Star) -> f64 {
    let mut v = s.snr;
    if s.hfd > 8.0 {
        v *= 0.5;
    }
    v
}

/// Star count and typical size in a frame (for focusing).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct StarStats {
    pub count: usize,
    /// Median half-flux radius (px).
    pub hfr: f64,
}

/// Measure stars across a whole frame with an aperture of `aperture` px.
pub fn star_stats(img: &Image, aperture: f64, max_stars: usize) -> Option<StarStats> {
    let frame = Frame::new(img).with_aperture(aperture);
    let mut hfds: Vec<f64> = frame
        .detect(max_stars)
        .iter()
        .filter(|s| !s.saturated && s.snr > 10.0)
        .map(|s| s.hfd)
        .collect();
    if hfds.is_empty() {
        return None;
    }
    hfds.sort_by(f64::total_cmp);
    Some(StarStats {
        count: hfds.len(),
        hfr: hfds[hfds.len() / 2] / 2.0,
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Deterministic pseudo-random noise in [-0.5, 0.5).
    fn noise(i: usize) -> f64 {
        let mut x = (i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0xD1B5_4A32_D192_ED03;
        x ^= x >> 29;
        x = x.wrapping_mul(0xBF58_476D_1CE4_E5B9);
        x ^= x >> 32;
        (x % 10_000) as f64 / 10_000.0 - 0.5
    }

    /// Synthetic frame with Gaussian stars `(x, y, peak)` on a noisy background.
    pub fn render(w: usize, h: usize, stars: &[(f64, f64, f64)], sigma_psf: f64, noise_amp: f64, seed: usize) -> Image {
        let data = (0..w * h)
            .map(|i| {
                let (x, y) = ((i % w) as f64, (i / w) as f64);
                let mut v = 1000.0 + noise_amp * noise(i + seed * 7919);
                for &(sx, sy, peak) in stars {
                    let d2 = (x - sx).powi(2) + (y - sy).powi(2);
                    v += peak * (-d2 / (2.0 * sigma_psf * sigma_psf)).exp();
                }
                v.clamp(0.0, 65535.0) as u16
            })
            .collect();
        Image {
            width: w,
            height: h,
            channels: 1,
            data,
            cfa: None,
        }
    }

    #[test]
    fn subpixel_centroid_accuracy() {
        let mut worst: f64 = 0.0;
        for (i, (x, y)) in [(40.3, 30.7), (41.5, 29.25), (39.9, 31.1), (42.77, 33.33)].iter().enumerate() {
            let img = render(80, 64, &[(*x, *y, 4000.0)], 1.6, 40.0, i);
            let frame = Frame::new(&img);
            let s = frame.measure_near(40.0, 31.0, 10).expect("star found");
            worst = worst.max((s.x - x).hypot(s.y - y));
            assert!((s.hfd - 2.0 * 1.6 * 1.18).abs() < 1.0, "hfd {}", s.hfd);
            assert!(!s.saturated);
        }
        assert!(worst < 0.05, "centroid error {worst} px");
    }

    #[test]
    fn detects_and_ranks_stars() {
        let img = render(
            200,
            150,
            &[(50.0, 40.0, 3000.0), (150.0, 100.0, 9000.0), (100.0, 70.0, 300.0), (5.0, 5.0, 9000.0)],
            1.5,
            20.0,
            3,
        );
        let mut img = img;
        // A hot pixel.
        img.data[120 * 200 + 30] = 60000;
        let frame = Frame::new(&img);
        let stars = frame.detect(10);
        let near = |x: f64, y: f64| stars.iter().position(|s| (s.x - x).hypot(s.y - y) < 1.0);
        assert_eq!(near(150.0, 100.0), Some(0), "{stars:?}");
        assert!(near(50.0, 40.0).is_some());
        assert!(near(5.0, 5.0).is_none(), "edge star rejected");
        assert!(near(30.0, 120.0).is_none(), "hot pixel rejected");
    }

    #[test]
    fn saturated_stars_rank_low() {
        let img = render(200, 150, &[(60.0, 60.0, 70000.0), (140.0, 90.0, 5000.0)], 1.5, 20.0, 5);
        let frame = Frame::new(&img);
        let stars = frame.detect(5);
        assert!((stars[0].x - 140.0).abs() < 0.5, "{stars:?}");
        assert!(stars.iter().any(|s| s.saturated));
    }

    #[test]
    fn hfr_grows_with_defocus() {
        let stars: Vec<(f64, f64, f64)> = (0..12)
            .map(|i| (40.0 + (i % 4) as f64 * 40.0, 35.0 + (i / 4) as f64 * 40.0, 3000.0))
            .collect();
        let mut last = 0.0;
        for sigma in [1.2, 2.0, 3.0, 4.5] {
            // Constant flux: peak falls as the star spreads.
            let s: Vec<_> = stars.iter().map(|&(x, y, p)| (x, y, p * 1.44 / (sigma * sigma))).collect();
            let img = render(200, 150, &s, sigma, 10.0, 9);
            let st = star_stats(&img, 15.0, 50).expect("stars");
            assert_eq!(st.count, 12, "sigma {sigma}");
            assert!(st.hfr > last, "hfr {} at sigma {sigma}", st.hfr);
            // Gaussian HFR ≈ 1.18 σ.
            assert!((st.hfr - 1.177 * sigma).abs() < 0.1 * sigma, "hfr {} at sigma {sigma}", st.hfr);
            last = st.hfr;
        }
    }

    #[test]
    fn no_star_no_result() {
        let img = render(100, 80, &[], 1.5, 20.0, 1);
        let frame = Frame::new(&img);
        assert!(frame.measure_near(50.0, 40.0, 10).is_none());
        assert!(frame.detect(5).is_empty());
    }
}
