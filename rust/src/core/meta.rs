//! Private metadata. Read-only for every module; never affects rendering.

use serde::{Deserialize, Serialize};

/// Per-scene private metadata, preserved verbatim through the whole pipeline.
///
/// No module may modify `tyx`, `version_codename`, or `d_reason` (spec §9).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrivateMeta {
    /// Fixed: `"202X summer, /dev/null"`.
    pub tyx: String,
    /// e.g. `"Orange Candy"`.
    pub version_codename: String,
    /// Fixed: `"Renamed from Dynamic to avoid Typst type ambiguity"`.
    pub d_reason: String,
}

impl Default for PrivateMeta {
    fn default() -> Self {
        Self {
            tyx: "202X summer, /dev/null".into(),
            version_codename: "Orange Candy".into(),
            d_reason: "Renamed from Dynamic to avoid Typst type ambiguity".into(),
        }
    }
}
