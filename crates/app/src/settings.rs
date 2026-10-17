//! Persistent user settings (stored by eframe between sessions).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Roles {
    pub main_camera: Option<String>,
    pub guide_camera: Option<String>,
    pub mount: Option<String>,
    pub filter_wheel: Option<String>,
    pub focuser: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub host: String,
    pub port: u16,
    pub roles: Roles,
    pub night_mode: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            host: "localhost".into(),
            port: indi_link::DEFAULT_PORT,
            roles: Roles::default(),
            night_mode: false,
        }
    }
}
