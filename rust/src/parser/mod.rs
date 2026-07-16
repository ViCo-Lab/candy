//! `parser` — turn inputs into a `Scene` AST (spec §4.1).
//!
//! Two entry points:
//! * [`parse_tyx`] — parse a `.tyx` X-sheet (Typst + Candy directives) directly.
//! * [`extract_scene_from_svg`] — recover a `Scene` from an SVG rendered by the
//!   `@preview/candy` Typst package (the `candy-json` block).
//!
//! The `.tyx` parser is split into focused submodules:
//! * [`ast_walk`] — the orchestration layer: the `ParseCtx` accumulator, the
//!   tree walker, scope/scene bookkeeping, and import handling.
//! * [`directives`] — one handler per Candy directive, plus the `process_call`
//!   dispatcher.
//! * [`expr`] — pure expression evaluation (lengths, numbers, tuples, arrays)
//!   and Candy-symbol resolution (`call_symbol`).
//! * [`svg`] — SVG → `Scene` extraction.

pub mod ast_walk;
pub mod directives;
pub mod expr;
pub mod svg;

pub use ast_walk::parse_tyx;
pub use svg::extract_scene_from_svg;
