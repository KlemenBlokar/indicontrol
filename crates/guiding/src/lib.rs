//! Autoguiding: star detection, centroiding, calibration and guide algorithms.
//! Pure computation (no I/O), so it can be tested against simulated mounts.

pub mod calibration;
pub mod focus;
pub mod guide;
pub mod settle;
pub mod star;

pub use focus::{FocusFit, FocusPoint, Verdict as FocusVerdict};
pub use calibration::{CalStep, Calibration, CalibrationSettings, Calibrator};
pub use guide::{Correction, DecMode, GuideController, GuideDir, GuideParams, GuideStats};
pub use settle::{DitherRng, SettleOutcome, SettleTracker, dither_offset};
pub use star::{Background, Frame, Star, StarStats, star_stats};
