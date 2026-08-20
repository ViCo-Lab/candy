//! Shape-`#morph` rendering helpers.
//!
//! [`localize_ring`] translates an interpolated morph outline ring so its
//! bounding-box top-left sits at the origin, which is required because the
//! ring is later placed at the target mobject's flow position. It lives here
//! rather than in the per-glyph `#transform` module (`transform.rs`) because
//! it belongs to the morph path.

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
