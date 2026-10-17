//! Autoguider messages.

use serde::{Deserialize, Serialize};

pub use guiding::{
    Calibration, CalibrationSettings, DecMode, GuideDir, GuideParams, GuideStats, Star,
};

/// Where guide pulses are sent.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum GuideOutput {
    /// Pulse guiding through the mount driver.
    #[default]
    Mount,
    /// ST-4 port of the guide camera.
    GuideCamera,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GuiderSettings {
    pub exposure: f64,
    /// Guide scope focal length (mm); `None` reads it from INDI.
    pub focal_length: Option<f64>,
    /// Guide camera pixel size (µm); `None` reads it from INDI.
    pub pixel_size: Option<f64>,
    pub output: GuideOutput,
    pub calibration: CalibrationSettings,
    pub params: GuideParams,
    /// How far (px) the star may move between frames and still be found.
    pub search_radius: f64,
    /// Consecutive frames without the star before guiding stops.
    pub max_lost_frames: u32,
    /// Pause after a pulse before the next exposure (ms).
    pub settle_ms: f64,
    /// Maximum dither offset along each axis (guide pixels).
    pub dither_px: f64,
    pub dither_ra_only: bool,
    /// Settled when the error stays below this (guide pixels)…
    pub settle_px: f64,
    /// …for this long (seconds).
    pub settle_time: f64,
    /// Give up waiting for settling after this long (seconds).
    pub settle_timeout: f64,
    /// Reuse the calibration after a meridian flip by turning it 180°.
    pub flip_calibration: bool,
    /// Also reverse Dec when flipping (uncheck if the mount reverses Dec itself).
    pub flip_dec: bool,
}

impl Default for GuiderSettings {
    fn default() -> Self {
        Self {
            exposure: 2.0,
            focal_length: None,
            pixel_size: None,
            output: GuideOutput::Mount,
            calibration: CalibrationSettings::default(),
            params: GuideParams::default(),
            search_radius: 15.0,
            max_lost_frames: 10,
            settle_ms: 100.0,
            dither_px: 3.0,
            dither_ra_only: false,
            settle_px: 1.5,
            settle_time: 10.0,
            settle_timeout: 60.0,
            flip_calibration: true,
            flip_dec: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state")]
pub enum GuiderState {
    Idle,
    Looping,
    Calibrating,
    Guiding,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GuiderStatus {
    pub state: GuiderState,
    pub camera: Option<String>,
    pub frame_size: Option<(usize, usize)>,
    pub star: Option<Star>,
    pub lock: Option<(f64, f64)>,
    pub calibration: Option<Calibration>,
    /// Arcseconds per guide-camera pixel, if focal length and pixel size are known.
    pub pixel_scale: Option<f64>,
    /// Recent guiding errors (pixels).
    pub stats: GuideStats,
    /// Waiting for guiding to settle (after a dither or start).
    pub settling: bool,
    pub message: String,
}

impl Default for GuiderStatus {
    fn default() -> Self {
        Self {
            state: GuiderState::Idle,
            camera: None,
            frame_size: None,
            star: None,
            lock: None,
            calibration: None,
            pixel_scale: None,
            stats: GuideStats::default(),
            settling: false,
            message: String::new(),
        }
    }
}

/// One guide cycle.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GuideStep {
    pub frame: u64,
    /// Seconds since guiding started.
    pub time: f64,
    /// Error along the mount axes (pixels).
    pub ra_px: f64,
    pub dec_px: f64,
    pub ra_pulse: Option<(GuideDir, f64)>,
    pub dec_pulse: Option<(GuideDir, f64)>,
    pub snr: f64,
    pub star_lost: bool,
}
