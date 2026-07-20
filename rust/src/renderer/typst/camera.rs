//! Global `#camera` pan/zoom/rotate: the SVG `<g transform>` string applied to
//! the whole scene.

use crate::core::ast::FrameData;

use super::PT_PER_CM;

/// Synthetic label used by `#camera` (a global pan/zoom/rotate transform).
/// Never rendered as an object — the renderer reads its per-frame state and
/// applies it as a wrapping transform over the whole scene.
pub(crate) const CAMERA_LABEL: &str = "__camera__";

/// SVG `<g transform>` attribute for the camera (pan + zoom + rotate about the
/// page center), in Typst points.
pub(crate) fn camera_transform_svg(cam: &FrameData, page_w: f64, page_h: f64) -> String {
    let (cx, cy) = (page_w / 2.0, page_h / 2.0);
    let ncx = -cx;
    let ncy = -cy;
    let dx = cam.x * PT_PER_CM;
    let dy = cam.y * PT_PER_CM;
    let s = cam.scale;
    let r = cam.rotation;
    format!(
        "translate({cx} {cy}) rotate({r}) scale({s}) translate({ncx} {ncy}) translate({dx} {dy})"
    )
}
