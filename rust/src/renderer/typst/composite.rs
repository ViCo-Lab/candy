//! Alpha-compositing ("over" operator), offset paste, formula-region crop, and
//! formula-id localization for the per-glyph transform path.

use crate::renderer::RenderedFrame;

/// Composite a (possibly transparent) source frame over an opaque destination
/// canvas using the "over" operator, scaled by `opacity`.
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
#[allow(dead_code)]
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

/// Crop a rectangular region (in Typst pt, page coords) out of a page-sized
/// `RenderedFrame`, returning a small RGBA whose top-left is the crop's top-left.
///
/// A `pad_pt` margin is added on every side before rounding so glyph ink that
/// overflows its strict path bbox (strokes, anti-aliasing, italic overshoot)
/// is not clipped — the previous exact-bbox crop was slicing the edges off
/// characters.
pub(crate) fn crop_formula_rgba(
    whole: &RenderedFrame,
    bx0: f64,
    by0: f64,
    bx1: f64,
    by1: f64,
    ppi: f32,
) -> RenderedFrame {
    let pad = 1.0; // pt, generous enough for strokes + AA
    let px0 = ((bx0 - pad) * ppi as f64).floor() as i64;
    let py0 = ((by0 - pad) * ppi as f64).floor() as i64;
    let px1 = ((bx1 + pad) * ppi as f64).ceil() as i64;
    let py1 = ((by1 + pad) * ppi as f64).ceil() as i64;
    let w = (px1 - px0).max(1) as usize;
    let h = (py1 - py0).max(1) as usize;
    let sw = whole.width as i64;
    let sh = whole.height as i64;
    let mut out = vec![0u8; w * h * 4];
    for y in 0..h as i64 {
        let sy = py0 + y;
        if sy < 0 || sy >= sh {
            continue;
        }
        for x in 0..w as i64 {
            let sx = px0 + x;
            if sx < 0 || sx >= sw {
                continue;
            }
            let di = ((y * w as i64 + x) * 4) as usize;
            let si = ((sy * sw + sx) * 4) as usize;
            out[di..di + 4].copy_from_slice(&whole.rgba[si..si + 4]);
        }
    }
    RenderedFrame {
        width: w,
        height: h,
        rgba: out,
    }
}

/// Like `composite_over_at` but applies a 2-D affine transform `(a,b,c,d)`
/// (SVG `matrix` order: `x' = a*x + c*y`, `y' = b*x + d*y`, in destination
/// pixels) to each source pixel, pasting the result at `(ox, oy)` (the
/// transform's pivot is the source's center). Bilinear sampling keeps rotated /
/// scaled glyphs smooth. `opacity` scales the source alpha. Used to let a
/// per-glyph `transform` inherit the target mobject's `#animate` scale /
/// rotation so the two animations compose.
pub(crate) fn composite_over_at_xf(
    dst: &mut [u8],
    src: &RenderedFrame,
    opacity: f64,
    ox: f64,
    oy: f64,
    a: f64,
    b: f64,
    c: f64,
    d: f64,
    w: usize,
    h: usize,
) {
    let op = opacity.clamp(0.0, 1.0);
    let sw = src.width as f64;
    let sh = src.height as f64;
    // Sample the source in its own center-origin frame so scale/rotate pivot
    // at the glyph's middle (matches SVG `transform-origin: center`).
    let cx = sw / 2.0;
    let cy = sh / 2.0;
    // Inverse matrix (src = M^-1 * (dst - origin)).
    let det = a * d - b * c;
    if det.abs() < 1e-9 {
        return;
    }
    let ia = d / det;
    let ib = -b / det;
    let ic = -c / det;
    let id = a / det;
    // `ox, oy` is the destination (canvas px) of the source crop's *center*
    // (the caller passes the glyph center). The inverse map below,
    // `src = M^-1 * (dst - (ox, oy)) + (cx, cy)`, already places the source
    // center `(cx, cy)` at `(ox, oy)` — so do NOT add `(cx, cy)` here. Adding
    // it shifted every fragment by half its crop size (down-right), smearing
    // the formula into a ghost. (The SVG path never applied this shift, which
    // is why only the pixel/MP4 path was wrong.)
    for dy in 0..h as i64 {
        for dx in 0..w as i64 {
            // map destination pixel (relative to origin) back into source space
            let rx = (dx as f64) - ox;
            let ry = (dy as f64) - oy;
            let sx = ia * rx + ic * ry + cx;
            let sy = ib * rx + id * ry + cy;
            if sx < 0.0 || sy < 0.0 || sx > sw - 1.0 || sy > sh - 1.0 {
                continue;
            }
            let x0 = sx.floor() as usize;
            let y0 = sy.floor() as usize;
            let x1 = (x0 + 1).min(src.width - 1);
            let y1 = (y0 + 1).min(src.height - 1);
            let fx = sx - x0 as f64;
            let fy = sy - y0 as f64;
            let di = ((dy * w as i64 + dx) * 4) as usize;
            // bilinear blend of the 4 neighbours' alpha-weighted rgb
            let mut acc_r = 0.0f32;
            let mut acc_g = 0.0f32;
            let mut acc_b = 0.0f32;
            let mut acc_a = 0.0f32;
            for (nx, nf_x) in [(x0, 1.0 - fx), (x1, fx)].into_iter() {
                for (ny, nf_y) in [(y0, 1.0 - fy), (y1, fy)].into_iter() {
                    let si = (ny * src.width + nx) * 4;
                    let wgt = (nf_x * nf_y) as f32;
                    acc_r += src.rgba[si] as f32 * wgt;
                    acc_g += src.rgba[si + 1] as f32 * wgt;
                    acc_b += src.rgba[si + 2] as f32 * wgt;
                    acc_a += src.rgba[si + 3] as f32 * wgt;
                }
            }
            let sa = (acc_a / 255.0) * op as f32;
            if sa <= 0.0 {
                continue;
            }
            for c in 0..3 {
                let s = if c == 0 { acc_r } else if c == 1 { acc_g } else { acc_b };
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
