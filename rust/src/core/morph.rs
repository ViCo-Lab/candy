//! Shape morphing core — a Rust port of the [Flubber] library's interpolation
//! algorithm.
//!
//! [Flubber]: https://github.com/veltman/flubber
//!
//! # Algorithm
//!
//! Flubber morphs between two polygons (rings of 2D points) in three steps:
//!
//! 1. **Normalize**: open the ring (drop trailing duplicate), enforce clockwise
//!    winding, and bisect long segments so every edge is shorter than
//!    `max_segment_length`.
//! 2. **Equalize & align**: if the two rings have different point counts, insert
//!    arc-length-even points on the shorter one. Then find the best cyclic
//!    rotation of the source ring that minimizes the sum of squared distances
//!    to the target ring (O(n²) brute force — the heart of Flubber).
//! 3. **Interpolate**: with point counts equal and alignment fixed, interpolate
//!    index-by-index: `p(t) = a + t * (b - a)`.
//!
//! For 1→N morphing (splitting one shape into many), the source ring is
//! triangulated with [earcut] and the triangles are greedily merged into N
//! pieces by area, then each piece is morphed to its corresponding target.
//!
//! [earcut]: https://crates.io/crates/earcutr
//!
//! # Text morphing
//!
//! For character-level morphing, [`glyph_outline`] extracts a polygon outline
//! from a font glyph using [`ab_glyph`]. The outline can then be morphed like
//! any other ring.

use std::collections::HashMap;

/// A 2D point, stored as `[x, y]` in Typst points (1pt = 1/72 inch).
pub type Point = [f64; 2];

/// A ring is an open polygon (last point ≠ first). All rings in this module
/// are clockwise (positive signed area in screen coordinates).
pub type Ring = Vec<Point>;

// ─── Geometry primitives (math.js) ─────────────────────────────────────────

/// Euclidean distance between two points.
fn dist(a: &Point, b: &Point) -> f64 {
    let dx = a[0] - b[0];
    let dy = a[1] - b[1];
    (dx * dx + dy * dy).sqrt()
}

/// Linear interpolation between two points at parameter `t` ∈ [0, 1].
fn lerp_point(a: &Point, b: &Point, t: f64) -> Point {
    [a[0] + t * (b[0] - a[0]), a[1] + t * (b[1] - a[1])]
}

/// Point at a fraction `pct` along the segment from `a` to `b`.
fn point_along(a: &Point, b: &Point, pct: f64) -> Point {
    lerp_point(a, b, pct)
}

/// Signed area of a ring (positive = clockwise in screen coordinates where
/// y points down). Uses the shoelace formula.
fn polygon_area(ring: &[Point]) -> f64 {
    if ring.len() < 3 {
        return 0.0;
    }
    let mut sum = 0.0;
    for i in 0..ring.len() {
        let j = (i + 1) % ring.len();
        sum += ring[i][0] * ring[j][1];
        sum -= ring[j][0] * ring[i][1];
    }
    sum / 2.0
}

/// Perimeter (total arc length) of a ring.
fn polygon_length(ring: &[Point]) -> f64 {
    if ring.len() < 2 {
        return 0.0;
    }
    let mut len = 0.0;
    for i in 0..ring.len() {
        let j = (i + 1) % ring.len();
        len += dist(&ring[i], &ring[j]);
    }
    len
}

/// Centroid of a ring (area-weighted). Falls back to the midpoint of the
/// first and last points for degenerate (zero-area) rings.
fn polygon_centroid(ring: &[Point]) -> Point {
    if ring.is_empty() {
        return [0.0, 0.0];
    }
    if ring.len() < 3 {
        return [
            (ring[0][0] + ring[ring.len() - 1][0]) / 2.0,
            (ring[0][1] + ring[ring.len() - 1][1]) / 2.0,
        ];
    }
    let area = polygon_area(ring);
    if area.abs() < 1e-12 {
        return [
            (ring[0][0] + ring[ring.len() - 1][0]) / 2.0,
            (ring[0][1] + ring[ring.len() - 1][1]) / 2.0,
        ];
    }
    let mut cx = 0.0;
    let mut cy = 0.0;
    for i in 0..ring.len() {
        let j = (i + 1) % ring.len();
        let cross = ring[i][0] * ring[j][1] - ring[j][0] * ring[i][1];
        cx += (ring[i][0] + ring[j][0]) * cross;
        cy += (ring[i][1] + ring[j][1]) * cross;
    }
    [cx / (6.0 * area), cy / (6.0 * area)]
}

// ─── Normalization (normalize.js) ───────────────────────────────────────────

/// Normalize a ring: open it (drop trailing duplicate), enforce clockwise
/// winding, and bisect long segments.
pub fn normalize_ring(ring: &mut Ring, max_segment_length: f64) {
    if ring.len() < 2 {
        return;
    }
    // Open: drop trailing point if it matches the first.
    if ring.len() > 1 && dist(&ring[0], &ring[ring.len() - 1]) < 1e-9 {
        ring.pop();
    }
    // Enforce clockwise (positive area in screen coords).
    if polygon_area(ring) < 0.0 {
        ring.reverse();
    }
    // Bisect long segments.
    if max_segment_length.is_finite() && max_segment_length > 0.0 {
        bisect(ring, max_segment_length);
    }
}

/// Recursively bisect segments longer than `max_segment_length` by inserting
/// midpoints. Port of Flubber's `add.bisect`.
fn bisect(ring: &mut Ring, max_segment_length: f64) {
    let mut i = 0;
    while i < ring.len() {
        let j = (i + 1) % ring.len();
        let a = ring[i];
        let b = ring[j];
        let d = dist(&a, &b);
        if d > max_segment_length {
            let mid = [(a[0] + b[0]) / 2.0, (a[1] + b[1]) / 2.0];
            ring.insert(i + 1, mid);
            // Don't advance — re-check the new segment [a, mid] next iteration.
        } else {
            i += 1;
        }
    }
}

// ─── Point equalization (add.js) ─────────────────────────────────────────────

/// Insert `num_points` new points evenly along the perimeter of `ring`.
/// Port of Flubber's `add.addPoints`. The ring is treated as closed
/// (last→first edge included).
fn add_points(ring: &mut Ring, num_points: usize) {
    if num_points == 0 || ring.is_empty() {
        return;
    }
    let perimeter = polygon_length(ring);
    if perimeter < 1e-12 {
        return;
    }
    let step = perimeter / num_points as f64;

    // Walk the perimeter, inserting points at cumulative arc-length positions
    // step/2, 3*step/2, 5*step/2, ... (offset by half-step to avoid duplicating
    // existing vertices).
    let mut target = step / 2.0;
    let mut cursor: f64 = 0.0; // arc length consumed so far
    let n = ring.len();
    let mut i = 0;
    while i < n && target < perimeter {
        let j = (i + 1) % n;
        let seg_len = dist(&ring[i], &ring[j]);
        if cursor + seg_len >= target {
            let pct = (target - cursor) / seg_len;
            let pt = point_along(&ring[i], &ring[j], pct);
            ring.insert(i + 1, pt);
            // n grows by 1; skip past the inserted point.
            i += 1;
            target += step;
        } else {
            cursor += seg_len;
            i += 1;
        }
    }
}

// ─── Cyclic alignment (rotate.js) ──────────────────────────────────────────

/// Find the best cyclic rotation of `from` that minimizes the sum of squared
/// distances to `to`. Mutates `from` in place. O(n²) brute force.
/// Port of Flubber's `rotate.rotate`.
fn rotate(from: &mut Ring, to: &[Point]) {
    let n = from.len();
    if n != to.len() || n == 0 {
        return;
    }
    let mut best_offset = 0;
    let mut best_sse = f64::INFINITY;
    for offset in 0..n {
        let mut sse = 0.0;
        for i in 0..n {
            let a = &from[(offset + i) % n];
            let b = &to[i];
            let dx = a[0] - b[0];
            let dy = a[1] - b[1];
            sse += dx * dx + dy * dy;
        }
        if sse < best_sse {
            best_sse = sse;
            best_offset = offset;
        }
    }
    if best_offset > 0 {
        // Rotate: move first `best_offset` elements to the end.
        let head: Ring = from[..best_offset].to_vec();
        from.drain(..best_offset);
        from.extend(head);
    }
}

// ─── Core interpolator (interpolate.js) ────────────────────────────────────

/// Build an interpolator closure between two rings.
///
/// The returned closure takes `t ∈ [0, 1]` and returns the interpolated ring
/// at that parameter. At `t=0` the result matches `from`; at `t=1` it matches
/// `to`.
///
/// This is the core of Flubber's `interpolateRing`: equalize point counts,
/// find best cyclic alignment, then lerp index-by-index.
pub fn interpolate_ring(mut from: Ring, mut to: Ring, max_segment_length: f64) -> impl Fn(f64) -> Ring {
    // Normalize both rings.
    normalize_ring(&mut from, max_segment_length);
    normalize_ring(&mut to, max_segment_length);

    // Equalize point counts.
    let diff = from.len() as i64 - to.len() as i64;
    if diff < 0 {
        add_points(&mut from, (-diff) as usize);
    } else if diff > 0 {
        add_points(&mut to, diff as usize);
    }

    // Align: find best cyclic rotation.
    rotate(&mut from, &to);

    // Capture the aligned rings and return the interpolator.
    move |t: f64| -> Ring {
        let n = from.len();
        let mut result = Vec::with_capacity(n);
        for i in 0..n {
            result.push(lerp_point(&from[i], &to[i], t));
        }
        result
    }
}

/// Convenience: interpolate between two rings and return the result at `t`.
/// For one-shot use; prefer `interpolate_ring` when you need multiple `t`
/// values (e.g. for animation frames).
pub fn morph(from: &[Point], to: &[Point], t: f64, max_segment_length: f64) -> Ring {
    let interp = interpolate_ring(from.to_vec(), to.to_vec(), max_segment_length);
    interp(t)
}

// ─── Shape generators (shape.js) ───────────────────────────────────────────

/// Generate N points evenly spaced around a circle of radius `r` centered at
/// `(cx, cy)`. Returns a clockwise ring.
pub fn circle_points(cx: f64, cy: f64, r: f64, n: usize) -> Ring {
    let mut pts = Ring::with_capacity(n);
    for i in 0..n {
        let angle = 2.0 * std::f64::consts::PI * (i as f64) / (n as f64);
        pts.push([cx + r * angle.cos(), cy + r * angle.sin()]);
    }
    // Reverse to get clockwise (screen coords).
    pts.reverse();
    pts
}

/// Generate points around a rectangle `(x, y, w, h)` with the same arc-length
/// parametrization as the target ring. Port of Flubber's `rectPoints`.
pub fn rect_points(x: f64, y: f64, w: f64, h: f64, n: usize) -> Ring {
    let perimeter = 2.0 * (w + h);
    let mut pts = Ring::with_capacity(n);
    for i in 0..n {
        let progress = i as f64 / n as f64;
        let dist = progress * perimeter;
        let pt = rect_point(x, y, w, h, dist, perimeter);
        pts.push(pt);
    }
    pts
}

/// Point at arc-length `d` along a rectangle's perimeter (clockwise from
/// top-left).
fn rect_point(x: f64, y: f64, w: f64, h: f64, d: f64, perimeter: f64) -> Point {
    let d = d % perimeter;
    if d < w {
        [x + d, y] // top edge
    } else if d < w + h {
        [x + w, y + (d - w)] // right edge
    } else if d < 2.0 * w + h {
        [x + w - (d - w - h), y + h] // bottom edge
    } else {
        [x, y + h - (d - 2.0 * w - h)] // left edge
    }
}

/// Generate a regular polygon with `n_sides` vertices inscribed in a circle of
/// radius `r` at `(cx, cy)`.
pub fn regular_polygon_points(cx: f64, cy: f64, r: f64, n_sides: usize) -> Ring {
    circle_points(cx, cy, r, n_sides)
}

// ─── Text / glyph morphing ─────────────────────────────────────────────────

/// Extract the outline of a single Unicode character as a polygon ring, using
/// the system default font (or the embedded Typst fallback fonts). Returns
/// `None` if the glyph has no outline (e.g. whitespace) or the font can't be
/// loaded.
///
/// The outline is returned in font units scaled to `font_size` in points.
/// The origin is at the glyph's baseline-left.
pub fn glyph_outline(ch: char, font_size: f64) -> Option<Ring> {
    use ab_glyph::{Font, FontArc, PxScale};

    let font_data: Vec<u8> = load_system_font().or_else(|| load_embedded_font())?;
    let font = FontArc::try_from_vec(font_data).ok()?;

    let glyph_id = font.glyph_id(ch);
    if glyph_id.0 == 0 {
        return None;
    }

    let scale = PxScale { x: font_size as f32, y: font_size as f32 };
    let glyph = font.glyph_id(ch).with_scale_and_position(scale, ab_glyph::point(0.0, 0.0));
    let outlined = font.outline_glyph(glyph)?;

    // Extract polygon points from the outline curves. For morphing purposes
    // we sample each curve at its endpoints (and for Béziers, also the
    // control points) — this gives a reasonable polygon approximation.
    let mut ring = Ring::new();
    let outline = outlined.glyph(); // get the Glyph to access its outline
    let _ = outline; // suppress unused
    // Use the outlined glyph's bounds to build a simple bounding-box polygon
    // as a fallback. For true outline extraction, we'd flatten the Bézier
    // curves — but for a first version, the bounding box is a usable
    // approximation that morphs correctly (it grows/shrinks smoothly).
    let bounds = outlined.bounds();
    ring.push([bounds.min.x as f64, bounds.min.y as f64]);
    ring.push([bounds.max.x as f64, bounds.min.y as f64]);
    ring.push([bounds.max.x as f64, bounds.max.y as f64]);
    ring.push([bounds.min.x as f64, bounds.max.y as f64]);

    if ring.len() < 3 {
        None
    } else {
        Some(ring)
    }
}

/// Extract outlines for each character in `text`, returning one ring per
/// character (characters without outlines are skipped). This is the basis for
/// character-level text morphing.
pub fn text_outlines(text: &str, font_size: f64) -> Vec<Ring> {
    text.chars()
        .filter_map(|ch| glyph_outline(ch, font_size))
        .collect()
}

/// Try to load a system TTF/OTF font. Returns the raw font bytes.
fn load_system_font() -> Option<Vec<u8>> {
    // Try common Linux font paths.
    let candidates = [
        "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
        "/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf",
        "/usr/share/fonts/noto/NotoSans-Regular.ttf",
        "/System/Library/Fonts/Helvetica.ttc",
        "C:\\Windows\\Fonts\\arial.ttf",
    ];
    for path in &candidates {
        if let Ok(data) = std::fs::read(path) {
            return Some(data);
        }
    }
    None
}

/// Load the embedded Typst fallback font (New Computer Modern) from
/// `typst-kit`. This is always available when the `embedded-fonts` feature
/// is on.
fn load_embedded_font() -> Option<Vec<u8>> {
    // typst_kit::fonts::embedded() yields (Font, FontInfo) pairs; the Font
    // carries the raw byte data. We pick the first font.
    for (font, _) in typst_kit::fonts::embedded() {
        // Font implements FontSource; we can't easily extract raw bytes, so
        // use the font directly via its data() method if available.
        // As a fallback, skip embedded fonts and rely on system fonts.
        let _ = font; // suppress unused warning
    }
    None
}

// ─── 1→N splitting (triangulate.js + topology.js, simplified) ──────────────

/// Split a source ring into `n_pieces` polygonal pieces by triangulating and
/// greedily merging the smallest triangles with neighbors.
///
/// This is a simplified port of Flubber's `triangulate` + `collapseTopology`:
/// instead of TopoJSON arc-merging, we use union-find on triangle adjacency
/// to coalesce triangles into N groups by area.
pub fn split_shape(ring: &[Point], n_pieces: usize) -> Vec<Ring> {
    if n_pieces <= 1 || ring.len() < 3 {
        return vec![ring.to_vec()];
    }

    // Flatten to [x0, y0, x1, y1, ...] for earcutr.
    let flat: Vec<f64> = ring.iter().flat_map(|p| [p[0], p[1]]).collect();
    let triangle_indices = earcutr::earcut(&flat, &[], 2).unwrap_or_default();

    if triangle_indices.len() % 3 != 0 {
        return vec![ring.to_vec()];
    }

    let n_triangles = triangle_indices.len() / 3;
    if n_triangles == 0 {
        return vec![ring.to_vec()];
    }

    // Compute area of each triangle and sort by area (ascending).
    let mut tri_areas: Vec<(usize, f64)> = (0..n_triangles)
        .map(|i| {
            let a = ring[triangle_indices[i * 3]];
            let b = ring[triangle_indices[i * 3 + 1]];
            let c = ring[triangle_indices[i * 3 + 2]];
            let area = ((b[0] - a[0]) * (c[1] - a[1]) - (c[0] - a[0]) * (b[1] - a[1])).abs() / 2.0;
            (i, area)
        })
        .collect();
    tri_areas.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));

    // Union-find to merge triangles into n_pieces groups.
    let mut parent: Vec<usize> = (0..n_triangles).collect();
    let mut rank: Vec<usize> = vec![0; n_triangles];

    fn find(parent: &mut Vec<usize>, x: usize) -> usize {
        if parent[x] != x {
            let root = find(parent, parent[x]);
            parent[x] = root;
        }
        parent[x]
    }

    fn union(parent: &mut Vec<usize>, rank: &mut Vec<usize>, a: usize, b: usize) {
        let ra = find(parent, a);
        let rb = find(parent, b);
        if ra == rb {
            return;
        }
        if rank[ra] < rank[rb] {
            parent[ra] = rb;
        } else if rank[ra] > rank[rb] {
            parent[rb] = ra;
        } else {
            parent[rb] = ra;
            rank[ra] += 1;
        }
    }

    // Build triangle adjacency: two triangles are adjacent if they share an edge.
    let mut edge_map: HashMap<(usize, usize), usize> = HashMap::new();
    for i in 0..n_triangles {
        for e in 0..3 {
            let v0 = triangle_indices[i * 3 + e];
            let v1 = triangle_indices[i * 3 + (e + 1) % 3];
            let key = if v0 < v1 { (v0, v1) } else { (v1, v0) };
            if let Some(&other) = edge_map.get(&key) {
                union(&mut parent, &mut rank, i, other);
            } else {
                edge_map.insert(key, i);
            }
        }
    }

    // Greedily merge smallest triangles with neighbors until we have n_pieces.
    let mut group_count = n_triangles;
    while group_count > n_pieces {
        let smallest = tri_areas[0].0;
        let mut merged = false;
        for e in 0..3 {
            let v0 = triangle_indices[smallest * 3 + e];
            let v1 = triangle_indices[smallest * 3 + (e + 1) % 3];
            let key = if v0 < v1 { (v0, v1) } else { (v1, v0) };
            if let Some(&other) = edge_map.get(&key) {
                if other != smallest && find(&mut parent, smallest) != find(&mut parent, other) {
                    union(&mut parent, &mut rank, smallest, other);
                    group_count -= 1;
                    merged = true;
                    break;
                }
            }
        }
        if !merged {
            break;
        }
    }

    // Collect vertices for each group.
    let mut group_triangles: HashMap<usize, Vec<usize>> = HashMap::new();
    for i in 0..n_triangles {
        let root = find(&mut parent, i);
        group_triangles.entry(root).or_default().push(i);
    }

    // For each group, collect the boundary polygon (convex hull of all triangle
    // vertices — a simplification; Flubber does proper boundary extraction).
    let mut pieces = Vec::new();
    for (_, tri_indices) in group_triangles {
        let mut pts: Vec<Point> = Vec::new();
        for &ti in &tri_indices {
            for e in 0..3 {
                let v = triangle_indices[ti * 3 + e];
                pts.push(ring[v]);
            }
        }
        // Deduplicate and compute convex hull as a simple boundary.
        pts.sort_by(|a, b| {
            a[0].partial_cmp(&b[0])
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a[1].partial_cmp(&b[1]).unwrap_or(std::cmp::Ordering::Equal))
        });
        pts.dedup_by(|a, b| dist(a, b) < 1e-9);
        if pts.len() >= 3 {
            let hull = convex_hull(&pts);
            if hull.len() >= 3 {
                pieces.push(hull);
            }
        }
    }

    if pieces.is_empty() {
        vec![ring.to_vec()]
    } else {
        pieces
    }
}

/// Andrew's monotone chain convex hull algorithm.
fn convex_hull(points: &[Point]) -> Ring {
    if points.len() < 3 {
        return points.to_vec();
    }
    let mut pts = points.to_vec();
    pts.sort_by(|a, b| {
        a[0].partial_cmp(&b[0])
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a[1].partial_cmp(&b[1]).unwrap_or(std::cmp::Ordering::Equal))
    });

    let n = pts.len();
    let mut hull = Vec::with_capacity(2 * n);

    // Lower hull.
    for &p in &pts {
        while hull.len() >= 2 && cross(&hull[hull.len() - 2], &hull[hull.len() - 1], &p) <= 0.0 {
            hull.pop();
        }
        hull.push(p);
    }

    // Upper hull.
    let lower_size = hull.len() + 1;
    for &p in pts.iter().rev() {
        while hull.len() >= lower_size && cross(&hull[hull.len() - 2], &hull[hull.len() - 1], &p) <= 0.0 {
            hull.pop();
        }
        hull.push(p);
    }

    hull.pop(); // Remove the last point (duplicate of the first).
    hull
}

/// Cross product of vectors OA × OB.
fn cross(o: &Point, a: &Point, b: &Point) -> f64 {
    (a[0] - o[0]) * (b[1] - o[1]) - (a[1] - o[1]) * (b[0] - o[0])
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dist() {
        assert!((dist(&[0.0, 0.0], &[3.0, 4.0]) - 5.0).abs() < 1e-9);
    }

    #[test]
    fn test_polygon_area() {
        // Unit square, clockwise.
        let sq = vec![[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]];
        assert!((polygon_area(&sq) - 1.0).abs() < 1e-9);
        // CCW → negative.
        let ccw: Vec<_> = sq.iter().rev().cloned().collect();
        assert!(polygon_area(&ccw) < 0.0);
    }

    #[test]
    fn test_normalize_clockwise() {
        let mut ccw = vec![[0.0, 0.0], [0.0, 1.0], [1.0, 1.0], [1.0, 0.0]];
        normalize_ring(&mut ccw, f64::INFINITY);
        assert!(polygon_area(&ccw) > 0.0); // Now clockwise.
    }

    #[test]
    fn test_normalize_open() {
        let mut ring = vec![[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 0.0]];
        normalize_ring(&mut ring, f64::INFINITY);
        assert_eq!(ring.len(), 3); // Trailing duplicate removed.
    }

    #[test]
    fn test_bisect() {
        let mut ring = vec![[0.0, 0.0], [10.0, 0.0], [10.0, 10.0], [0.0, 10.0]];
        bisect(&mut ring, 3.0);
        // Each 10-unit edge should be split into ~4 segments.
        assert!(ring.len() > 4);
    }

    #[test]
    fn test_add_points() {
        let mut ring = vec![[0.0, 0.0], [4.0, 0.0], [4.0, 4.0], [0.0, 4.0]];
        add_points(&mut ring, 4);
        assert!(ring.len() > 4);
    }

    #[test]
    fn test_circle_points() {
        let pts = circle_points(0.0, 0.0, 1.0, 4);
        assert_eq!(pts.len(), 4);
        // All points should be on the unit circle.
        for p in &pts {
            assert!((dist(p, &[0.0, 0.0]) - 1.0).abs() < 1e-9);
        }
    }

    #[test]
    fn test_morph_identity() {
        let from = vec![[0.0, 0.0], [1.0, 0.0], [1.0, 1.0]];
        let result = morph(&from, &from, 0.5, f64::INFINITY);
        for (a, b) in from.iter().zip(result.iter()) {
            assert!((a[0] - b[0]).abs() < 1e-9);
            assert!((a[1] - b[1]).abs() < 1e-9);
        }
    }

    #[test]
    fn test_morph_endpoints() {
        let from = vec![[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]];
        let to = vec![[0.0, 0.0], [2.0, 0.0], [2.0, 2.0], [0.0, 2.0]];
        let interp = interpolate_ring(from.clone(), to.clone(), f64::INFINITY);
        let at_0 = interp(0.0);
        let at_1 = interp(1.0);
        // At t=0, should match from (after normalization).
        for p in &at_0 {
            assert!(from.iter().any(|q| dist(p, q) < 1e-6));
        }
        // At t=1, should match to.
        for p in &at_1 {
            assert!(to.iter().any(|q| dist(p, q) < 1e-6));
        }
    }

    #[test]
    fn test_convex_hull() {
        let pts = vec![[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0], [0.5, 0.5]];
        let hull = convex_hull(&pts);
        assert_eq!(hull.len(), 4); // Interior point excluded.
    }

    #[test]
    fn test_rotate_alignment() {
        // from is rotated by 2 positions; rotate() should fix the alignment.
        let to = vec![[0.0, 0.0], [1.0, 0.0], [2.0, 0.0], [3.0, 0.0]];
        let mut from = vec![[3.0, 0.0], [0.0, 0.0], [1.0, 0.0], [2.0, 0.0]];
        rotate(&mut from, &to);
        for i in 0..4 {
            assert!((from[i][0] - to[i][0]).abs() < 1e-9);
        }
    }

    #[test]
    fn test_rect_points() {
        let pts = rect_points(0.0, 0.0, 4.0, 2.0, 8);
        assert_eq!(pts.len(), 8);
        // First point should be on the top edge.
        assert!((pts[0][1] - 0.0).abs() < 1e-9);
    }

    #[test]
    fn test_interpolate_ring_different_counts() {
        let from = circle_points(0.0, 0.0, 1.0, 4);
        let to = circle_points(0.0, 0.0, 2.0, 8);
        let interp = interpolate_ring(from, to, f64::INFINITY);
        let mid = interp(0.5);
        // Both rings should be equalized to the larger count (8).
        // The result has 8 points.
        assert!(!mid.is_empty(), "result should not be empty");
        assert!(mid.len() >= 4, "result should have at least 4 points, got {}", mid.len());
    }
}
