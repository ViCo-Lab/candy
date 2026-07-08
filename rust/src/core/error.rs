//! Error handling. All fallible operations return `Result<T, CandyError>`;
//! production code must not panic (spec §6).

use std::fmt;

use crate::core::ast::Label;

/// Candy's unified error type. The `code()` method maps each variant to the
/// mandatory error codes E001–E007.
#[derive(Debug)]
pub enum CandyError {
    /// E001 — `.tyx` file not found / generic I/O failure.
    Io(std::io::Error),
    /// E002 — Invalid `.tyx` syntax.
    Parse(String),
    /// E003 — `candy-json` missing/invalid (DSL extraction).
    Dsl(String),
    /// E004 — `@label` not found in the Typst layout.
    LabelNotFound(Label),
    /// E005 — Invalid interpolation range (clamped, not fatal).
    Interp(String),
    /// E006 — Typst render failure.
    Typst(String),
    /// E007 — Rav1e encoding failure.
    Encode(String),
}

impl CandyError {
    /// Mandatory error code (spec §6).
    pub fn code(&self) -> &'static str {
        match self {
            CandyError::Io(_) => "E001",
            CandyError::Parse(_) => "E002",
            CandyError::Dsl(_) => "E003",
            CandyError::LabelNotFound(_) => "E004",
            CandyError::Interp(_) => "E005",
            CandyError::Typst(_) => "E006",
            CandyError::Encode(_) => "E007",
        }
    }
}

impl fmt::Display for CandyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CandyError::Io(e) => write!(f, "[E001] I/O error: {e}"),
            CandyError::Parse(e) => write!(f, "[E002] Invalid .tyx syntax: {e}"),
            CandyError::Dsl(e) => write!(f, "[E003] candy-json missing/invalid: {e}"),
            CandyError::LabelNotFound(l) => {
                write!(f, "[E004] label @{} not found in Typst layout", l.0)
            }
            CandyError::Interp(e) => write!(f, "[E005] interpolation range: {e}"),
            CandyError::Typst(e) => write!(f, "[E006] Typst render failure: {e}"),
            CandyError::Encode(e) => write!(f, "[E007] rav1e encoding failure: {e}"),
        }
    }
}

impl std::error::Error for CandyError {}

impl From<std::io::Error> for CandyError {
    fn from(e: std::io::Error) -> Self {
        // A missing file is the canonical E001 trigger.
        CandyError::Io(e)
    }
}

impl From<serde_json::Error> for CandyError {
    fn from(e: serde_json::Error) -> Self {
        CandyError::Dsl(e.to_string())
    }
}
