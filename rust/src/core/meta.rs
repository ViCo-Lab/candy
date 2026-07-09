//! Private metadata. Read-only for every module; never affects rendering.

use serde::{Deserialize, Serialize};

/// Per-scene private metadata, preserved verbatim through the whole pipeline.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrivateMeta {
    pub tyx: String,
    pub candy: String,
    pub version_codename: String,
    pub in_memory_of: String,
}

impl Default for PrivateMeta {
    fn default() -> Self {
        Self {
            tyx: "Candy".into(),
            candy: "TYX".into(),
            version_codename: "Ribose".into(),
            in_memory_of: "CChO2025".into(),
        }
    }
}
