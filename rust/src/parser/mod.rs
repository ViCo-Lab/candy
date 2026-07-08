//! `parser` — turn inputs into a `Scene` AST (spec §4.1).
//!
//! Two entry points:
//! * [`parse_tyx`] — parse a `.tyx` X-sheet (Typst + Candy directives) directly.
//! * [`extract_dsl_from_svg`] — recover a `Scene` from an SVG rendered by the
//!   `@preview/candy` Typst package (the `candy-json` block).

pub mod dsl;
pub mod tyx;

pub use dsl::extract_dsl_from_svg;
pub use tyx::parse_tyx;
