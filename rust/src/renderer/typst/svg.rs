//! SVG geometry parsing helpers: bounding boxes, path tokenization, and
//! attribute extraction used by the native-layout pass and the formula-glyph
//! fragment extractor.

use std::collections::HashMap;

use roxmltree::Node;

/// Accumulator for path / element bounding boxes (user-space points).
pub(crate) struct SvgBBox {
    minx: f64,
    miny: f64,
    maxx: f64,
    maxy: f64,
    has: bool,
}

impl SvgBBox {
    fn new() -> Self {
        SvgBBox {
            minx: 0.0,
            miny: 0.0,
            maxx: 0.0,
            maxy: 0.0,
            has: false,
        }
    }
    fn add(&mut self, x: f64, y: f64) {
        if !self.has {
            self.minx = x;
            self.miny = y;
            self.maxx = x;
            self.maxy = y;
            self.has = true;
        } else {
            self.minx = self.minx.min(x);
            self.miny = self.miny.min(y);
            self.maxx = self.maxx.max(x);
            self.maxy = self.maxy.max(y);
        }
    }
}

/// A 2-D affine transform in SVG `matrix(a b c d e f)` form:
/// `x' = a*x + c*y + e`, `y' = b*x + d*y + f`.
type Xf = (f64, f64, f64, f64, f64, f64);

/// Axis-aligned bounding box `(x0, y0, x1, y1)` in SVG user units.
pub(crate) type BBox = (f64, f64, f64, f64);
/// One extracted formula leaf: its bounding box plus a signature (the glyph's
/// path data), used to match identical glyphs across formulas in `#transform`.
pub(crate) type FormulaLeaf = (BBox, String);

/// Parse an SVG `transform` attribute (`matrix(…)` / `translate(…)`).
pub(crate) fn parse_transform(s: &str) -> Xf {
    let s = s.trim();
    if let Some(rest) = s.strip_prefix("matrix(") {
        let inner = &rest[..rest.find(')').unwrap_or(rest.len())];
        let nums: Vec<f64> = inner
            .split_whitespace()
            .filter_map(|x| x.parse::<f64>().ok())
            .collect();
        if nums.len() >= 6 {
            return (nums[0], nums[1], nums[2], nums[3], nums[4], nums[5]);
        }
    } else if let Some(rest) = s.strip_prefix("translate(") {
        let inner = &rest[..rest.find(')').unwrap_or(rest.len())];
        let nums: Vec<f64> = inner
            .split([' ', ','])
            .filter_map(|x| x.parse::<f64>().ok())
            .collect();
        let tx = nums.first().copied().unwrap_or(0.0);
        let ty = nums.get(1).copied().unwrap_or(0.0);
        return (1.0, 0.0, 0.0, 1.0, tx, ty);
    }
    (1.0, 0.0, 0.0, 1.0, 0.0, 0.0)
}

/// Compose two transforms: `combine(parent, child)` applies `child` first,
/// then `parent` (i.e. parent ∘ child).
pub(crate) fn combine(m1: Xf, m2: Xf) -> Xf {
    let (a1, b1, c1, d1, e1, f1) = m1;
    let (a2, b2, c2, d2, e2, f2) = m2;
    (
        a1 * a2 + c1 * b2,
        b1 * a2 + d1 * b2,
        a1 * c2 + c1 * d2,
        b1 * c2 + d1 * d2,
        a1 * e2 + c1 * f2 + e1,
        b1 * e2 + d1 * f2 + f1,
    )
}

/// Transform an axis-aligned rect's two corners and return their bbox.
pub(crate) fn xf_rect(t: Xf, r: (f64, f64, f64, f64)) -> (f64, f64, f64, f64) {
    let (a, b, c, d, e, f) = t;
    let p1 = (a * r.0 + c * r.1 + e, b * r.0 + d * r.1 + f);
    let p2 = (a * r.2 + c * r.3 + e, b * r.2 + d * r.3 + f);
    (
        p1.0.min(p2.0),
        p1.1.min(p2.1),
        p1.0.max(p2.0),
        p1.1.max(p2.1),
    )
}

/// Recursively collect leaf drawables (glyph `<use>` / decoration `<path>`) from
/// a formula's SVG, flattening `<g>` groups so each glyph / bar becomes its own
/// fragment. `acc` is the transform accumulated from ancestor groups.
pub(crate) fn collect_formula_leaves(
    node: &Node,
    acc: Xf,
    symbols: &HashMap<String, String>,
    out: &mut Vec<FormulaLeaf>,
) {
    let tag = node.tag_name().name();
    match tag {
        "use" => {
            if let Some(href) = node
                .attribute("xlink:href")
                .or_else(|| node.attribute("href"))
            {
                let id = href.trim_start_matches('#');
                if let Some(d) = symbols.get(id) {
                    let t = combine(
                        acc,
                        parse_transform(node.attribute("transform").unwrap_or("")),
                    );
                    let lb = path_bbox(d).unwrap_or((0.0, 0.0, 0.0, 0.0));
                    out.push((xf_rect(t, lb), d.to_string()));
                }
            }
        }
        "path" => {
            if let Some(d) = node.attribute("d") {
                let t = combine(
                    acc,
                    parse_transform(node.attribute("transform").unwrap_or("")),
                );
                if let Some(lb) = path_bbox(d) {
                    out.push((xf_rect(t, lb), d.to_string()));
                }
            }
        }
        "g" => {
            let t = combine(
                acc,
                parse_transform(node.attribute("transform").unwrap_or("")),
            );
            for child in node.children() {
                if child.is_element() {
                    collect_formula_leaves(&child, t, symbols, out);
                }
            }
        }
        _ => {}
    }
}

/// Bounding box of an SVG path's geometry (pt). Control points of curves are
/// included so the box is never smaller than the true ink.
pub(crate) fn path_bbox(d: &str) -> Option<(f64, f64, f64, f64)> {
    let b = d.as_bytes();
    let n = b.len();
    let mut i = 0usize;
    let mut cmd = ' ';
    let mut cx = 0.0f64;
    let mut cy = 0.0f64;
    let mut sx = 0.0f64;
    let mut sy = 0.0f64;
    let mut bb = SvgBBox::new();
    let mut first = true;
    while i < n {
        while i < n
            && (b[i] == b' ' || b[i] == b',' || b[i] == b'\t' || b[i] == b'\n' || b[i] == b'\r')
        {
            i += 1;
        }
        if i >= n {
            break;
        }
        let c = b[i] as char;
        if c.is_ascii_alphabetic() {
            cmd = c;
            i += 1;
        }
        match cmd {
            'M' => {
                let (x, y) = (read_num(b, &mut i), read_num(b, &mut i));
                cx = x;
                cy = y;
                bb.add(x, y);
                if first {
                    sx = x;
                    sy = y;
                    first = false;
                }
                cmd = 'L';
            }
            'm' => {
                let (x, y) = (read_num(b, &mut i), read_num(b, &mut i));
                cx += x;
                cy += y;
                bb.add(cx, cy);
                if first {
                    sx = cx;
                    sy = cy;
                    first = false;
                }
                cmd = 'l';
            }
            'L' => {
                let (x, y) = (read_num(b, &mut i), read_num(b, &mut i));
                cx = x;
                cy = y;
                bb.add(x, y);
            }
            'l' => {
                let (x, y) = (read_num(b, &mut i), read_num(b, &mut i));
                cx += x;
                cy += y;
                bb.add(cx, cy);
            }
            'H' => {
                let x = read_num(b, &mut i);
                cx = x;
                bb.add(cx, cy);
            }
            'h' => {
                cx += read_num(b, &mut i);
                bb.add(cx, cy);
            }
            'V' => {
                let y = read_num(b, &mut i);
                cy = y;
                bb.add(cx, cy);
            }
            'v' => {
                cy += read_num(b, &mut i);
                bb.add(cx, cy);
            }
            'C' => {
                let (c1x, c1y) = (read_num(b, &mut i), read_num(b, &mut i));
                let (c2x, c2y) = (read_num(b, &mut i), read_num(b, &mut i));
                let (x, y) = (read_num(b, &mut i), read_num(b, &mut i));
                bb.add(c1x, c1y);
                bb.add(c2x, c2y);
                bb.add(x, y);
                cx = x;
                cy = y;
            }
            'c' => {
                let (c1x, c1y) = (read_num(b, &mut i), read_num(b, &mut i));
                let (c2x, c2y) = (read_num(b, &mut i), read_num(b, &mut i));
                let (x, y) = (read_num(b, &mut i), read_num(b, &mut i));
                bb.add(cx + c1x, cy + c1y);
                bb.add(cx + c2x, cy + c2y);
                bb.add(cx + x, cy + y);
                cx += x;
                cy += y;
            }
            'S' => {
                let (c2x, c2y) = (read_num(b, &mut i), read_num(b, &mut i));
                let (x, y) = (read_num(b, &mut i), read_num(b, &mut i));
                bb.add(c2x, c2y);
                bb.add(x, y);
                cx = x;
                cy = y;
            }
            's' => {
                let (c2x, c2y) = (read_num(b, &mut i), read_num(b, &mut i));
                let (x, y) = (read_num(b, &mut i), read_num(b, &mut i));
                bb.add(cx + c2x, cy + c2y);
                bb.add(cx + x, cy + y);
                cx += x;
                cy += y;
            }
            'Q' => {
                let (c1x, c1y) = (read_num(b, &mut i), read_num(b, &mut i));
                let (x, y) = (read_num(b, &mut i), read_num(b, &mut i));
                bb.add(c1x, c1y);
                bb.add(x, y);
                cx = x;
                cy = y;
            }
            'q' => {
                let (c1x, c1y) = (read_num(b, &mut i), read_num(b, &mut i));
                let (x, y) = (read_num(b, &mut i), read_num(b, &mut i));
                bb.add(cx + c1x, cy + c1y);
                bb.add(cx + x, cy + y);
                cx += x;
                cy += y;
            }
            'T' => {
                let (x, y) = (read_num(b, &mut i), read_num(b, &mut i));
                cx = x;
                cy = y;
                bb.add(x, y);
            }
            't' => {
                let (x, y) = (read_num(b, &mut i), read_num(b, &mut i));
                cx += x;
                cy += y;
                bb.add(cx, cy);
            }
            'A' | 'a' => {
                let _rx = read_num(b, &mut i);
                let _ry = read_num(b, &mut i);
                let _rot = read_num(b, &mut i);
                let _large = read_num(b, &mut i);
                let _sweep = read_num(b, &mut i);
                let (x, y) = (read_num(b, &mut i), read_num(b, &mut i));
                if cmd == 'a' {
                    cx += x;
                    cy += y;
                } else {
                    cx = x;
                    cy = y;
                }
                bb.add(cx, cy);
            }
            'Z' | 'z' => {
                cx = sx;
                cy = sy;
            }
            _ => {
                i += 1;
            }
        }
    }
    if bb.has {
        Some((bb.minx, bb.miny, bb.maxx, bb.maxy))
    } else {
        None
    }
}

/// Parse a single SVG path number (advancing `i` past it).
pub(crate) fn read_num(b: &[u8], i: &mut usize) -> f64 {
    let n = b.len();
    while *i < n
        && (b[*i] == b' ' || b[*i] == b',' || b[*i] == b'\t' || b[*i] == b'\n' || b[*i] == b'\r')
    {
        *i += 1;
    }
    let start = *i;
    if *i < n && (b[*i] == b'-' || b[*i] == b'+') {
        *i += 1;
    }
    while *i < n
        && (b[*i].is_ascii_digit()
            || b[*i] == b'.'
            || b[*i] == b'e'
            || b[*i] == b'E'
            || ((b[*i] == b'-' || b[*i] == b'+') && *i > start))
    {
        *i += 1;
    }
    if *i == start {
        return 0.0;
    }
    std::str::from_utf8(&b[start..*i])
        .ok()
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(0.0)
}
