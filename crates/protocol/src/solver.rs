//! Plate solving with an external solver (ASTAP or astrometry.net).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum SolverKind {
    #[default]
    Astap,
    AstrometryNet,
}

impl SolverKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Astap => "ASTAP",
            Self::AstrometryNet => "astrometry.net",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SolverSettings {
    pub kind: SolverKind,
    /// ASTAP executable (`astap` or `astap_cli`).
    pub astap_path: String,
    /// astrometry.net `solve-field` executable.
    pub astrometry_path: String,
    /// Search radius around the mount position (degrees).
    pub search_radius: f64,
    /// Downsample factor passed to the solver (0 = solver default).
    pub downsample: u32,
    pub timeout: f64,
    /// Extra command-line arguments, split on whitespace.
    pub extra_args: String,
    /// Solve exposure on the main camera.
    pub exposure: f64,
    pub binning: Option<u32>,
    pub gain: Option<f64>,
    /// Filter for solve frames (`None` keeps the current one).
    pub filter_slot: Option<u32>,
    /// Centering is done when within this distance of the target (arcmin).
    pub center_tolerance: f64,
    pub center_attempts: u32,
}

impl Default for SolverSettings {
    fn default() -> Self {
        Self {
            kind: SolverKind::Astap,
            astap_path: "astap".into(),
            astrometry_path: "solve-field".into(),
            search_radius: 10.0,
            downsample: 2,
            timeout: 120.0,
            extra_args: String::new(),
            exposure: 5.0,
            binning: Some(2),
            gain: None,
            filter_slot: None,
            center_tolerance: 1.0,
            center_attempts: 5,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SolveResult {
    /// Image centre, J2000.
    pub ra_hours: f64,
    pub dec_deg: f64,
    /// Arcseconds per (binned) pixel.
    pub pixel_scale: f64,
    /// Angle of the image "up" direction from north through east (degrees).
    pub rotation: f64,
    /// The image is mirrored relative to the sky.
    pub mirrored: bool,
    pub width_deg: f64,
    pub height_deg: f64,
    pub solver: SolverKind,
    pub seconds: f64,
    /// Distance between the mount's idea of the position and the solution (arcmin).
    pub pointing_error: Option<f64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SolveStatus {
    pub busy: bool,
    pub message: String,
    pub last: Option<SolveResult>,
}

/// Optional sequence target, centred by plate solving.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Target {
    /// J2000.
    pub ra_hours: f64,
    pub dec_deg: f64,
}
