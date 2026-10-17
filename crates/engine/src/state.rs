//! Engine settings persisted across restarts.

use std::path::{Path, PathBuf};

use protocol::guider::Calibration;
use protocol::{FlatRecord, FocusSettings, GuiderSettings, ImagingSettings, Roles, Sequence, SolverSettings};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct IndiSettings {
    pub host: String,
    pub port: u16,
    /// Connect on startup and reconnect after failures.
    pub autoconnect: bool,
}

impl Default for IndiSettings {
    fn default() -> Self {
        Self {
            host: "localhost".into(),
            port: indi_link::DEFAULT_PORT,
            autoconnect: false,
        }
    }
}

/// A saved rig: everything that differs between setups.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Profile {
    pub name: String,
    pub indi: IndiSettings,
    pub roles: Roles,
    pub imaging: ImagingSettings,
    pub guider: GuiderSettings,
    pub guider_calibration: Option<Calibration>,
    pub solver: SolverSettings,
    pub focus: FocusSettings,
    pub flat_exposures: Vec<FlatRecord>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PersistedState {
    pub indi: IndiSettings,
    pub roles: Roles,
    pub imaging: ImagingSettings,
    pub sequences: Vec<Sequence>,
    /// Exposures found by flat auto-exposure, for matching dark flats.
    pub flat_exposures: Vec<FlatRecord>,
    pub guider: GuiderSettings,
    pub guider_calibration: Option<Calibration>,
    pub solver: SolverSettings,
    pub focus: FocusSettings,
    pub profiles: Vec<Profile>,
    pub active_profile: Option<String>,
}

impl PersistedState {
    /// The current rig as a profile.
    pub fn snapshot(&self, name: &str) -> Profile {
        Profile {
            name: name.to_owned(),
            indi: self.indi.clone(),
            roles: self.roles.clone(),
            imaging: self.imaging.clone(),
            guider: self.guider.clone(),
            guider_calibration: self.guider_calibration.clone(),
            solver: self.solver.clone(),
            focus: self.focus.clone(),
            flat_exposures: self.flat_exposures.clone(),
        }
    }

    pub fn apply(&mut self, p: &Profile) {
        self.indi = p.indi.clone();
        self.roles = p.roles.clone();
        self.imaging = p.imaging.clone();
        self.guider = p.guider.clone();
        self.guider_calibration = p.guider_calibration.clone();
        self.solver = p.solver.clone();
        self.focus = p.focus.clone();
        self.flat_exposures = p.flat_exposures.clone();
        self.active_profile = Some(p.name.clone());
        self.fill_defaults();
    }
}

/// `~/Pictures/indicontrol` (or `~/indicontrol` without a pictures dir).
pub fn default_output_dir() -> PathBuf {
    dirs::picture_dir()
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("indicontrol")
}

pub fn default_state_file() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("indicontrol").join("engine.json"))
}

impl PersistedState {
    pub fn load(path: &Path) -> Self {
        let mut state = match std::fs::read_to_string(path) {
            Ok(s) => serde_json::from_str(&s).unwrap_or_else(|e| {
                tracing::warn!("ignoring unreadable state file {}: {e}", path.display());
                Self::default()
            }),
            Err(_) => Self::default(),
        };
        state.fill_defaults();
        state
    }

    pub fn fill_defaults(&mut self) {
        if self.imaging.output_dir.trim().is_empty() {
            self.imaging.output_dir = default_output_dir().display().to_string();
        }
    }

    pub fn save(&self, path: &Path) {
        let result = path
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|_| {
                // Write-then-rename so a crash never leaves a truncated file.
                let tmp = path.with_extension("json.tmp");
                std::fs::write(&tmp, serde_json::to_vec_pretty(self).expect("serializable"))?;
                std::fs::rename(tmp, path)
            });
        if let Err(e) = result {
            tracing::warn!("failed to save state to {}: {e}", path.display());
        }
    }
}
