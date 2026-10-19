//! Mount position and state, as reported by the engine.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MountInfo {
    pub name: String,
    /// Apparent (JNow) coordinates.
    pub ra: f64,
    pub dec: f64,
    pub ra_j2000: f64,
    pub dec_j2000: f64,
    pub alt: Option<f64>,
    /// Hour angle in hours, -12..12 (negative: east of the meridian).
    pub hour_angle: Option<f64>,
    pub lst: Option<f64>,
    pub pier_side: Option<String>,
    pub tracking: Option<bool>,
    pub parked: Option<bool>,
    pub slewing: bool,
}

/// Angular distance between two positions (hours/degrees in, arcminutes out).
pub fn separation_arcmin(ra1: f64, dec1: f64, ra2: f64, dec2: f64) -> f64 {
    let (a1, d1, a2, d2) = (
        (ra1 * 15.0).to_radians(),
        dec1.to_radians(),
        (ra2 * 15.0).to_radians(),
        dec2.to_radians(),
    );
    // Haversine: stable for small angles.
    let h = ((d2 - d1) / 2.0).sin().powi(2) + d1.cos() * d2.cos() * ((a2 - a1) / 2.0).sin().powi(2);
    (2.0 * h.sqrt().asin()).to_degrees() * 60.0
}

impl MountInfo {
    /// Minutes until the target crosses the meridian (negative once past).
    pub fn minutes_to_meridian(&self) -> Option<f64> {
        self.hour_angle.map(|ha| -ha * 60.0 * 0.997_269_6)
    }
}
