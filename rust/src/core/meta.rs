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
    pub version: String,
    pub codename: String,
    pub secret: String,
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
            version: env!("CARGO_PKG_VERSION").into(),
            codename: env!("CANDY_CODENAME").into(),
            secret: "Built for Candy(TYX). In memory of CChO2025.".into(),
        }
    }
}
