//! CPU SVG → pixels rasterization for the per-glyph `#transform` overlay.
//!
//! The overlay is kept as SVG (`<path>` / `<use>` fragments) and rasterized
//! **once** at the final step — replacing the old per-fragment pixel path
//! (`transform_fragment_frames`), which rasterized the whole formula once *per
//! fragment* and composited each crop. Keeping the overlay as SVG means the
//! formula is embedded only once (in `<defs>`, reused via `<use>`), and only
//! this single rasterization touches pixels, so memory and CPU stay bounded.

use crate::core::diag::CandyError;
use crate::renderer::RenderedFrame;

/// Rasterize a complete SVG document to a `width × height` RGBA8 buffer in a
/// single pass. The SVG root is expected to carry `width`/`height` (px) matching
/// `width`/`height` and a `viewBox` in the document's native coordinate space
/// (pt); `usvg` applies the viewBox→viewport scale, so an identity root
/// transform fills the pixmap exactly.
///
/// The pixmap starts transparent, so the result can be alpha-composited over a
/// base frame (the `#transform` overlay is drawn on top of the mobjects it
/// replaces).
pub(crate) fn rasterize_svg_once(
    svg: &str,
    width: u32,
    height: u32,
) -> Result<RenderedFrame, CandyError> {
    let tree = usvg::Tree::from_str(svg, &usvg::Options::default())
        .map_err(|e| CandyError::Encode(format!("usvg parse: {e}")))?;
    let mut pixmap = tiny_skia::Pixmap::new(width, height)
        .ok_or_else(|| CandyError::Encode("failed to allocate pixmap".into()))?;
    // Identity root transform: the SVG's own viewBox→viewport scale (applied by
    // usvg during parsing) already maps the document into the pixmap.
    resvg::render(&tree, tiny_skia::Transform::identity(), &mut pixmap.as_mut());
    Ok(RenderedFrame {
        width: width as usize,
        height: height as usize,
        rgba: pixmap.data().to_vec(),
    })
}
