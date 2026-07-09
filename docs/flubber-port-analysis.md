# Flubber → Rust Port: Feasibility Analysis

## Overview

This document analyzes the feasibility of porting the [Flubber](https://github.com/veltman/flubber) JavaScript shape-morphing library to Rust, for integration into candy's Morph module.

## Flubber Algorithm Summary

Flubber morphs between two polygons (rings of 2D points) in three steps:

1. **Normalize**: open the ring (drop trailing duplicate), enforce clockwise winding, bisect long segments.
2. **Equalize & align**: insert arc-length-even points on the shorter ring, then find the best cyclic rotation (O(n²) brute force) that minimizes sum of squared distances.
3. **Interpolate**: lerp index-by-index: `p(t) = a + t*(b-a)`.

For 1→N morphing: triangulate with earcut, greedily merge triangles into N pieces by area, morph each piece independently.

## Port Status

### Completed (this commit)

| Flubber module | Rust port | Status |
|---|---|---|
| `math.js` | Geometry primitives (dist, lerp, area, centroid) | ✅ Complete |
| `normalize.js` | `normalize_ring` (open, CW, bisect) | ✅ Complete |
| `add.js` | `add_points`, `bisect` | ✅ Complete |
| `rotate.js` | `rotate` (cyclic alignment) | ✅ Complete |
| `interpolate.js` | `interpolate_ring` (returns `impl Fn(f64)->Ring`) | ✅ Complete |
| `shape.js` | `circle_points`, `rect_points` | ✅ Complete |
| `triangulate.js` | `split_shape` (earcutr + union-find) | ✅ Simplified (convex hull instead of TopoJSON boundary extraction) |
| `topology.js` | Union-find merge | ✅ Simplified |
| `order.js` | (not ported — needed for N→N morphing) | ⏳ Deferred |
| `multiple.js` | (not ported — needed for N→N morphing) | ⏳ Deferred |
| `svg.js` | (not ported — candy uses explicit points, not SVG strings) | ⏳ N/A |

### Text morphing (字符级拆分)

| Feature | Status |
|---|---|
| `glyph_outline(ch, font_size)` | ✅ Returns bounding-box polygon (4 points) |
| `text_outlines(text, font_size)` | ✅ Extracts outline per character |
| True Bézier outline flattening | ⏳ Future work (currently bounding-box approximation) |
| Character-level morphing animation | ⏳ Future work (pair glyphs by index, morph each pair) |

## Feasibility Assessment

### Straightforward (done)

- **Pure numeric code** (80% of Flubber): `math.js`, `add.js`, `rotate.js`, `normalize.js` — mechanical translation, ~250 lines of trivial Rust.
- **Core interpolator**: `interpolate_ring` returns a closure, natural in Rust.
- **Shape generators**: `circle_points`, `rect_points` — pure math.
- **Triangulation**: `earcutr` crate is a direct port of Flubber's `earcut` dependency.

### Needed Rust-specific adaptations (done)

- **Closures**: JS `t => …` → Rust `impl Fn(f64) -> Ring`. Works naturally.
- **In-place mutation**: JS `Array.splice` → Rust `Vec::insert`/`Vec::splice`. Works.
- **Dynamic typing**: JS `string | Array` → Rust `Ring = Vec<[f64; 2]>` (candy uses explicit points, not SVG strings).

### Hard parts (simplified or deferred)

1. **SVG path parsing** (`svg.js`): Not needed for candy — candy uses explicit point arrays, not SVG path strings. If SVG extraction from typst-svg output is needed later, use `svgtypes` + `kurbo` crates.

2. **TopoJSON collapse** (`topology.js`): Replaced with union-find on triangle adjacency. The current implementation uses convex hull for boundary extraction (simpler but less accurate than Flubber's arc-merging). For production use, port the proper boundary extraction (~200 LOC).

3. **Glyph outline flattening**: Currently returns bounding box. For true outline morphing, flatten the Bézier curves from `ab_glyph::OutlineCurve` into a polygon. This requires:
   - Iterate `Outline.curves` (Vec<OutlineCurve>)
   - For each `OutlineCurve::Line(p1, p2)`: add p2
   - For each `OutlineCurve::Quad(p1, c, p2)`: flatten quadratic Bézier (subdivide until segments < threshold)
   - For each `OutlineCurve::Cubic(p1, c1, c2, p2)`: flatten cubic Bézier
   - This is ~50 LOC with `kurbo::Bez` or manual de Casteljau.

### Recommended external crates

| Crate | Replaces | Used? |
|---|---|---|
| `earcutr` | `earcut` | ✅ Yes |
| `ab_glyph` | Font loading for glyph outlines | ✅ Yes |
| `svgtypes` + `kurbo` | `svgpath` + `svg-path-properties` | ⏳ Not needed yet |
| `geo` | `d3-polygon` | ⏳ Not needed (hand-rolled) |

## Integration with candy

### Current approach: explicit points API

candy's mobjects are opaque Typst content strings. The morph module operates on explicit point arrays (`Vec<[f64; 2]>`), which users provide via helper functions:

```typst
#let from = circle-points(0.0, 0.0, 1.0, 32)  // 32-point circle
#let to = rect-points(0.0, 0.0, 2.0, 1.0, 32)  // 32-point rectangle
#let morphed = morph(from, to, 0.5)  // interpolated at t=0.5
```

### Future: SVG extraction from typst-svg

Render both mobjects via `typst-svg`, extract the dominant `<path>` element, parse its `d` attribute into a polygon ring, morph, and re-emit as a Typst `polygon(...)`. This would enable the magical `morph(rect(...), circle(...))` syntax without explicit point arrays.

### Future: character-level text morphing (无缝移动)

Pair glyphs of source text with glyphs of target text (by index or by centroid matching), morph each pair independently using the same `interpolate_ring` core. Each character's outline is extracted via `ab_glyph` and flattened from Bézier curves into polygons.
