//! Deep-sky object lookup in the engine's catalogue (OpenNGC).

use serde::{Deserialize, Serialize};

/// Most matches the engine returns for one query.
pub const MAX_RESULTS: usize = 8;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CatalogObject {
    /// Main designation, e.g. "M 110" or "NGC 7000".
    pub name: String,
    /// Other designations of the same object, e.g. "NGC 205".
    pub aliases: Vec<String>,
    pub common_names: Vec<String>,
    /// Object type, e.g. "Galaxy" or "Open cluster".
    pub kind: String,
    /// J2000 coordinates.
    pub ra_hours: f64,
    pub dec_deg: f64,
    /// Visual (or else blue) magnitude.
    pub mag: Option<f32>,
}
