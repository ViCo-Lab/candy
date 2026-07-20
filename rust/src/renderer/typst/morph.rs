//! Shape-`#morph` rendering helpers.
//!
//! These are the renderer-side utilities that turn a morphed outline ring into
//! something Typst can draw: localizing a ring to its bounding-box origin
//! ([`localize_ring`]), converting an SVG paint color to a Typst color
//! expression ([`svg_color_to_typst`]), and emitting a `polygon(...)`
//! body ([`polygon_svg`]). They live here — not in the per-glyph
//! `#transform` module (`transform.rs`) — because they belong to the morph
//! path, not the transform path.

/// Translate a ring so its bounding-box top-left sits at the origin. Morph
/// outlines are interpolated in this local frame and later placed (via
/// `place_source`) at the target mobject's flow top-left, so the morph is
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
pub(crate) fn polygon_svg(
    ring: &[[f64; 2]],
    fill: &Option<String>,
    stroke: &Option<String>,
) -> String {
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
