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
            .split(|c| c == ' ' || c == ',')
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
    out: &mut Vec<((f64, f64, f64, f64), String)>,
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

/// Union only the geometry that carries the given `fill` colour
/// (case-insensitive, `#rrggbb`). Used by the native layout pass: each mobject
/// is wrapped in a uniquely-coloured `box`, so locating that colour's footprint
/// recovers the object's natural top-left as laid out by Typst itself — no
/// hand-computed coordinates.
pub(crate) fn bbox_of_svg_with_fill(svg: &str, fill: &str) -> Option<(f64, f64, f64, f64)> {
    let target = fill.to_ascii_lowercase();
    let mut fill_stack: Vec<String> = Vec::new();
    let mut cur_fill = String::new();
    let mut stack: Vec<[f64; 6]> = Vec::new();
    let mut cur: [f64; 6] = [1.0, 0.0, 0.0, 1.0, 0.0, 0.0];
    let mut min_x = f64::INFINITY;
    let mut min_y = f64::INFINITY;
    let mut max_x = f64::NEG_INFINITY;
    let mut max_y = f64::NEG_INFINITY;

    let mut idx = 0;
    while idx < svg.len() {
        let Some(lt) = svg[idx..].find('<') else {
            break;
        };
        let lt = idx + lt;
        if svg[lt..].starts_with("</g>") {
            if let Some(m) = stack.pop() {
                cur = m;
            }
            if let Some(f) = fill_stack.pop() {
                cur_fill = f;
            }
            idx = lt + 4;
            continue;
        }
        let Some(gt) = svg[lt..].find('>') else {
            break;
        };
        let gt = lt + gt;
        let tag = &svg[lt + 1..gt];
        let is_g_open = tag == "g" || tag.starts_with("g ") || tag.starts_with("g>");
        let mut el_matrix = cur;
        if let Some(t) = svg_attr(tag, "transform") {
            el_matrix = compose_matrix(cur, &parse_transform_attr(&t));
        }
        // Effective fill for this element: an explicit `fill` attr wins,
        // otherwise it inherits from the nearest ancestor group.
        let mut el_fill = cur_fill.clone();
        if let Some(f) = svg_attr(tag, "fill") {
            el_fill = f.to_ascii_lowercase();
        }
        if is_g_open {
            stack.push(cur);
            fill_stack.push(cur_fill.clone());
            cur = el_matrix;
            cur_fill = el_fill;
            idx = gt + 1;
            continue;
        }
        if el_fill == target {
            let pts: Vec<(f64, f64)> = match tag.split_whitespace().next() {
                Some("rect") => {
                    let (x, y) = (svg_num(tag, "x"), svg_num(tag, "y"));
                    let (w, h) = (svg_num(tag, "width"), svg_num(tag, "height"));
                    vec![(x, y), (x + w, y), (x + w, y + h), (x, y + h)]
                }
                Some("circle") => {
                    let (cx, cy, r) = (svg_num(tag, "cx"), svg_num(tag, "cy"), svg_num(tag, "r"));
                    vec![(cx - r, cy - r), (cx + r, cy + r)]
                }
                Some("ellipse") => {
                    let (cx, cy) = (svg_num(tag, "cx"), svg_num(tag, "cy"));
                    let (rx, ry) = (svg_num(tag, "rx"), svg_num(tag, "ry"));
                    vec![(cx - rx, cy - ry), (cx + rx, cy + ry)]
                }
                Some("polygon") | Some("polyline") => svg_points(svg_attr(tag, "points")),
                Some("path") => match svg_attr(tag, "d") {
                    Some(d) => collect_path_points(&d),
                    None => vec![],
                },
                _ => vec![],
            };
            for (x, y) in pts {
                let (px, py) = apply_matrix(&el_matrix, x, y);
                if px < min_x {
                    min_x = px;
                }
                if py < min_y {
                    min_y = py;
                }
                if px > max_x {
                    max_x = px;
                }
                if py > max_y {
                    max_y = py;
                }
            }
        }
        idx = gt + 1;
    }
    if min_x.is_finite() {
        Some((min_x, min_y, max_x, max_y))
    } else {
        None
    }
}

/// Extract `name="value"` (single or double quoted) from a tag string.
pub(crate) fn svg_attr(tag: &str, name: &str) -> Option<String> {
    let pat = format!("{name}=");
    let i = tag.find(&pat)? + pat.len();
    let b = tag.as_bytes().get(i)?;
    if *b != b'"' && *b != b'\'' {
        return None;
    }
    let q = *b as char;
    let start = i + 1;
    let end = start + tag[start..].find(q)?;
    Some(tag[start..end].to_string())
}

pub(crate) fn svg_num(tag: &str, name: &str) -> f64 {
    svg_attr(tag, name)
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(0.0)
}

/// Parse a `points="x1,y1 x2,y2 ..."` attribute into coordinate pairs.
pub(crate) fn svg_points(s: Option<String>) -> Vec<(f64, f64)> {
    let Some(s) = s else {
        return vec![];
    };
    s.split(|c: char| c == ',' || c.is_whitespace())
        .filter(|t| !t.is_empty())
        .filter_map(|t| t.trim().parse::<f64>().ok())
        .collect::<Vec<_>>()
        .chunks(2)
        .filter_map(|c| {
            if c.len() == 2 {
                Some((c[0], c[1]))
            } else {
                None
            }
        })
        .collect()
}

/// Loose extent of an SVG path: every coordinate pair in `d` (control points
/// included). Good enough for layout spacing.
/// A token in an SVG path `d` string: a command letter or a numeric argument.
pub(crate) enum PathTok {
    Cmd(char),
    Num(f64),
}

/// Pull the next numeric argument, skipping any interleaved command letters
/// (which belong to a later group). Returns `None` at end-of-input or when the
/// next token is a command (so the caller can stop consuming this group).
pub(crate) fn next_path_num(toks: &[PathTok], i: &mut usize) -> Option<f64> {
    while *i < toks.len() {
        match toks[*i] {
            PathTok::Num(v) => {
                *i += 1;
                return Some(v);
            }
            PathTok::Cmd(_) => return None,
        }
    }
    None
}

/// Parse an SVG path `d` attribute into the set of points that bound it.
///
/// Unlike a naive "pair up all numbers" scheme, this honours command letters,
/// relative (lowercase) vs absolute (uppercase) coordinates, the single-axis
/// `h`/`v` commands, and implicit command repetition (e.g. `M 0 0 1 1` draws a
/// move followed by a line). Bézier control points are included so the returned
/// hull bounds the whole curve (a Bézier lies inside its control-point convex
/// hull). Previously this function just zipped every number into `(x, y)` pairs,
/// which silently transposed `v`/`h` rects and broke any non-square path.
pub(crate) fn collect_path_points(d: &str) -> Vec<(f64, f64)> {
    // Tokenize: command letters vs numbers (scientific notation is allowed).
    let mut toks: Vec<PathTok> = Vec::new();
    let mut num = String::new();
    let flush = |num: &mut String, toks: &mut Vec<PathTok>| {
        if !num.is_empty() {
            if let Ok(v) = num.parse::<f64>() {
                toks.push(PathTok::Num(v));
            }
            num.clear();
        }
    };
    for c in d.chars() {
        if matches!(
            c,
            'M' | 'm'
                | 'L'
                | 'l'
                | 'H'
                | 'h'
                | 'V'
                | 'v'
                | 'C'
                | 'c'
                | 'S'
                | 's'
                | 'Q'
                | 'q'
                | 'T'
                | 't'
                | 'A'
                | 'a'
                | 'Z'
                | 'z'
        ) {
            flush(&mut num, &mut toks);
            toks.push(PathTok::Cmd(c));
        } else if c.is_ascii_digit() || c == '.' || c == '-' || c == '+' || c == 'e' || c == 'E' {
            // `e`/`E` only appear inside scientific-notation numbers, so they are
            // part of a numeric token rather than a (non-existent) command.
            num.push(c);
        } else {
            flush(&mut num, &mut toks);
        }
    }
    flush(&mut num, &mut toks);

    let mut pts: Vec<(f64, f64)> = Vec::new();
    let mut cx = 0.0;
    let mut cy = 0.0;
    let mut sx = 0.0; // current subpath start (for `Z`)
    let mut sy = 0.0;
    let mut cmd: Option<char> = None;
    let mut first = true; // first argument group of the current command run
    let mut i = 0;
    while i < toks.len() {
        if let PathTok::Cmd(c) = toks[i] {
            cmd = Some(c);
            first = true;
            i += 1;
        }
        let base = match cmd {
            Some(c) => c,
            None => {
                i += 1;
                continue;
            }
        };
        let rel = base.is_lowercase();
        // A `M`/`m` run emits move then implicit lineto for the rest of the group.
        let eff = if first {
            base
        } else {
            match base {
                'M' => 'L',
                'm' => 'l',
                o => o,
            }
        };
        match eff {
            'Z' | 'z' => {
                cx = sx;
                cy = sy;
                pts.push((cx, cy));
                first = false;
            }
            'H' | 'h' => {
                let x = match next_path_num(&toks, &mut i) {
                    Some(v) => v,
                    None => break,
                };
                cx = if rel { cx + x } else { x };
                pts.push((cx, cy));
                first = false;
            }
            'V' | 'v' => {
                let y = match next_path_num(&toks, &mut i) {
                    Some(v) => v,
                    None => break,
                };
                cy = if rel { cy + y } else { y };
                pts.push((cx, cy));
                first = false;
            }
            'L' | 'l' | 'M' | 'm' | 'T' | 't' => {
                let x = match next_path_num(&toks, &mut i) {
                    Some(v) => v,
                    None => break,
                };
                let y = match next_path_num(&toks, &mut i) {
                    Some(v) => v,
                    None => break,
                };
                let (nx, ny) = if rel { (cx + x, cy + y) } else { (x, y) };
                cx = nx;
                cy = ny;
                if eff == 'M' || eff == 'm' {
                    sx = cx;
                    sy = cy;
                }
                pts.push((cx, cy));
                first = false;
            }
            'Q' | 'q' => {
                let x1 = next_path_num(&toks, &mut i);
                let y1 = next_path_num(&toks, &mut i);
                let x = match next_path_num(&toks, &mut i) {
                    Some(v) => v,
                    None => break,
                };
                let y = match next_path_num(&toks, &mut i) {
                    Some(v) => v,
                    None => break,
                };
                let (cx1, cy1) = if rel {
                    (cx + x1.unwrap_or(0.0), cy + y1.unwrap_or(0.0))
                } else {
                    (x1.unwrap_or(0.0), y1.unwrap_or(0.0))
                };
                let (nx, ny) = if rel { (cx + x, cy + y) } else { (x, y) };
                pts.push((cx1, cy1));
                pts.push((nx, ny));
                cx = nx;
                cy = ny;
                first = false;
            }
            'S' | 's' => {
                let x2 = next_path_num(&toks, &mut i);
                let y2 = next_path_num(&toks, &mut i);
                let x = match next_path_num(&toks, &mut i) {
                    Some(v) => v,
                    None => break,
                };
                let y = match next_path_num(&toks, &mut i) {
                    Some(v) => v,
                    None => break,
                };
                let (cx2, cy2) = if rel {
                    (cx + x2.unwrap_or(0.0), cy + y2.unwrap_or(0.0))
                } else {
                    (x2.unwrap_or(0.0), y2.unwrap_or(0.0))
                };
                let (nx, ny) = if rel { (cx + x, cy + y) } else { (x, y) };
                pts.push((cx2, cy2));
                pts.push((nx, ny));
                cx = nx;
                cy = ny;
                first = false;
            }
            'C' | 'c' => {
                let x1 = next_path_num(&toks, &mut i);
                let y1 = next_path_num(&toks, &mut i);
                let x2 = next_path_num(&toks, &mut i);
                let y2 = next_path_num(&toks, &mut i);
                let x = match next_path_num(&toks, &mut i) {
                    Some(v) => v,
                    None => break,
                };
                let y = match next_path_num(&toks, &mut i) {
                    Some(v) => v,
                    None => break,
                };
                let (cx1, cy1) = if rel {
                    (cx + x1.unwrap_or(0.0), cy + y1.unwrap_or(0.0))
                } else {
                    (x1.unwrap_or(0.0), y1.unwrap_or(0.0))
                };
                let (cx2, cy2) = if rel {
                    (cx + x2.unwrap_or(0.0), cy + y2.unwrap_or(0.0))
                } else {
                    (x2.unwrap_or(0.0), y2.unwrap_or(0.0))
                };
                let (nx, ny) = if rel { (cx + x, cy + y) } else { (x, y) };
                pts.push((cx1, cy1));
                pts.push((cx2, cy2));
                pts.push((nx, ny));
                cx = nx;
                cy = ny;
                first = false;
            }
            'A' | 'a' => {
                // rx ry x-axis-rotation large-arc-flag sweep-flag x y
                let _rx = next_path_num(&toks, &mut i);
                let _ry = next_path_num(&toks, &mut i);
                let _rot = next_path_num(&toks, &mut i);
                let _la = next_path_num(&toks, &mut i);
                let _sw = next_path_num(&toks, &mut i);
                let x = match next_path_num(&toks, &mut i) {
                    Some(v) => v,
                    None => break,
                };
                let y = match next_path_num(&toks, &mut i) {
                    Some(v) => v,
                    None => break,
                };
                let (nx, ny) = if rel { (cx + x, cy + y) } else { (x, y) };
                pts.push((nx, ny));
                cx = nx;
                cy = ny;
                first = false;
            }
            _ => {
                // Unknown command: consume one number and move on.
                next_path_num(&toks, &mut i);
                first = false;
            }
        }
    }
    pts
}

/// Apply a 2-D affine `[a, b, c, d, e, f]` to a point.
pub(crate) fn apply_matrix(m: &[f64; 6], x: f64, y: f64) -> (f64, f64) {
    (m[0] * x + m[2] * y + m[4], m[1] * x + m[3] * y + m[5])
}

/// Compose two affines so that `result` applies `b` then `a` (SVG `a b` order).
pub(crate) fn compose_matrix(a: [f64; 6], b: &[f64; 6]) -> [f64; 6] {
    [
        a[0] * b[0] + a[2] * b[1],
        a[1] * b[0] + a[3] * b[1],
        a[0] * b[2] + a[2] * b[3],
        a[1] * b[2] + a[3] * b[3],
        a[0] * b[4] + a[2] * b[5] + a[4],
        a[1] * b[4] + a[3] * b[5] + a[5],
    ]
}

/// Parse a `transform` attribute (`translate` / `scale` / `rotate` / `matrix`).
pub(crate) fn parse_transform_attr(s: &str) -> [f64; 6] {
    let mut m = [1.0, 0.0, 0.0, 1.0, 0.0, 0.0];
    let mut rest = s;
    while let Some(open) = rest.find('(') {
        let Some(close) = rest[open..].find(')') else {
            break;
        };
        let close = open + close;
        let name_start = rest[..open]
            .rfind(|c: char| !(c.is_alphabetic() || c == '-'))
            .map(|i| i + 1)
            .unwrap_or(0);
        let name = &rest[name_start..open];
        let args: Vec<f64> = rest[open + 1..close]
            .split(|c: char| {
                !(c.is_ascii_digit() || c == '.' || c == '-' || c == '+' || c == 'e' || c == 'E')
            })
            .filter(|t| !t.is_empty())
            .filter_map(|t| t.parse::<f64>().ok())
            .collect();
        let tm = match name {
            "translate" if args.len() >= 2 => [1.0, 0.0, 0.0, 1.0, args[0], args[1]],
            "translate" => [
                1.0,
                0.0,
                0.0,
                1.0,
                args.first().copied().unwrap_or(0.0),
                0.0,
            ],
            "scale" if args.len() >= 2 => [args[0], 0.0, 0.0, args[1], 0.0, 0.0],
            "scale" => {
                let s = args.first().copied().unwrap_or(1.0);
                [s, 0.0, 0.0, s, 0.0, 0.0]
            }
            "rotate" if args.len() >= 1 => {
                let r = args[0].to_radians();
                let (s, c) = (r.sin(), r.cos());
                [c, s, -s, c, 0.0, 0.0]
            }
            "matrix" if args.len() >= 6 => [args[0], args[1], args[2], args[3], args[4], args[5]],
            _ => [1.0, 0.0, 0.0, 1.0, 0.0, 0.0],
        };
        m = compose_matrix(m, &tm);
        rest = &rest[close + 1..];
    }
    m
}
