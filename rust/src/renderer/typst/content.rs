//! Per-frame Typst source assembly: preamble re-declaration, the content
//! timeline (`content_for`), AST-driven `ecval(...)` counter substitution, and
//! subtitle placement / compilation.

use std::panic::{AssertUnwindSafe, catch_unwind};
use typst::compile;
use typst_layout::PagedDocument;
use typst_library::foundations::Dict;
use typst_svg::{SvgOptions, svg};
use typst_syntax::ast::{self, Expr};
use typst_syntax::{LinkedNode, parse_code};

use crate::core::ast::{Label, Scene, SubPos, Subtitle};
use crate::core::diag::CandyError;

use super::world::{CandyWorld, WorldState};

/// Build a Typst preamble that re-declares every `@preview`/package import
/// captured from the source `.tyx`, so the detached per-object compile snippets
/// (which would otherwise lose the binding) can reference package symbols used
/// inside mobject bodies.
pub(crate) fn imports_preamble(scene: &Scene) -> String {
    if scene.imports.is_empty() {
        String::new()
    } else {
        let mut s = String::new();
        for imp in &scene.imports {
            s.push_str(imp);
            s.push('\n');
        }
        s
    }
}

/// Resolve the Typst body for `label` at frame time `time_ms`.
///
/// A `transform` records content switches on `Scene.content_timeline` as
/// `(time_ms, new_body)` pairs. For a given frame we use the latest switch
/// whose `time_ms <= frame`, falling back to `items[label]` (the original
/// body) before any transform. This lets a single label render different
/// content before/after a `transform` without corrupting earlier slides.
pub(crate) fn content_for(scene: &Scene, label: &Label, time_ms: u32) -> (String, Vec<String>) {
    let body = if let Some(timeline) = scene.content_timeline.get(label) {
        let mut chosen: Option<&String> = None;
        for (t, body) in timeline {
            if *t <= time_ms {
                chosen = Some(body);
            }
        }
        if let Some(b) = chosen {
            b.clone()
        } else {
            scene.items.get(label).cloned().unwrap_or_default()
        }
    } else {
        scene.items.get(label).cloned().unwrap_or_default()
    };
    // Substitute `ecval(name)` counter references with their integer value at
    // this frame (honoring shadowing + lifecycle).
    substitute_counters(scene, &body, time_ms)
}

/// Replace every `ecval("name")` (or `ecval(name)`) counter reference in `body`
/// with the integer value of counter `name` at `time_ms`, per the scene's scope
/// shadowing / lifecycle rules.
///
/// Returns `(substituted_body, unknown_counters)` where `unknown_counters` is a
/// list of counter names that were referenced but not declared (to be reported
/// as E009 errors).
///
/// Expansion is **AST-driven**, not naive string replacement: `body` is parsed
/// into a Typst `SyntaxNode` tree and every *real* `ecval(..)` function-call
/// node is swapped for an integer literal. This keeps `ecval` a valid AST node
/// that composes like any other Typst expression (e.g. inside
/// `rect(width: ecval("n") * 1cm)`) and avoids rewriting substrings that merely
/// *look* like the call (inside strings, comments, …). The canonical call form
/// is `ecval("name")` (a quoted string); the bare-ident form `ecval(name)` is
/// also accepted for backwards compatibility with existing `.tyx` sources.
pub(crate) fn substitute_counters(
    scene: &Scene,
    body: &str,
    time_ms: u32,
) -> (String, Vec<String>) {
    // Fast path: no counter read at all → short-circuit.
    if !body.contains("ecval") {
        return (body.to_string(), Vec::new());
    }
    // Parse as *code* (the body is a Typst expression, not a markup document),
    // so `ecval(..)` parses to a real `FuncCall` node whose source range maps
    // 1:1 onto `body`.
    let root = parse_code(body);
    let node = LinkedNode::new(&root);

    // Collect (source range → replacement) for every `ecval(..)` call.
    let mut edits: Vec<(std::ops::Range<usize>, String)> = Vec::new();
    let mut unknown_counters: Vec<String> = Vec::new();
    collect_ecval_edits(&node, scene, time_ms, &mut edits, &mut unknown_counters);
    // Drop any edit whose range is nested inside another (a nested `ecval`),
    // keeping the innermost node so we never clobber an already-replaced child.
    let drop: Vec<bool> = edits
        .iter()
        .map(|(r, _)| {
            edits
                .iter()
                .any(|(o, _)| o != r && o.start <= r.start && r.end <= o.end)
        })
        .collect();
    let mut kept: Vec<(std::ops::Range<usize>, String)> = Vec::new();
    for (keep, e) in drop.into_iter().zip(edits) {
        if !keep {
            kept.push(e);
        }
    }
    let mut edits = kept;
    if edits.is_empty() {
        return (body.to_string(), unknown_counters);
    }
    // Apply right-to-left so earlier edits don't invalidate later offsets.
    edits.sort_by_key(|e| std::cmp::Reverse(e.0.start));
    let mut out = body.to_string();
    for (range, text) in edits {
        out.replace_range(range, &text);
    }
    (out, unknown_counters)
}

/// Walk `node`, appending an edit that swaps each `ecval(name)` call for its
/// current integer value (only for counters actually declared in the scene).
/// Collects unknown counter names for E009 reporting.
fn collect_ecval_edits(
    node: &LinkedNode,
    scene: &Scene,
    time_ms: u32,
    edits: &mut Vec<(std::ops::Range<usize>, String)>,
    unknown_counters: &mut Vec<String>,
) {
    if let Some(call) = node.get().cast::<ast::FuncCall>() {
        if let Some(name) = ecval_counter_name(&call) {
            // Only substitute declared counters; collect unknown ones for E009.
            if !scene.counters.iter().any(|c| c.name == name) {
                if !unknown_counters.contains(&name) {
                    unknown_counters.push(name);
                }
                return;
            }
            let val = scene.counter_value_at(&name, time_ms).to_string();
            edits.push((node.range(), val));
        }
    }
    for child in node.children() {
        collect_ecval_edits(&child, scene, time_ms, edits, unknown_counters);
    }
}

/// If `call` is an `ecval(..)` read, return the counter name it references.
fn ecval_counter_name(call: &ast::FuncCall) -> Option<String> {
    let is_ecval = match call.callee() {
        Expr::Ident(id) => id.as_str() == "ecval",
        Expr::FieldAccess(fa) => fa.field().as_str() == "ecval",
        _ => false,
    };
    if !is_ecval {
        return None;
    }
    // The first positional argument is the counter name. A leading named
    // argument means this isn't the canonical read form → bail.
    match call.args().items().next() {
        Some(ast::Arg::Pos(p)) => match p {
            Expr::Str(s) => Some(s.get().to_string()),
            Expr::Ident(i) => Some(i.as_str().to_string()),
            _ => None,
        },
        _ => None,
    }
}

/// Inset (in cm) from the page edge for the named subtitle anchors.
const SUBTITLE_MARGIN_CM: f64 = 1.0;

/// Build the Typst `place(...)` expression that anchors a subtitle's body,
/// keeping the caption fully inside the viewport. Named anchors use
/// alignment (e.g. `bottom + center`) so the caption's box hugs the requested
/// edge instead of overflowing it — the old code placed the box's *top-left*
/// corner at the anchor, which pushed bottom/top captions off-screen.
fn subtitle_place_expr(sub: &Subtitle, margin: f64) -> String {
    match sub.position {
        SubPos::Absolute(x, y) => {
            // Anchor the box's top-left corner at the absolute (x, y) in cm.
            format!("place(top + left, dx: {x}cm, dy: {y}cm)")
        }
        SubPos::Bottom => format!("place(bottom + center, dy: -{margin}cm)"),
        SubPos::Top => format!("place(top + center, dy: {margin}cm)"),
        SubPos::Center => "place(center + center)".to_string(),
        SubPos::BottomLeft => {
            format!("place(bottom + left, dx: {margin}cm, dy: -{margin}cm)")
        }
        SubPos::BottomRight => {
            format!("place(bottom + right, dx: -{margin}cm, dy: -{margin}cm)")
        }
        SubPos::TopLeft => format!("place(top + left, dx: {margin}cm, dy: {margin}cm)"),
        SubPos::TopRight => {
            format!("place(top + right, dx: -{margin}cm, dy: {margin}cm)")
        }
    }
}

/// Compile a subtitle's body to a single-page Typst document, placed at the
/// subtitle's resolved anchor and with `ecval(...)` counters substituted.
pub(crate) fn subtitle_doc(
    state: &WorldState,
    scene: &Scene,
    sub: &Subtitle,
    page_w: f64,
    page_h: f64,
    time_ms: u32,
) -> Result<PagedDocument, CandyError> {
    let (body, _unknown) = substitute_counters(scene, &sub.body, time_ms);
    let preamble = imports_preamble(scene);
    let pre = if preamble.is_empty() {
        String::new()
    } else {
        format!("{preamble}\n")
    };
    let place = subtitle_place_expr(sub, SUBTITLE_MARGIN_CM);

    // Fixed style: white text with black stroke for maximum contrast on any background
    let src = format!(
        "{pre}#set page(width: {pw}pt, height: {ph}pt, margin: 0pt, fill: none)\n\
         #set text(fill: white, stroke: black + 0.025em)\n\
         #{place}[#{{ ({body}) }}]\n",
        pw = page_w,
        ph = page_h,
    );
    let source = state.main_source(&src);
    let world = CandyWorld::new(state, source, Dict::new());
    // Mirror `Renderer::compile`: a malformed body can make typst panic rather
    // than return a diagnostic — catch it so the error is always reported as
    // `E006` instead of crashing the process (notably in release builds).
    let warned = match catch_unwind(AssertUnwindSafe(|| compile::<PagedDocument>(&world))) {
        Ok(w) => w,
        Err(payload) => {
            let msg = if let Some(s) = payload.downcast_ref::<String>() {
                s.clone()
            } else if let Some(s) = payload.downcast_ref::<&str>() {
                s.to_string()
            } else {
                "typst panicked during compilation".to_string()
            };
            return Err(CandyError::Typst(format!("typst panicked: {msg}"), None));
        }
    };
    match warned.output {
        Ok(doc) => Ok(doc),
        Err(errs) => {
            let loc = errs
                .first()
                .and_then(|d| super::typst_diag_loc(&world, d, std::path::Path::new("")));
            Err(CandyError::Typst(
                crate::core::diag::format_typst_errors(&errs),
                loc,
            ))
        }
    }
}

/// Render a subtitle to an SVG string (used by the SVG frame path).
pub(crate) fn render_subtitle_svg_impl(
    state: &WorldState,
    scene: &Scene,
    sub: &Subtitle,
    page_w: f64,
    page_h: f64,
    time_ms: u32,
) -> Result<String, CandyError> {
    let doc = subtitle_doc(state, scene, sub, page_w, page_h, time_ms)?;
    let page = doc
        .pages()
        .first()
        .ok_or_else(|| CandyError::Typst("document produced no pages".into(), None))?;
    Ok(svg(page, &SvgOptions::default()))
}
