//! Autofocus curve fitting.
//!
//! Defocused star size follows a hyperbola, HFR² = A + B·(x − c)², so a
//! least-squares parabola through HFR² gives the best position `c` exactly.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct FocusPoint {
    pub position: f64,
    pub hfr: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FocusFit {
    pub best: f64,
    pub min_hfr: f64,
    /// HFR² = c0 + c1·u + c2·u², with u = (x − origin) / scale.
    coeffs: [f64; 3],
    origin: f64,
    scale: f64,
}

impl FocusFit {
    pub fn hfr_at(&self, x: f64) -> f64 {
        let u = (x - self.origin) / self.scale;
        let [a, b, c] = self.coeffs;
        (a + b * u + c * u * u).max(0.0).sqrt()
    }

    /// Sampled fitted curve for plotting.
    pub fn curve(&self, from: f64, to: f64, n: usize) -> Vec<(f64, f64)> {
        (0..=n)
            .map(|i| {
                let x = from + (to - from) * i as f64 / n.max(1) as f64;
                (x, self.hfr_at(x))
            })
            .collect()
    }
}

/// Least-squares fit; `None` if there are too few points or no minimum.
pub fn fit(points: &[FocusPoint]) -> Option<FocusFit> {
    if points.len() < 3 {
        return None;
    }
    let origin = points.iter().map(|p| p.position).sum::<f64>() / points.len() as f64;
    let scale = points
        .iter()
        .map(|p| (p.position - origin).abs())
        .fold(0.0, f64::max)
        .max(1.0);
    // Normal equations for y = a + b u + c u².
    let mut m = [[0.0; 3]; 3];
    let mut v = [0.0; 3];
    for p in points {
        let u = (p.position - origin) / scale;
        let y = p.hfr * p.hfr;
        let pows = [1.0, u, u * u];
        for i in 0..3 {
            for j in 0..3 {
                m[i][j] += pows[i] * pows[j];
            }
            v[i] += pows[i] * y;
        }
    }
    let coeffs = solve3(m, v)?;
    if coeffs[2] <= 0.0 {
        return None;
    }
    let u_best = -coeffs[1] / (2.0 * coeffs[2]);
    let best = origin + u_best * scale;
    let min_sq = coeffs[0] + coeffs[1] * u_best + coeffs[2] * u_best * u_best;
    Some(FocusFit {
        best,
        min_hfr: min_sq.max(0.0).sqrt(),
        coeffs,
        origin,
        scale,
    })
}

fn solve3(mut m: [[f64; 3]; 3], mut v: [f64; 3]) -> Option<[f64; 3]> {
    for col in 0..3 {
        let pivot = (col..3).max_by(|&a, &b| m[a][col].abs().total_cmp(&m[b][col].abs()))?;
        if m[pivot][col].abs() < 1e-12 {
            return None;
        }
        m.swap(col, pivot);
        v.swap(col, pivot);
        for row in 0..3 {
            if row != col {
                let f = m[row][col] / m[col][col];
                for k in col..3 {
                    m[row][k] -= f * m[col][k];
                }
                v[row] -= f * v[col];
            }
        }
    }
    Some([v[0] / m[0][0], v[1] / m[1][1], v[2] / m[2][2]])
}

/// Positions to sample, outermost first (so every move is inward).
pub fn plan(center: i64, step: i64, steps_out: u32) -> Vec<i64> {
    let n = steps_out as i64;
    (-n..=n).rev().map(|k| center + k * step).collect()
}

#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    /// Move here: best focus.
    Done { best: f64, min_hfr: f64 },
    /// The minimum is at the edge of the sampled range: sample around here.
    Shift { center: i64 },
    Failed(String),
}

/// Decide what to do with the points measured so far over `range`.
pub fn evaluate(points: &[FocusPoint], range: (i64, i64)) -> Verdict {
    if points.len() < 3 {
        return Verdict::Failed(format!("only {} usable frames — not enough stars?", points.len()));
    }
    let lowest = points.iter().min_by(|a, b| a.hfr.total_cmp(&b.hfr)).expect("non-empty");
    let (lo, hi) = (range.0.min(range.1) as f64, range.0.max(range.1) as f64);
    let at_edge = lowest.position <= lo + 0.5 || lowest.position >= hi - 0.5;
    if at_edge {
        return Verdict::Shift {
            center: lowest.position.round() as i64,
        };
    }
    match fit(points) {
        Some(f) if f.best >= lo && f.best <= hi => Verdict::Done {
            best: f.best,
            min_hfr: f.min_hfr,
        },
        Some(f) => Verdict::Failed(format!("fitted focus {:.0} is outside the sampled range", f.best)),
        None => Verdict::Failed("the measurements do not form a V-curve".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hyperbola(best: f64, x: f64) -> f64 {
        (1.5f64.powi(2) + (0.01 * (x - best)).powi(2)).sqrt()
    }

    #[test]
    fn fit_recovers_hyperbola_minimum() {
        let pts: Vec<FocusPoint> = plan(5000, 100, 4)
            .into_iter()
            .map(|p| FocusPoint { position: p as f64, hfr: hyperbola(5037.0, p as f64) })
            .collect();
        let f = fit(&pts).unwrap();
        assert!((f.best - 5037.0).abs() < 0.5, "{}", f.best);
        assert!((f.min_hfr - 1.5).abs() < 0.01);
        assert!((f.hfr_at(5437.0) - hyperbola(5037.0, 5437.0)).abs() < 0.01);
    }

    #[test]
    fn plan_goes_inward() {
        assert_eq!(plan(1000, 50, 2), vec![1100, 1050, 1000, 950, 900]);
    }

    #[test]
    fn evaluate_shifts_then_finishes() {
        let measure = |center: i64| -> Vec<FocusPoint> {
            plan(center, 100, 4)
                .into_iter()
                .map(|p| FocusPoint { position: p as f64, hfr: hyperbola(5000.0, p as f64) })
                .collect()
        };
        // Started far inside focus: the best sample is the outermost one.
        let first = measure(4400);
        assert_eq!(evaluate(&first, (4800, 4000)), Verdict::Shift { center: 4800 });
        let second = measure(4800);
        match evaluate(&second, (5200, 4400)) {
            Verdict::Done { best, .. } => assert!((best - 5000.0).abs() < 1.0, "{best}"),
            other => panic!("{other:?}"),
        }
        assert!(matches!(evaluate(&first[..2], (4800, 4000)), Verdict::Failed(_)));
    }

    #[test]
    fn noisy_curve_still_close() {
        let pts: Vec<FocusPoint> = plan(5000, 100, 5)
            .into_iter()
            .enumerate()
            .map(|(i, p)| FocusPoint {
                position: p as f64,
                hfr: hyperbola(4980.0, p as f64) * (1.0 + 0.04 * if i % 2 == 0 { 1.0 } else { -1.0 }),
            })
            .collect();
        let f = fit(&pts).unwrap();
        assert!((f.best - 4980.0).abs() < 30.0, "{}", f.best);
    }
}
