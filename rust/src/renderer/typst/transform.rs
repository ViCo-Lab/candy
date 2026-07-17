//! Per-glyph `#transform` plan types, the Typst source builders for placing a
//! single mobject body (and for morph outlines), and the whole per-glyph
//! transform engine: building the fragment layout (`build_transform_fragments`),
//! and emitting the interpolated SVG overlay (`transform_overlay_svg`).
//!
//! The overlay is kept as SVG (`<path>` / `<use>` fragments) and rasterized
//! **once** at the final step by `crate::renderer::raster::cpu` — replacing the old
//! per-fragment pixel path, which rasterized the whole formula once *per
//! fragment* and composited each crop. Keeping the overlay as SVG means the
//! formula is embedded only once (in `<defs>`, reused via `<use>`), so it is
//! never copied many times and only the single final rasterization touches
//! pixels.

use std::collections::HashMap;

use typst_svg::SvgOptions;

use crate::core::ast::{FrameData, Label};
use crate::core::easing::Easing;
use crate::renderer::typst::{
    collect_formula_leaves, imports_preamble, localize_formula_ids, Renderer, PT_PER_CM,
};
use typst_library::foundations::Dict;

/// One animated glyph/decoration in a per-glyph `transform`. The fragment is
/// cropped from the *whole* old or new formula render (so it keeps Typst's
/// exact metrics — script sizes, true fraction bars, …) and moved from
/// `(from_x, from_y)` to `(to_x, to_y)` (body-relative cm) while its opacity
/// goes `from_op → to_op`.
pub(crate) struct GlyphAnim {
    /// Which formula to crop from: `0` = old, `1` = new.
    pub(crate) src: u8,
    /// Source clip box, in the formula's local coords (pt; the formula is
    /// rendered at page origin so local == page coords).
    pub(crate) bx0: f64,
    pub(crate) by0: f64,
    pub(crate) bx1: f64,
    pub(crate) by1: f64,
    /// Interpolated target top-left, body-relative (cm).
    pub(crate) from_x: f64,
    pub(crate) from_y: f64,
    pub(crate) to_x: f64,
    pub(crate) to_y: f64,
    pub(crate) from_op: f64,
    pub(crate) to_op: f64,
    /// Clip box in the *new* formula's local coords (pt). Used when this fragment
    /// is drawn from the new formula — matched glyphs handed off at the midpoint,
    /// and inserted units. For fragments that only ever draw from one formula
    /// (deleted old / inserted new) this equals the corresponding `bx*..` box.
    pub(crate) nbx0: f64,
    pub(crate) nby0: f64,
    pub(crate) nbx1: f64,
    pub(crate) nby1: f64,
}

/// A precomputed per-glyph `Transform` layout for one `#transform(target, to:
/// …)` call whose old/new bodies are inline content (formula / text). Built
/// once in `ensure_natural` by rendering the whole old and new formulas and
/// extracting each glyph/decoration as a positioned fragment (via Typst's own
/// SVG layout — no custom parser). During `[start_ms, end_ms)` the render
/// paths composite the interpolated fragments *over* `target` so the old
/// content disassembles and reassembles into the new content (Manim-style).
pub(crate) struct TransformFragmentPlan {
    pub(crate) target: Label,
    pub(crate) old: Label,
    pub(crate) start_ms: u32,
    pub(crate) end_ms: u32,
    pub(crate) easing: Easing,
    /// SVG inner markup of the old / new formulas (used by the SVG path).
    pub(crate) old_inner: String,
    pub(crate) new_inner: String,
    /// Per-glyph animation fragments.
    pub(crate) anims: Vec<GlyphAnim>,
}

/// Build the Typst source that places a single mobject body at `(x_cm, y_cm)`
/// from the top-left corner, scaled by `scale_pct`% and rotated by `rotation`
/// degrees (clockwise, around the object's top-left origin).
///
/// When `rotation == 0.0` the `rotate(..)` wrapper is omitted, keeping the
/// generated source minimal for the common case (and matching the v0.1 output
/// exactly, so existing SVG drafts are byte-identical when no rotation is
/// applied).
#[allow(clippy::too_many_arguments)]
pub(crate) fn place_source(
    page_w: f64,
    page_h: f64,
    x_cm: f64,
    y_cm: f64,
    scale_pct: f64,
    rotation: f64,
    body: &str,
    preamble: &str,
) -> String {
    // The body is a raw Typst *expression* (e.g. "rect(width: 2cm, fill: red)")
    // recovered from the source AST node. We wrap it in a code block
    // `#{{ (body) }}` so it is evaluated as Typst *code*, not markup:
    //   · a body containing a string with `#` (e.g. `rgb("#9fb3ff")`) is safe —
    //     in markup mode the `#` would re-enter code mode and corrupt the source;
    //   · the surrounding parentheses let a multi-line body (e.g. `a\n + b`)
    //     stay a single expression — Typst treats newlines as separators inside
    //     a code block, but not inside parentheses.
    let pre = if preamble.is_empty() {
        String::new()
    } else {
        format!("{preamble}\n")
    };
    if rotation.abs() < 1e-9 {
        let out = format!(
            "{pre}#set page(width: {page_w}pt, height: {page_h}pt, margin: 0pt, fill: none)\n\
             #place(top + left, dx: {x_cm}cm, dy: {y_cm}cm)[#scale(origin: top + left, {scale_pct}%)[#{{ ({body}) }}]]\n"
        );
        out
    } else {
        format!(
            "{pre}#set page(width: {page_w}pt, height: {page_h}pt, margin: 0pt, fill: none)\n\
             #place(top + left, dx: {x_cm}cm, dy: {y_cm}cm)[#scale(origin: top + left, {scale_pct}%)[#rotate(origin: top + left, {rotation}deg)[#{{ ({body}) }}]]]"
        )
    }
}

/// One extracted graphical unit of a formula (a glyph outline, a fraction bar,
/// a root, …) — the "atom" the transform splits content into. Carries the
/// unit's shape signature (its path data, so identical glyphs match across the
/// old and new formula) and its center (pt), used for proximity matching.
struct ShapeUnit {
    sig: String,
    cx: f64,
    cy: f64,
}

impl ShapeUnit {
    /// Build a unit from an `extract_formula` fragment `((x0,y0,x1,y1), sig)`.
    fn from_frag(frag: &((f64, f64, f64, f64), String)) -> Self {
        let (x0, y0, x1, y1) = frag.0;
        ShapeUnit {
            sig: frag.1.clone(),
            cx: (x0 + x1) * 0.5,
            cy: (y0 + y1) * 0.5,
        }
    }
}

/// Squared Euclidean distance between two centers (pt). Squared is enough for
/// nearest-neighbour comparisons and avoids a `sqrt` per candidate.
fn dist2(ax: f64, ay: f64, bx: f64, by: f64) -> f64 {
    let dx = ax - bx;
    let dy = ay - by;
    dx * dx + dy * dy
}

/// Match old ↔ new graphical units by shape identity + geometric proximity
/// (Manim `TransformMatchingShapes`). Units with the same signature are paired
/// greedily by ascending center distance, so each surviving glyph glides to its
/// nearest same-shaped counterpart (identical glyphs no longer cross). Returns
/// `(old_index, new_index)` pairs. The pairing is deterministic: candidate
/// pairs are sorted by `(distance, old_index, new_index)`.
fn match_shapes(old: &[ShapeUnit], new: &[ShapeUnit]) -> Vec<(usize, usize)> {
    // Build all same-signature candidate pairs with their center distance.
    let mut cands: Vec<(f64, usize, usize)> = Vec::new();
    for (oi, o) in old.iter().enumerate() {
        for (ni, n) in new.iter().enumerate() {
            if o.sig == n.sig {
                cands.push((dist2(o.cx, o.cy, n.cx, n.cy), oi, ni));
            }
        }
    }
    // Greedy assignment: shortest pairs first, one-to-one.
    cands.sort_by(|a, b| {
        a.0.partial_cmp(&b.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.1.cmp(&b.1))
            .then(a.2.cmp(&b.2))
    });
    let mut used_old = vec![false; old.len()];
    let mut used_new = vec![false; new.len()];
    let mut matched: Vec<(usize, usize)> = Vec::new();
    for (_, oi, ni) in cands {
        if used_old[oi] || used_new[ni] {
            continue;
        }
        used_old[oi] = true;
        used_new[ni] = true;
        matched.push((oi, ni));
    }
    // Emit in old-index order so downstream draw order is stable.
    matched.sort_by_key(|a| a.0);
    matched
}

/// Find, among the matched pairs, the one whose *old* unit is geometrically
/// nearest to `(cx, cy)`, and project it through `pick` (typically to the pair's
/// new position). Used to give a deleted old unit a sensible collapse target.
fn nearest_matched(
    cx: f64,
    cy: f64,
    matched: &[(usize, usize)],
    old: &[ShapeUnit],
    pick: impl Fn(&(usize, usize)) -> (f64, f64),
) -> Option<(f64, f64)> {
    matched
        .iter()
        .min_by(|a, b| {
            let da = dist2(cx, cy, old[a.0].cx, old[a.0].cy);
            let db = dist2(cx, cy, old[b.0].cx, old[b.0].cy);
            da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
        })
        .map(pick)
}

/// Mirror of [`nearest_matched`] keyed on the pair's *new* unit — used to give
/// an inserted new unit a sensible emergence origin (its nearest survivor's old
/// position).
fn nearest_matched_new(
    cx: f64,
    cy: f64,
    matched: &[(usize, usize)],
    new: &[ShapeUnit],
    pick: impl Fn(&(usize, usize)) -> (f64, f64),
) -> Option<(f64, f64)> {
    matched
        .iter()
        .min_by(|a, b| {
            let da = dist2(cx, cy, new[a.1].cx, new[a.1].cy);
            let db = dist2(cx, cy, new[b.1].cx, new[b.1].cy);
            da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
        })
        .map(pick)
}

/// Per-glyph `#transform` engine, implemented on [`Renderer`]. Split out of
/// `mod.rs` so the renderer's core (natural layout, per-frame compositing,
/// camera, scenes) stays free of the transform-specific plumbing.
impl Renderer {
    /// Sanitize a label into an SVG-id-safe prefix (`[A-Za-z0-9_]`), suffixed
    /// with the plan index so two transforms on the same target never collide.
    fn transform_id_prefix(&self, target: &Label, plan_idx: usize) -> String {
        let mut base = String::new();
        for c in target.0.chars() {
            if c.is_alphanumeric() {
                base.push(c);
            } else {
                base.push('_');
            }
        }
        format!("{base}_{plan_idx}")
    }

    /// Build the per-glyph transform layout for one inline `#transform` plan.
    ///
    /// Renders the whole old and new formulas and extracts each glyph /
    /// decoration (fraction bar, root, …) as a positioned graphical unit, using
    /// Typst's *own* SVG layout — no custom token scanner (smart splitting: one
    /// unit per real shape). Old↔new units are then matched by shape identity +
    /// geometric proximity ([`match_shapes`], Manim `TransformMatchingShapes`
    /// style): surviving units glide smoothly to their nearest same-shaped
    /// counterpart (smart positioning), deleted units fade out into the nearest
    /// survivor, inserted units fade in from the nearest survivor. Returns
    /// `None` (leaving the legacy crossfade intact) if either body yields no
    /// extractable units.
    pub(crate) fn build_transform_fragments(
        &self,
        plan: &crate::core::ast::TransformPlan,
    ) -> Option<TransformFragmentPlan> {
        let preamble = imports_preamble(&self.scene);
        let old_svg = self.render_formula_svg(&plan.old_body, &preamble)?;
        let new_svg = self.render_formula_svg(&plan.new_body, &preamble)?;
        let (old_inner, old_frags) = Self::extract_formula(&old_svg)?;
        let (new_inner, new_frags) = Self::extract_formula(&new_svg)?;
        if old_frags.is_empty() || new_frags.is_empty() {
            return None;
        }

        // Smart matching (Manim `TransformMatchingShapes` style): shapes are
        // already split per graphical unit by `extract_formula` (each glyph
        // outline / fraction bar / root is its own leaf). We now pair old↔new
        // units *by shape identity + geometric proximity* rather than by
        // left-to-right source order, so repeated glyphs travel the shortest
        // path to their counterpart and never needlessly cross.
        let old_u: Vec<ShapeUnit> = old_frags.iter().map(ShapeUnit::from_frag).collect();
        let new_u: Vec<ShapeUnit> = new_frags.iter().map(ShapeUnit::from_frag).collect();
        let matched = match_shapes(&old_u, &new_u);
        let matched_old: std::collections::HashSet<usize> =
            matched.iter().map(|(o, _)| *o).collect();
        let matched_new: std::collections::HashSet<usize> =
            matched.iter().map(|(_, n)| *n).collect();

        // Anchors for the fade halves: a deleted old unit collapses toward the
        // new counterpart of its geometrically nearest *matched* old neighbour,
        // and an inserted new unit emerges from the old counterpart of its
        // nearest *matched* new neighbour. This makes insertions/deletions grow
        // out of / dissolve into the surrounding surviving content (smart
        // positioning) instead of jumping to an arbitrary sequence neighbour.
        let tl = |b: &(f64, f64, f64, f64)| (b.0 / PT_PER_CM, b.1 / PT_PER_CM);
        let mut anims: Vec<GlyphAnim> = Vec::new();

        // Matched units glide smoothly, staying fully opaque.
        for (o, n) in &matched {
            let (fx, fy) = tl(&old_frags[*o].0);
            let (tx, ty) = tl(&new_frags[*n].0);
            anims.push(GlyphAnim {
                src: 0,
                bx0: old_frags[*o].0 .0,
                by0: old_frags[*o].0 .1,
                bx1: old_frags[*o].0 .2,
                by1: old_frags[*o].0 .3,
                from_x: fx,
                from_y: fy,
                to_x: tx,
                to_y: ty,
                from_op: 1.0,
                to_op: 1.0,
                // Matched glyph draws from the new formula after the midpoint, so
                // it needs the *new* glyph's own clip box (not its old box, which
                // would land on empty space in the new formula).
                nbx0: new_frags[*n].0 .0,
                nby0: new_frags[*n].0 .1,
                nbx1: new_frags[*n].0 .2,
                nby1: new_frags[*n].0 .3,
            });
        }

        // Deleted old units fade out, drifting toward the new position of their
        // nearest surviving old neighbour (or their own spot if none matched).
        for (o, u) in old_u.iter().enumerate() {
            if matched_old.contains(&o) {
                continue;
            }
            let (fx, fy) = tl(&old_frags[o].0);
            let (tx, ty) =
                nearest_matched(u.cx, u.cy, &matched, &old_u, |&(_, n)| tl(&new_frags[n].0))
                    .unwrap_or((fx, fy));
            anims.push(GlyphAnim {
                src: 0,
                bx0: old_frags[o].0 .0,
                by0: old_frags[o].0 .1,
                bx1: old_frags[o].0 .2,
                by1: old_frags[o].0 .3,
                from_x: fx,
                from_y: fy,
                to_x: tx,
                to_y: ty,
                from_op: 1.0,
                to_op: 0.0,
                nbx0: old_frags[o].0 .0,
                nby0: old_frags[o].0 .1,
                nbx1: old_frags[o].0 .2,
                nby1: old_frags[o].0 .3,
            });
        }

        // Inserted new units fade in, emerging from the old position of their
        // nearest surviving new neighbour (or their own spot if none matched).
        for (n, u) in new_u.iter().enumerate() {
            if matched_new.contains(&n) {
                continue;
            }
            let (tx, ty) = tl(&new_frags[n].0);
            let (fx, fy) =
                nearest_matched_new(u.cx, u.cy, &matched, &new_u, |&(o, _)| tl(&old_frags[o].0))
                    .unwrap_or((tx, ty));
            anims.push(GlyphAnim {
                src: 1,
                bx0: new_frags[n].0 .0,
                by0: new_frags[n].0 .1,
                bx1: new_frags[n].0 .2,
                by1: new_frags[n].0 .3,
                from_x: fx,
                from_y: fy,
                to_x: tx,
                to_y: ty,
                from_op: 0.0,
                to_op: 1.0,
                nbx0: new_frags[n].0 .0,
                nby0: new_frags[n].0 .1,
                nbx1: new_frags[n].0 .2,
                nby1: new_frags[n].0 .3,
            });
        }
        Some(TransformFragmentPlan {
            target: plan.target.clone(),
            old: plan.old.clone(),
            start_ms: plan.start_ms,
            end_ms: plan.end_ms,
            easing: plan.easing.clone(),
            old_inner,
            new_inner,
            anims,
        })
    }

    /// Render a body at the page origin (top-left) and return its SVG string.
    /// Rendering at the origin means each fragment's measured bbox (in page pt)
    /// is already relative to the formula's top-left — exactly the offset the
    /// compositor adds to the target mobject's position at render time.
    fn render_formula_svg(&self, body: &str, preamble: &str) -> Option<String> {
        let src = place_source(
            self.page_w,
            self.page_h,
            0.0,
            0.0,
            100.0,
            0.0,
            body,
            preamble,
        );
        let doc = self.compile_cached(&src, &Dict::new()).ok()?;
        let page = doc.pages().first()?;
        Some(typst_svg::svg(page, &SvgOptions::default()))
    }

    /// Extract every positioned glyph / decoration from a formula's SVG (as
    /// emitted by `typst_svg`). Returns the SVG's inner markup (kept verbatim
    /// so the whole formula — `<defs>` included — can be re-embedded later for
    /// clip+translate compositing) and, for each top-level drawable, its
    /// absolute bounding box (pt, in the SVG's own coordinate space) plus a
    /// stable signature used to match the same glyph across the old and new
    /// formulas.
    ///
    /// Typst renders every glyph as `<use xlink:href="#sym">` referencing a
    /// `<path>` outline inside `<defs>`, and decorations (fraction bars, roots,
    /// …) as `<path>` elements. We walk the DOM, compute each element's bbox by
    /// applying its (and ancestors') transforms to its path geometry, and sign
    /// it by the path data so identical glyphs match across formulas.
    fn extract_formula(
        svg: &str,
    ) -> Option<(String, Vec<crate::renderer::typst::svg::FormulaLeaf>)> {
        let doc = roxmltree::Document::parse(svg).ok()?;
        let root = doc.root_element();
        // Inner markup: everything between `<svg …>` and `</svg>`.
        let inner = {
            let open = svg.find("<svg")?;
            let after = open + svg[open..].find('>')? + 1;
            let end = svg.rfind("</svg>")?;
            svg[after..end].to_string()
        };
        // Gather symbol path data from <defs> so <use> can resolve outlines.
        let mut symbols: HashMap<String, String> = HashMap::new();
        for defs in root.children().filter(|e| e.tag_name().name() == "defs") {
            for sym in defs.children().filter(|e| e.tag_name().name() == "symbol") {
                if let Some(id) = sym.attribute("id") {
                    if let Some(path) = sym.children().find(|e| e.tag_name().name() == "path") {
                        if let Some(d) = path.attribute("d") {
                            symbols.insert(id.to_string(), d.to_string());
                        }
                    }
                }
            }
        }
        let mut frags = Vec::new();
        for child in root.children() {
            if child.is_element() && child.tag_name().name() != "defs" {
                collect_formula_leaves(
                    &child,
                    (1.0, 0.0, 0.0, 1.0, 0.0, 0.0),
                    &symbols,
                    &mut frags,
                );
            }
        }
        if frags.is_empty() {
            None
        } else {
            Some((inner, frags))
        }
    }

    /// Whether `label` is hidden by an active per-glyph transform (its `target`
    /// or `old` mobject), so the renderer can draw the interpolated fragments
    /// instead. Only plans that actually produced fragments hide their labels.
    pub(crate) fn transform_hidden(&self, label: &Label, time_ms: u32) -> bool {
        for p in &self.transform_fragments {
            // Inclusive of `end_ms`: at the final frame the interpolated overlay
            // draws the exact target formula (see `transform_progress`), so the
            // base `target`/`old` mobjects must stay hidden through `end_ms` to
            // avoid a double-draw / the old formula flashing on the last frame.
            if time_ms >= p.start_ms
                && time_ms <= p.end_ms
                && (&p.target == label || &p.old == label)
            {
                return true;
            }
        }
        false
    }

    /// Interpolated transform state for the plan `p` at `time_ms`: the target
    /// mobject's current top-left (cm), the eased progress `te`, and the target's
    /// current `scale` / `rotation` (so a `transform` can be combined with other
    /// `#animate` tracks on the same label — e.g. the formula can glide *and*
    /// scale/spin at once). The target's crossfade opacity is intentionally
    /// **not** returned: the per-glyph fragments already encode their own
    /// opacity curves (matched glyphs stay opaque, inserted/deleted ones fade),
    /// and multiplying by the target's 0→1 fade would make surviving characters
    /// flicker in and out. Returns `None` when the frame is outside the plan
    /// window.
    fn transform_progress(
        &self,
        p: &TransformFragmentPlan,
        states: &HashMap<Label, FrameData>,
        time_ms: u32,
    ) -> Option<(f64, f64, f64, f64, f64)> {
        if time_ms < p.start_ms || time_ms > p.end_ms {
            return None;
        }
        let nat = self.nat.get(&p.target).cloned().unwrap_or((0.0, 0.0));
        let nat_cm = (nat.0 / PT_PER_CM, nat.1 / PT_PER_CM);
        let (sx, sy, scale, rot) = match states.get(&p.target) {
            Some(s) => (nat_cm.0 + s.x, nat_cm.1 + s.y, s.scale, s.rotation),
            None => (nat_cm.0, nat_cm.1, 1.0, 0.0),
        };
        // At the final frame (`end_ms`) the eased progress is forced to exactly
        // 1.0 so the overlay reconstructs the *target* formula pixel-for-pixel
        // (matched glyphs land on their new positions, inserted units are fully
        // opaque, deleted units fully faded). This is what makes the last frame
        // show the target rather than an intermediate (te < 1) morph state. The
        // force is explicit (not `easing(1.0)`) because some easings — e.g.
        // `ThereAndBack` — do not return 1.0 at t = 1.
        let te = if time_ms == p.end_ms {
            1.0
        } else {
            let denom = (p.end_ms - p.start_ms).max(1) as f64;
            let t = (((time_ms - p.start_ms) as f64) / denom).clamp(0.0, 1.0);
            (p.easing.resolve())(t)
        };
        Some((sx, sy, te, scale, rot))
    }

    /// Emit the per-glyph transform SVG overlay for the current frame.
    ///
    /// For each active plan the whole old/new formula is embedded exactly ONCE
    /// (its inner markup, with symbol ids localized under a per-plan prefix and
    /// wrapped in a single `<g id=…>` inside `<defs>`), then each fragment is
    /// drawn as a clipped `<use>` of that group. Embedding the formula once
    /// (instead of repeating the markup inside every fragment's clip) keeps the
    /// SVG small and prevents neighbouring glyphs from leaking through a
    /// slightly-off clip box (the "residual garbage" artefact). The clip + the
    /// translate follow the target mobject (nat + state) so the transform stays
    /// aligned with the rest of the scene.
    pub(crate) fn transform_overlay_svg(
        &self,
        states: &HashMap<Label, FrameData>,
        time_ms: u32,
    ) -> String {
        let mut out = String::new();
        // Pass 1: symbol definitions (one <defs> per active plan). Active only
        // during the transform window, inclusive of `end_ms` so the final frame
        // can still draw the exact target formula. Before `start_ms` the defs
        // are omitted to keep the SVG small and to avoid polluting symbol-count
        // assertions in tests that compare before/after frames.
        for (pi, p) in self.transform_fragments.iter().enumerate() {
            if time_ms < p.start_ms || time_ms > p.end_ms {
                continue;
            }
            let prefix = self.transform_id_prefix(&p.target, pi);
            let old_g = format!("tf_{prefix}_old");
            let new_g = format!("tf_{prefix}_new");
            out.push_str(&format!(
                "<defs><g id=\"{old_g}\">{old}</g><g id=\"{new_g}\">{new}</g></defs>\n",
                old = localize_formula_ids(&p.old_inner, &old_g),
                new = localize_formula_ids(&p.new_inner, &new_g),
            ));
        }
        // Pass 2: the clipped, translated <use> for every fragment.
        for (pi, p) in self.transform_fragments.iter().enumerate() {
            let Some((sx, sy, te, scale, rot)) = self.transform_progress(p, states, time_ms) else {
                continue;
            };
            let prefix = self.transform_id_prefix(&p.target, pi);
            let old_g = format!("tf_{prefix}_old");
            let new_g = format!("tf_{prefix}_new");
            for (idx, f) in p.anims.iter().enumerate() {
                let lx = f.from_x + (f.to_x - f.from_x) * te;
                let ly = f.from_y + (f.to_y - f.from_y) * te;
                let op = f.from_op + (f.to_op - f.from_op) * te;
                if op <= 0.001 {
                    continue;
                }
                // Draw from the source this fragment was assigned at layout time,
                // without inheriting the target's artificial crossfade opacity.
                let grp = if f.src == 0 { &old_g } else { &new_g };
                // Crop from the *same* formula we draw (old or new).
                let (bbx0, bby0, bbx1, bby1) = if *grp == new_g {
                    (f.nbx0, f.nby0, f.nbx1, f.nby1)
                } else {
                    (f.bx0, f.by0, f.bx1, f.by1)
                };
                // Translate so the fragment's *center* lands at the interpolated
                // page position of the glyph center — i.e. the interpolated top-left
                // (sx+lx, sy+ly) plus the (scale*rotate)-transformed offset from
                // top-left to center — then pivot scale/rotation about that center so
                // a simultaneous `#animate` (including translation) composes with the
                // transform. (Previously this subtracted the full center coordinate,
                // shifting every fragment ~2·center toward the top-left — the
                // "scattered fragments / ghost" artefact.)
                let tx = (sx + lx) * PT_PER_CM;
                let ty = (sy + ly) * PT_PER_CM;
                let cx = (bbx0 + bbx1) / 2.0;
                let cy = (bby0 + bby1) / 2.0;
                let r = rot.to_radians();
                let (s, c) = (r.sin(), r.cos());
                let dx = cx - bbx0; // center offset from top-left
                let dy = cy - bby0;
                let px = tx + scale * (c * dx - s * dy);
                let py = ty + scale * (s * dx + c * dy);
                let ncx = -cx;
                let ncy = -cy;
                let mtx = format!(
                    "translate({px:.4}, {py:.4}) rotate({rot:.4}) scale({scale:.4}) translate({ncx:.4}, {ncy:.4})",
                    px = px,
                    py = py,
                    rot = rot,
                    scale = scale,
                    ncx = ncx,
                    ncy = ncy,
                );
                // Pad the clip rect on *all* sides; the previous right/bottom-only
                // padding clipped the left/top edges of moving glyphs.
                let pad = 3.0; // pt
                let bw = (bbx1 - bbx0).max(0.0) + 2.0 * pad;
                let bh = (bby1 - bby0).max(0.0) + 2.0 * pad;
                let cid = format!("tf_{prefix}_{idx}");
                out.push_str(&format!(
                    "<g opacity=\"{op:.4}\" transform=\"{mtx}\">\n\
                     <clipPath id=\"{cid}\"><rect x=\"{bx0:.4}\" y=\"{by0:.4}\" width=\"{bw:.4}\" height=\"{bh:.4}\"/></clipPath>\n\
                     <use xlink:href=\"#{grp}\" clip-path=\"url(#{cid})\"/>\n</g>\n",
                    op = op,
                    mtx = mtx,
                    bx0 = bbx0 - pad,
                    by0 = bby0 - pad,
                    bw = bw,
                    bh = bh,
                ));
            }
        }
        out
    }

    /// SVG overlay for active morph pairs. For each `#morph(from, to)` pair
    /// whose `[start_ms, end_ms]` window contains `time_ms`, the morphed
    /// polygon is emitted as an SVG `<path>` element at the `to` object's
    /// natural position (with the `to` object's per-frame transform applied).
    /// The `from` object is already being faded/shrunk by the scheduler's
    /// crossfade actions, so only the morphed shape needs to be drawn here.
    pub(crate) fn morph_overlay_svg(
        &self,
        states: &HashMap<Label, FrameData>,
        time_ms: u32,
    ) -> String {
        let mut out = String::new();
        for pair in &self.scene.morph_pairs {
            if time_ms < pair.start_ms || time_ms > pair.end_ms {
                continue;
            }
            let key = (pair.from.clone(), pair.to.clone());
            let Some(plan) = self.morph_cache.get(&key) else {
                continue;
            };
            let denom = (pair.end_ms - pair.start_ms).max(1) as f64;
            let p = (((time_ms - pair.start_ms) as f64) / denom).clamp(0.0, 1.0);
            let eased = pair.easing.resolve()(p);
            let ring = plan.at(eased);
            if ring.is_empty() {
                continue;
            }
            // Position the morphed polygon at the `to` object's current
            // transform (natural position + animate offset + scale + rotation).
            let st = states.get(&pair.to);
            let (tx, ty, scale, rot) = if let Some(st) = st {
                let nat = self.nat.get(&pair.to).cloned().unwrap_or((0.0, 0.0));
                let nat_cm = (nat.0 / super::PT_PER_CM, nat.1 / super::PT_PER_CM);
                (
                    (nat_cm.0 + st.x) * super::PT_PER_CM,
                    (nat_cm.1 + st.y) * super::PT_PER_CM,
                    st.scale,
                    st.rotation,
                )
            } else {
                let nat = self.nat.get(&pair.to).cloned().unwrap_or((0.0, 0.0));
                (nat.0, nat.1, 1.0, 0.0)
            };
            let path = super::polygon_svg(&ring, &plan.fill, &plan.stroke);
            // The morph polygon is always fully visible during the morph window.
            // The crossfade is handled by the `from` object's FadeOut + ScaleBy
            // actions, not by the morph polygon's opacity. Using the `to`
            // object's state opacity would make the morph nearly invisible
            // (the `to` object is Hidden at morph start and FadeIn's from 0).
            if rot.abs() < 0.01 {
                out.push_str(&format!(
                    "<g transform=\"translate({tx:.4},{ty:.4}) scale({scale:.4})\">\n{path}\n</g>\n",
                ));
            } else {
                out.push_str(&format!(
                    "<g transform=\"translate({tx:.4},{ty:.4}) rotate({rot:.4}) scale({scale:.4})\">\n{path}\n</g>\n",
                ));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::{match_shapes, ShapeUnit};

    fn unit(sig: &str, cx: f64, cy: f64) -> ShapeUnit {
        ShapeUnit {
            sig: sig.to_string(),
            cx,
            cy,
        }
    }

    /// Repeated identical glyphs must be paired by geometric proximity, not by
    /// source order. Old `x`s at 0 and 100 with new `x`s at 110 and 5 must pair
    /// (0↔5) and (100↔110) — the shortest total travel — even though the naive
    /// order-based pairing would be (0↔110), (100↔5).
    #[test]
    fn match_shapes_pairs_by_proximity_not_order() {
        let old = vec![unit("x", 0.0, 0.0), unit("x", 100.0, 0.0)];
        let new = vec![unit("x", 110.0, 0.0), unit("x", 5.0, 0.0)];
        let mut m = match_shapes(&old, &new);
        m.sort();
        assert_eq!(
            m,
            vec![(0, 1), (1, 0)],
            "expected nearest-neighbour pairing"
        );
    }

    /// Only same-signature units are ever paired; a differing glyph is left
    /// unmatched (to be faded), and the matched count equals the per-signature
    /// multiset intersection size.
    #[test]
    fn match_shapes_respects_signature_and_multiset_count() {
        // old: a, +, b, =, c   new: a, +, b, +, d, =, c
        let old = vec![
            unit("a", 0.0, 0.0),
            unit("+", 10.0, 0.0),
            unit("b", 20.0, 0.0),
            unit("=", 30.0, 0.0),
            unit("c", 40.0, 0.0),
        ];
        let new = vec![
            unit("a", 0.0, 0.0),
            unit("+", 10.0, 0.0),
            unit("b", 20.0, 0.0),
            unit("+", 25.0, 0.0),
            unit("d", 30.0, 0.0),
            unit("=", 40.0, 0.0),
            unit("c", 50.0, 0.0),
        ];
        let m = match_shapes(&old, &new);
        // 5 old units all have a same-signature counterpart -> 5 matched.
        assert_eq!(m.len(), 5, "matched: {m:?}");
        // The single old '+' must pair with the nearest new '+' (index 1 @10),
        // not the farther one (index 3 @25).
        let plus = m.iter().find(|(o, _)| *o == 1).unwrap();
        assert_eq!(plus.1, 1, "old '+' should pair with nearest new '+'");
    }
}
