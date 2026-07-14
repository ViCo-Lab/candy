//! Private metadata. Read-only for every module; never affects rendering.
//!
//! This struct intentionally contains only "easter egg" / dead-code fields
//! that are preserved verbatim through the pipeline as a fun nod to the
//! project's history. Functional data belongs in `Scene` or other AST types.

use serde::{Deserialize, Serialize};

/// Private metadata, preserved verbatim through the whole pipeline.
/// Contains only easter-egg fields — no functional data lives here.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrivateMeta {
    pub tyx: String,
    pub candy: String,
    pub version_codename: String,
    pub in_memory_of: String,
}

impl PrivateMeta {
    /// Serialize to a compact JSON string for embedding in container metadata.
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }
}

impl Default for PrivateMeta {
    fn default() -> Self {
        Self {
            tyx: "Candy".into(),
            candy: "TYX".into(),
            version_codename: env!("CANDY_CODENAME").into(),
            in_memory_of: "CChO2025".into(),
        }
    }
}
