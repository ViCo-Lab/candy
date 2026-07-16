//! Alpha-compositing ("over" operator), offset paste, formula-region crop, and
//! formula-id localization for the per-glyph transform path.

use crate::renderer::RenderedFrame;

/// Composite a (possibly transparent) source frame over an opaque destination
/// canvas using the "over" operator, scaled by `opacity`.
///
/// Kept as the canonical top-left paste; the cropped-sprite path now uses
/// [`composite_over_at`], but this remains the simplest reference compositor.
#[allow(dead_code)]
pub(crate) fn composite_over(
    dst: &mut [u8],
    src: &RenderedFrame,
    opacity: f64,
    w: usize,
    h: usize,
) {
    let op = opacity.clamp(0.0, 1.0);
    for y in 0..h.min(src.height) {
        for x in 0..w.min(src.width) {
            let di = (y * w + x) * 4;
            let si = (y * src.width + x) * 4;
            let sa = (src.rgba[si + 3] as f32 / 255.0) * op as f32;
            if sa <= 0.0 {
                continue;
            }
            for c in 0..3 {
                let s = src.rgba[si + c] as f32;
                let d = dst[di + c] as f32;
                dst[di + c] = (s * sa + d * (1.0 - sa)).round() as u8;
            }
            dst[di + 3] = 255;
        }
    }
}

/// Like `composite_over` but pastes `src` at an explicit pixel offset `(ox, oy)`
/// (may be negative / partially off-canvas) instead of the top-left.
pub(crate) fn composite_over_at(
    dst: &mut [u8],
    src: &RenderedFrame,
    opacity: f64,
    ox: f64,
    oy: f64,
    w: usize,
    h: usize,
) {
    let op = opacity.clamp(0.0, 1.0);
    let ox = ox.round() as i64;
    let oy = oy.round() as i64;
    for y in 0..src.height as i64 {
        let dy = oy + y;
        if dy < 0 || dy >= h as i64 {
            continue;
        }
        for x in 0..src.width as i64 {
            let dx = ox + x;
            if dx < 0 || dx >= w as i64 {
                continue;
            }
            let di = (dy * w as i64 + dx) as usize * 4;
            let si = (y * src.width as i64 + x) as usize * 4;
            let sa = (src.rgba[si + 3] as f32 / 255.0) * op as f32;
            if sa <= 0.0 {
                continue;
            }
            for c in 0..3 {
                let s = src.rgba[si + c] as f32;
                let d = dst[di + c] as f32;
                dst[di + c] = (s * sa + d * (1.0 - sa)).round() as u8;
            }
            dst[di + 3] = 255;
        }
    }
}

/// Rewrite every `id="X"` in `markup` to `id="{prefix}X"` and every
/// `xlink:href="#X"` / `href="#X"` to the prefixed form, so two formulas that
/// both define `glyph0`, … can be embedded in the same SVG document without
/// their symbol definitions colliding.
pub(crate) fn localize_formula_ids(markup: &str, prefix: &str) -> String {
    // Collect all ids defined in this markup.
    let mut ids: Vec<String> = Vec::new();
    let mut i = 0;
    while let Some(pos) = markup[i..].find("id=\"") {
        let start = i + pos + 4;
        if let Some(end) = markup[start..].find('"') {
            ids.push(markup[start..start + end].to_string());
            i = start + end + 1;
        } else {
            break;
        }
    }
    // Longest first so an id that is a prefix of another is rewritten after it.
    ids.sort_by(|a, b| b.len().cmp(&a.len()));
    let mut out = markup.to_string();
    for id in &ids {
        out = out.replace(&format!("id=\"{id}\""), &format!("id=\"{prefix}{id}\""));
        out = out.replace(
            &format!("xlink:href=\"#{id}\""),
            &format!("xlink:href=\"#{prefix}{id}\""),
        );
        out = out.replace(&format!("href=\"#{id}\""), &format!("href=\"#{prefix}{id}\""));
    }
    out
}
