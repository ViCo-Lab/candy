//! Expression evaluation + Candy-symbol resolution for the `.tyx` parser.
//!
//! This module is the *pure* layer of the parser: it knows how to read values
//! out of Typst AST expression nodes (lengths, numbers, tuples, arrays) and
//! how to resolve a function call's *callee* to a Candy directive — but it
//! does not walk the tree or mutate parse state beyond reading it.
//!
//! Detection is **import-agnostic** for bare identifiers: a call `mobject(...)`
//! is treated as Candy iff its resolved name matches a Candy symbol that was
//! actually imported (see [`crate::parser::ast_walk`]). For *field access*
//! (`candy.mobject(...)`) the receiver must be a known Candy module alias, so
//! ordinary method calls like `obj.morph()` are never mistaken for Candy.

use std::collections::HashMap;
use std::ops::Range;

use typst_syntax::LinkedNode;
use typst_syntax::SyntaxNode;
use typst_syntax::ast::{self, AstNode, Expr};

use crate::core::diag::CandyWarn;
use crate::warn;
use crate::parser::ast_walk::ParseCtx;

/// The Candy symbol names recognized as directives.
pub(crate) const CANDY: &[&str] = &[
    "mobject",
    "animate",
    "pause",
    "audio",
    "play",
    // Manim-inspired state / indication / visibility directives.
    "save-state",
    "restore",
    "indicate",
    "flash",
    "wiggle",
    "appear",
    "disappear",
    "set-color",
    // Manim-inspired composite animations.
    "blink",
    "spiral-in",
    "focus-on",
    "fade-transform",
    "move-along-path",
    "morph",
    // Manim-style single-object content transform (the headline `Transform` /
    // `ReplacementTransform`): morph a target mobject into NEW inline content
    // (e.g. an equation), keeping the original label reusable afterwards.
    "transform",
    // Multi-keyframe track: drive one target through several keyframes, each
    // controlling a subset of its properties. Mirrors a timeline track.
    "track",
    // Global camera pan / zoom / rotate.
    "camera",
    // Parent→child grouping: children inherit the parent's transform.
    "group",
    // Progressive text reveal (per-char / per-word) and typewriter.
    "reveal",
    "typewriter",
    // Subtitle module.
    "subtitle",
    // Scene module: `scene` establishes a nestable, scope-bounded, one-page
    // segment of the timeline (parent auto-hides when a child is active).
    "scene",
    // Easing-counter module: `ecounter` defines a named integer
    // counter, `ecval` reads its current integer value (substituted per-frame by
    // the renderer), and `counter_pause` / `counter_resume` / `counter_destroy`
    // drive its lifecycle. `ecval` is a no-op at parse time (the parser never
    // acts on reads — only definitions and lifecycle events matter).
    "ecounter",
    "ecval",
    "counter-pause",
    "counter-resume",
    "counter-destroy",
];

/// Resolve a function call to its Candy symbol (or `None` if it isn't one).
///
/// Works for `mobject(...)` (imported via `#import "candy": *` or
/// `#import "candy": mobject as mob`), `candy.mobject(...)` (field access on
/// the Candy module alias), and renamed imports.
///
/// Crucially, this does **not** treat an arbitrary field access as Candy:
/// `obj.morph()` or `dict.animate()` are ordinary user code and return `None`.
pub(crate) fn call_symbol(call: &ast::FuncCall, ctx: &ParseCtx) -> Option<String> {
    let callee = call.callee();
    match callee {
        Expr::Ident(id) => {
            let name = id.as_str();
            // Accept both naming conventions: the public API and the Typst
            // module use underscores (`save_state`, `set_color`,
            // `counter_pause`), while the parser's `CANDY` set uses kebab-case
            // (`save-state`, `set-color`). Normalize so a call resolves
            // regardless of which convention the author wrote.
            let norm = name.replace('_', "-");
            ctx.symbol_map
                .get(&norm)
                .or_else(|| ctx.symbol_map.get(name))
                .filter(|o| CANDY.contains(&o.as_str()))
                .cloned()
        }
        Expr::FieldAccess(fa) => {
            // Only treat a field access as a Candy pseudo-function when the
            // receiver is the *Candy module itself* (e.g. `candy.mobject` after
            // `#import "candy"` / `#import "candy" as c`). A field access on any
            // other value (e.g. `obj.morph`, `self.animate`) is ordinary user
            // code and must NOT be parsed as a Candy directive.
            let Expr::Ident(recv) = fa.target() else {
                return None;
            };
            if !ctx.candy_aliases.contains(recv.as_str()) {
                return None;
            }
            let field = fa.field().as_str();
            let norm = field.replace('_', "-");
            if CANDY.contains(&norm.as_str()) {
                Some(norm)
            } else if CANDY.contains(&field) {
                Some(field.to_string())
            } else {
                None
            }
        }
        _ => None,
    }
}

/// The current (innermost) lexical scope id, as a string.
pub(crate) fn current_scope(ctx: &ParseCtx) -> String {
    ctx.scope_stack.last().copied().unwrap_or(0).to_string()
}

/// Resolve the easing named arg, falling back to Linear with a warning.
pub(crate) fn resolve_easing(
    named: &HashMap<String, Expr>,
    label: &crate::core::ast::Label,
) -> crate::core::easing::Easing {
    match named.get("easing") {
        Some(Expr::Str(s)) => {
            let name = s.get();
            match crate::core::easing::Easing::from_str(name.as_str()) {
                Some(e) => e,
                None => {
                    warn!(CandyWarn::UnknownEasing(format!(
                        "'{name}' for @{}",
                        label.0
                    )));
                    crate::core::easing::Easing::Linear
                }
            }
        }
        _ => crate::core::easing::Easing::Linear,
    }
}

/// Extract the target label (positional string arg or `target:` named arg).
pub(crate) fn target_arg(
    pos: &[Expr],
    named: &HashMap<String, Expr>,
) -> Option<crate::core::ast::Label> {
    let e = pos.first().or_else(|| named.get("target"))?;
    match e {
        Expr::Str(s) => Some(crate::core::ast::Label(s.get().to_string())),
        _ => None,
    }
}

/// Try to evaluate an expression as a length in cm. Handles `4cm`, `3in`,
/// `5pt`, bare numbers (treated as cm), and **signed** lengths like `-4cm`
/// (which Typst parses as `Expr::Unary`, not `Expr::Numeric`).
pub(crate) fn expr_length_cm(e: &Expr) -> Option<f64> {
    match e {
        Expr::Numeric(n) => {
            let (val, unit) = n.get();
            return unit_to_cm(val, unit);
        }
        Expr::Int(i) => return Some(i.get() as f64),
        Expr::Float(fl) => return Some(fl.get()),
        // Signed lengths like `-4cm` / `+4cm` parse as a *unary operation*
        // wrapping the inner `Numeric`.
        Expr::Unary(u) => {
            let sign = match u.op() {
                ast::UnOp::Neg => -1.0,
                ast::UnOp::Pos => 1.0,
                ast::UnOp::Not => return None,
            };
            return expr_length_cm(&u.expr()).map(|v| sign * v);
        }
        _ => None,
    }
}

/// Convert a `(value, unit)` pair from Typst's `Numeric` node to centimeters.
pub(crate) fn unit_to_cm(val: f64, unit: ast::Unit) -> Option<f64> {
    match unit {
        ast::Unit::Cm => Some(val),
        ast::Unit::Mm => Some(val * 0.1),
        ast::Unit::Pt => Some(val / crate::parser::ast_walk::PT_PER_CM),
        ast::Unit::In => Some(val * 2.54),
        _ => None,
    }
}

/// Evaluate a unitless numeric expression to `f64`.
pub(crate) fn expr_to_f64(e: &Expr) -> Option<f64> {
    match e {
        Expr::Int(i) => Some(i.get() as f64),
        Expr::Float(f) => Some(f.get()),
        Expr::Numeric(n) => Some(n.get().0),
        _ => None,
    }
}

/// Evaluate a boolean expression.
pub(crate) fn expr_to_bool(e: &Expr) -> Option<bool> {
    match e {
        Expr::Bool(b) => Some(b.get()),
        _ => None,
    }
}

/// Evaluate a unit-less numeric expression to `i64` (for counter seed/step).
pub(crate) fn expr_to_i64(e: &Expr) -> Option<i64> {
    match e {
        Expr::Int(i) => Some(i.get() as i64),
        Expr::Float(f) => Some(f.get().round() as i64),
        Expr::Numeric(n) => Some(n.get().0.round() as i64),
        _ => None,
    }
}

/// If `e` is an array literal `(a, b, ...)` — possibly wrapped in
/// `Expr::Parenthesized` — return the inner `Array` node.
pub(crate) fn as_array<'a>(e: &'a Expr<'a>) -> Option<ast::Array<'a>> {
    match e {
        Expr::Array(a) => Some(a.clone()),
        Expr::Parenthesized(p) => match p.expr() {
            Expr::Array(a) => Some(a.clone()),
            _ => None,
        },
        _ => None,
    }
}

/// Evaluate a `(x, y)` length tuple to centimeters.
pub(crate) fn tuple_cm(e: &Expr, _raw: &str, _node: &LinkedNode) -> Option<(f64, f64)> {
    let arr: ast::Array = match e {
        Expr::Array(a) => a.clone(),
        Expr::Parenthesized(p) => match p.expr() {
            Expr::Array(a) => a,
            _ => return None,
        },
        _ => return None,
    };
    let mut vals = Vec::with_capacity(2);
    for it in arr.items() {
        // AST-based: extract the length directly from the expression node,
        // no string parsing. This handles `4cm`, `0pt`, `10mm`, `1in` via
        // typst_syntax's Numeric AST node (which carries the value + unit).
        let expr = match it {
            ast::ArrayItem::Pos(e) => e,
            ast::ArrayItem::Spread(_) => return None,
        };
        vals.push(expr_length_cm(&expr)?);
    }
    if vals.len() == 2 {
        Some((vals[0], vals[1]))
    } else {
        None
    }
}

/// Recover the source byte range of `target` by identity (pointer) within the
/// `LinkedNode` subtree. This works even for *detached* sources, where Typst
/// assigns no resolvable span numbers and `LinkedNode::find` would fail.
pub(crate) fn range_of(node: &LinkedNode, target: &SyntaxNode) -> Option<Range<usize>> {
    if std::ptr::eq(node.get(), target) {
        return Some(node.range());
    }
    for child in node.children() {
        if let Some(r) = range_of(&child, target) {
            return Some(r);
        }
    }
    None
}

/// Source text of an expression node, recovered by identity within the tree.
pub(crate) fn expr_src<'a>(raw: &'a str, node: &LinkedNode, e: &Expr) -> &'a str {
    match range_of(node, e.to_untyped()) {
        Some(r) => &raw[r],
        None => "",
    }
}

/// If `body` is a string literal `"..."`, return its inner text; else `None`.
pub(crate) fn strip_string_literal(body: &str) -> Option<String> {
    let b = body.trim();
    if b.starts_with('"') && b.ends_with('"') && b.len() >= 2 {
        Some(b[1..b.len() - 1].to_string())
    } else {
        None
    }
}

/// Parse a `#track` keyframe tuple `(t, (x, y, scale, opacity, rotation))` into
/// a [`crate::core::ast::TrackKey`]. `x`/`y` are unit-aware centimeters;
/// `scale`/`opacity`/`rotation` are unitless numbers.
///
/// Matches the format documented in `lib.typ`: `keys` is an array of `(t,
/// (x, y, scale, opacity, rotation))` tuples, e.g.
/// `((0, (1cm, 0cm, 1, 1, 0)), (500, (4cm, 0cm, 1.5, 0.5, 0)))`.
pub(crate) fn track_key_from_expr(e: &Expr) -> Option<crate::core::ast::TrackKey> {
    // `e` is a *single* keyframe tuple `(t, (x, y, scale, opacity, rotation))`.
    // `t` is relative to the slide start (ms); `x`/`y` are unit-aware
    // centimeters; `scale`/`opacity`/`rotation` are unitless numbers. The
    // tuple (and its inner `(x, y, …)` array) may be wrapped in
    // `Expr::Parenthesized`, which `as_array` unwraps.
    let arr = as_array(e)?;
    let parts: Vec<ast::ArrayItem> = arr.items().collect();
    if parts.len() < 2 {
        return None;
    }
    let t = match &parts[0] {
        ast::ArrayItem::Pos(e) => expr_to_f64(e)?,
        _ => return None,
    } as u32;
    let st: Vec<ast::ArrayItem> = match &parts[1] {
        ast::ArrayItem::Pos(e) => match as_array(e) {
            Some(a) => a.items().collect(),
            None => return None,
        },
        _ => return None,
    };
    let x = st.get(0).and_then(|it| match it {
        ast::ArrayItem::Pos(e) => expr_length_cm(e),
        _ => None,
    });
    let y = st.get(1).and_then(|it| match it {
        ast::ArrayItem::Pos(e) => expr_length_cm(e),
        _ => None,
    });
    let scale = st.get(2).and_then(|it| match it {
        ast::ArrayItem::Pos(e) => expr_to_f64(e),
        _ => None,
    });
    let opacity = st.get(3).and_then(|it| match it {
        ast::ArrayItem::Pos(e) => expr_to_f64(e),
        _ => None,
    });
    let rotation = st.get(4).and_then(|it| match it {
        ast::ArrayItem::Pos(e) => expr_to_f64(e),
        _ => None,
    });
    Some(crate::core::ast::TrackKey {
        t,
        x,
        y,
        scale,
        opacity,
        rotation,
    })
}

/// Parse the `position:` named arg of `subtitle` into a
/// [`crate::core::ast::SubPos`].
pub(crate) fn parse_sub_pos(named: &HashMap<String, Expr>) -> crate::core::ast::SubPos {
    let Some(e) = named.get("position") else {
        return crate::core::ast::SubPos::Bottom;
    };
    match e {
        Expr::Str(s) => match s.get().to_ascii_lowercase().as_str() {
            "bottom" => crate::core::ast::SubPos::Bottom,
            "top" => crate::core::ast::SubPos::Top,
            "center" | "centre" => crate::core::ast::SubPos::Center,
            "bottom-left" => crate::core::ast::SubPos::BottomLeft,
            "bottom-right" => crate::core::ast::SubPos::BottomRight,
            "top-left" => crate::core::ast::SubPos::TopLeft,
            "top-right" => crate::core::ast::SubPos::TopRight,
            _ => crate::core::ast::SubPos::Bottom,
        },
        // Absolute `(x, y)` in cm.
        _ => match tuple_cm(e, "", &LinkedNode::new(&typst_syntax::parse(""))) {
            Some((x, y)) => crate::core::ast::SubPos::Absolute(x, y),
            None => crate::core::ast::SubPos::Bottom,
        },
    }
}
