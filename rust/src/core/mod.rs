//! `core` — pure data structures and transformation logic.
//!
//! No I/O, no rendering, no external crates beyond `serde`. Every function here
//! is deterministic and side-effect free, which makes the mandatory pipeline
//! assertions cheap to verify.

pub mod ast;
pub mod easing;
pub mod error;
pub mod interpolator;
pub mod meta;
pub mod scheduler;

pub use ast::*;
pub use easing::Easing;
pub use error::CandyError;
