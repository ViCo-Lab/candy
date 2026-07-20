//! CPU SVG → pixels rasterization for the frame render path.
//!
//! The renderer produces one standard SVG per frame (see
//! [`crate::renderer::typst::Renderer::render_frame_at`]); this module
//! rasterizes it to an RGBA8 buffer in a single pass. The per-glyph `#transform`
//! overlay is embedded in that same SVG (never rasterized separately), so only
//! this one rasterization touches pixels — keeping memory and CPU bounded.

use crate::core::diag::CandyError;
use crate::renderer::RenderedFrame;

/// Rasterize a complete SVG document to a `width × height` RGBA8 buffer in a
/// single pass.
///
/// The SVG root carries `width`/`height` in *point* units (the scene's page
/// size) with a matching `viewBox`. We rewrite the root viewport to the target
/// pixel size (leaving the `viewBox` in pt) so `usvg` applies the
/// viewBox→viewport scale and the scene fills the whole pixmap.
pub(crate) fn rasterize_svg(
    svg: &str,
    width: u32,
    height: u32,
) -> Result<RenderedFrame, CandyError> {
    let svg = set_svg_viewport_px(svg, width, height);
    let tree = usvg::Tree::from_str(&svg, &usvg::Options::default())
        .map_err(|e| CandyError::Encode(format!("usvg parse: {e}")))?;
    let mut pixmap = tiny_skia::Pixmap::new(width, height)
        .ok_or_else(|| CandyError::Encode("failed to allocate pixmap".into()))?;
    resvg::render(
        &tree,
        tiny_skia::Transform::identity(),
        &mut pixmap.as_mut(),
    );
    // Zero-copy: consume the pixmap's inner Vec instead of cloning.
    let rgba = pixmap.take();
    Ok(RenderedFrame {
        width: width as usize,
        height: height as usize,
        rgba,
    })
}

/// Rewrite the root `<svg>` element's `width`/`height` (the viewport) to the
/// given pixel dimensions, leaving the `viewBox` (in pt) untouched.
///
/// `usvg` fits the `viewBox` into the viewport via a scale transform, so this
/// is what maps the scene's point-space geometry to the pixel-sized render
/// target. Only the first `width`/`height` attributes — those on the opening
/// `<svg ...>` tag — are touched; child elements live after the closing `>`
/// and are never affected.
pub(crate) fn set_svg_viewport_px(svg: &str, w: u32, h: u32) -> String {
    let open = match svg.find("<svg") {
        Some(i) => i,
        None => return svg.to_string(),
    };
    let close = match svg[open..].find('>') {
        Some(i) => open + i,
        None => return svg.to_string(),
    };
    let tag = &svg[open..=close];
    let tag = replace_attr(tag, "width", w);
    let tag = replace_attr(&tag, "height", h);
    let mut out = String::with_capacity(svg.len());
    out.push_str(&svg[..open]);
    out.push_str(&tag);
    out.push_str(&svg[close + 1..]);
    out
}

/// Replace the first `name="..."` attribute value within `s` with `value`.
fn replace_attr(s: &str, name: &str, value: u32) -> String {
    let needle = format!("{name}=\"");
    let start = match s.find(&needle) {
        Some(i) => i,
        None => return s.to_string(),
    };
    let val_start = start + needle.len();
    let end = match s[val_start..].find('"') {
        Some(i) => val_start + i,
        None => return s.to_string(),
    };
    let mut out = String::with_capacity(s.len());
    out.push_str(&s[..val_start]);
    out.push_str(&value.to_string());
    out.push_str(&s[end..]);
    out
}
