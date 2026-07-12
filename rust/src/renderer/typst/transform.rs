//! Per-glyph `#transform` plan types and the Typst source builders for placing
//! a single mobject body (and for morph outlines).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::core::ast::Label;
use crate::core::easing::Easing;
use crate::renderer::RenderedFrame;

/// One animated glyph/decoration in a per-glyph `transform`. The fragment is
/// cropped from the *whole* old or new formula render (so it keeps Typst's
/// exact metrics — script sizes, true fraction bars, …) and moved from
/// `(from_x, from_y)` to `(to_x, to_y)` (body-relative cm) while its opacity
/// goes `from_op → to_op`.
pub(crate) struct GlyphAnim {
    /// Which formula to crop from: `0` = old, `1` = new.
    pub(crate) src: u8,
    /// Source clip box, in the formula's local coords (pt; the formula is
    /// rendered at page origin so local == page coords).
    pub(crate) bx0: f64,
    pub(crate) by0: f64,
    pub(crate) bx1: f64,
    pub(crate) by1: f64,
    /// Interpolated target top-left, body-relative (cm).
    pub(crate) from_x: f64,
    pub(crate) from_y: f64,
    pub(crate) to_x: f64,
    pub(crate) to_y: f64,
    pub(crate) from_op: f64,
    pub(crate) to_op: f64,
}

/// A precomputed per-glyph `Transform` layout for one `#transform(target, to:
/// …)` call whose old/new bodies are inline content (formula / text). Built
/// once in `ensure_natural` by rendering the whole old and new formulas and
/// extracting each glyph/decoration as a positioned fragment (via Typst's own
/// SVG layout — no custom parser). During `[start_ms, end_ms)` the render
/// paths composite the interpolated fragments *over* `target` so the old
/// content disassembles and reassembles into the new content (Manim-style).
pub(crate) struct TransformFragmentPlan {
    pub(crate) target: Label,
    pub(crate) old: Label,
    pub(crate) start_ms: u32,
    pub(crate) end_ms: u32,
    pub(crate) easing: Easing,
    /// Raw bodies (used by the pixel path to rasterize the whole formulas).
    pub(crate) old_body: String,
    pub(crate) new_body: String,
    /// SVG inner markup of the old / new formulas (used by the SVG path).
    pub(crate) old_inner: String,
    pub(crate) new_inner: String,
    /// Per-glyph animation fragments.
    pub(crate) anims: Vec<GlyphAnim>,
    /// Pixel-path cache of whole-formula RGBA, keyed by `(which: 0/1, ppi_q)`.
    pub(crate) formula_cache: Mutex<HashMap<(u8, u32), Arc<RenderedFrame>>>,
}

/// Translate a ring so its bounding-box top-left sits at the origin. Morph
/// outlines are interpolated in this local frame and later placed (via
/// `place_source`) at the target mobject's natural top-left, so the morph is
/// anchored correctly and matches standard Typst positioning at `t = 1`.
pub(crate) fn localize_ring(ring: Vec<[f64; 2]>) -> Vec<[f64; 2]> {
    if ring.is_empty() {
        return ring;
    }
    let mut min_x = f64::INFINITY;
    let mut min_y = f64::INFINITY;
    for p in &ring {
        if p[0] < min_x {
            min_x = p[0];
        }
        if p[1] < min_y {
            min_y = p[1];
        }
    }
    ring.into_iter()
        .map(|p| [p[0] - min_x, p[1] - min_y])
        .collect()
}

/// Convert an SVG paint color string (as captured from a Typst-rendered SVG,
/// which uses hex like `#0074d9` or `rgb(...)`) into a Typst color expression
/// that is safe to embed in code mode (e.g. inside `polygon(fill: …, …)`).
///
/// Typst's `#` is a code-mode marker, so a raw `#0074d9` would be a syntax
/// error — we wrap hex colors as `rgb("#0074d9")`, which Typst accepts.
pub(crate) fn svg_color_to_typst(color: &str) -> String {
    let c = color.trim();
    if let Some(hex) = c.strip_prefix('#') {
        // `#rrggbb` / `#rrggbbaa` → rgb("#…")
        format!("rgb(\"#{hex}\")")
    } else if c.starts_with("rgb(") || c.starts_with("rgba(") || c.starts_with("hsl(") {
        // Already a valid Typst color expression.
        c.to_string()
    } else {
        // Named color (`red`, `blue`, …) or anything else — pass through.
        c.to_string()
    }
}

/// Build a Typst `polygon(...)` body (no leading `#`) from a ring, preserving
/// the target shape's paint. Points are emitted as absolute `(x*pt, y*pt)`.
pub(crate) fn polygon_svg(ring: &[[f64; 2]], fill: &Option<String>, stroke: &Option<String>) -> String {
    let pts: Vec<String> = ring
        .iter()
        .map(|p| format!("({:.2}pt, {:.2}pt)", p[0], p[1]))
        .collect();
    let fill = svg_color_to_typst(fill.clone().unwrap_or_else(|| "black".to_string()).as_str());
    let stroke_attr = match stroke {
        Some(s) => format!(", stroke: {}", svg_color_to_typst(s)),
        None => String::new(),
    };
    format!(
        "polygon(fill: {fill}{stroke_attr}, {pts})",
        pts = pts.join(", ")
    )
}

/// Build the Typst source that places a single mobject body at `(x_cm, y_cm)`
/// from the top-left corner, scaled by `scale_pct`% and rotated by `rotation`
/// degrees (clockwise, around the object's top-left origin).
///
/// When `rotation == 0.0` the `rotate(..)` wrapper is omitted, keeping the
/// generated source minimal for the common case (and matching the v0.1 output
/// exactly, so existing SVG drafts are byte-identical when no rotation is
/// applied).
pub(crate) fn place_source(
    page_w: f64,
    page_h: f64,
    x_cm: f64,
    y_cm: f64,
    scale_pct: f64,
    rotation: f64,
    body: &str,
    preamble: &str,
) -> String {
    // The body is a raw Typst expression (e.g. "rect(width: 2cm, fill: red)")
    // captured from the .tyx source. Inside a content block `[...]`, function
    // calls MUST be prefixed with `#` — otherwise Typst treats them as plain
    // text. We add the `#` here so the body renders as an object, not text.
    let pre = if preamble.is_empty() {
        String::new()
    } else {
        format!("{preamble}\n")
    };
    if rotation.abs() < 1e-9 {
        format!(
            "{pre}#set page(width: {page_w}pt, height: {page_h}pt, margin: 0pt, fill: none)\n\
             #place(top + left, dx: {x_cm}cm, dy: {y_cm}cm)[#scale(origin: top + left, {scale_pct}%)[#{body}]]\n"
        )
    } else {
        format!(
            "{pre}#set page(width: {page_w}pt, height: {page_h}pt, margin: 0pt, fill: none)\n\
             #place(top + left, dx: {x_cm}cm, dy: {y_cm}cm)[#scale(origin: top + left, {scale_pct}%)[#rotate(origin: top + left, {rotation}deg)[#{body}]]]\n"
        )
    }
}
