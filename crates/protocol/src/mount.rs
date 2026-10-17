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

impl MountInfo {
    /// Minutes until the target crosses the meridian (negative once past).
    pub fn minutes_to_meridian(&self) -> Option<f64> {
        self.hour_angle.map(|ha| -ha * 60.0 * 0.997_269_6)
    }
}
