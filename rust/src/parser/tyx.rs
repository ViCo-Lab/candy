//! Parse a `.tyx` (Typst X-sheet) file into a `Scene` AST.
//!
//! The `.tyx` format is **valid standard Typst**: it imports the Candy package
//! and calls plain Candy functions (`mobject`, `animate`, `pause`, `audio`,
//! `play`). This parser is **AST-driven** (built on `typst_syntax`), not a
//! regex scanner: it walks the Typst syntax tree, resolves every call through
//! the file's *imports*, and extracts each directive's arguments from the real
//! expression nodes.
//!
//! Detection is **import-agnostic**: a call is treated as a Candy directive iff
//! its resolved name matches a Candy symbol. So it works whether the user wrote
//! `#import "candy": *` (then `mobject(...)`), `#import "candy"` (then
//! `candy.mobject(...)`), or renamed an import (`#import "candy": animate as
//! anim`). The binding is what matters, not the literal prefix.

use std::collections::HashMap;
use std::ops::Range;
use std::path::Path;

use typst_syntax::ast::{self, AstNode, Expr};
use typst_syntax::LinkedNode;
use typst_syntax::parse;
use typst_syntax::SyntaxNode;

use crate::core::ast::{Action, AudioTrack, FrameData, Label, Scene, Slide};
use crate::core::easing::Easing;
use crate::core::error::CandyError;
use crate::core::meta::PrivateMeta;

/// The Candy symbol names recognized as directives.
const CANDY: &[&str] = &[
    "mobject",
    "animate",
    "pause",
    "audio",
    "play",
    // Manim-inspired state / indication / visibility directives.
    "save_state",
    "restore",
    "indicate",
    "flash",
    "wiggle",
    "appear",
    "disappear",
    "set_color",
    // Manim-inspired composite animations.
    "blink",
    "spiral_in",
    "focus_on",
    "fade_transform",
    "move_along_path",
    "morph",
];

/// Parse `.tyx` file into a `Scene` AST.
///
/// Precondition: `path` exists and is valid UTF-8 (else E001).
/// Postcondition: returns `Ok(Scene)` with validated slides (else E002).
/// `private_metadata` is set to the fixed defaults.
pub fn parse_tyx(path: &Path) -> Result<Scene, CandyError> {
    let raw = std::fs::read_to_string(path)?; // E001 on missing file
    let root = parse(&raw);
    let node = LinkedNode::new(&root);

    let mut ctx = ParseCtx::default();
    walk(&node, &raw, &mut ctx);

    let private = PrivateMeta::default();
    let scene = Scene {
        slides: ctx.slides,
        items: ctx.items,
        initial: ctx.initial,
        audio: ctx.audio,
        imports: ctx.imports.clone(),
        page_size: ctx.page_size_cm.map(|(w, h)| (w * PT_PER_CM, h * PT_PER_CM)),
        private_metadata: private,
    };
    scene.validate().map_err(CandyError::Parse)?; // E002
    Ok(scene)
}

/// Centimeters per Typst point. Must match renderer::typst::PT_PER_CM.
const PT_PER_CM: f64 = 28.346_456_692_913_385;

/// Extract `width` and `height` (in cm) from a `#set page(width: X, height: Y)`
/// condition. Only the first occurrence is recorded; subsequent `set page`
/// calls are ignored (the user is responsible for using a consistent page size).
fn extract_page_size(node: &LinkedNode, ctx: &mut ParseCtx) {
    let mut width: Option<f64> = None;
    let mut height: Option<f64> = None;
    collect_named_lengths(node, &mut |name, cm| {
        match name {
            "width" => width = Some(cm),
            "height" => height = Some(cm),
            _ => {}
        }
    });
    if let (Some(w), Some(h)) = (width, height) {
        ctx.page_size_cm = Some((w, h));
    }
}

/// Recursively walk an expression tree, calling `f(name, cm)` for every
/// `name: <length>` named-arg pair found. Uses the raw syntax node tree
/// because typst_syntax 0.15's `Expr` enum doesn't expose a `Named` variant
/// directly — `Named` is a separate AST node reachable via `cast()`.
fn collect_named_lengths(node: &LinkedNode, f: &mut impl FnMut(&str, f64)) {
    if let Some(named) = node.get().cast::<ast::Named>() {
        let name = named.name().as_str();
        let expr = named.expr();
        if let Some(cm) = expr_length_cm(&expr) {
            f(name, cm);
        }
    }
    for child in node.children() {
        collect_named_lengths(&child, f);
    }
}

/// Try to evaluate an expression as a length in cm. Handles `4cm`, `3in`,
/// `5pt`, bare numbers (treated as cm).
fn expr_length_cm(e: &Expr) -> Option<f64> {
    match e {
        Expr::Numeric(n) => {
            let (val, unit) = n.get();
            match unit {
                typst_syntax::ast::Unit::Cm => Some(val),
                typst_syntax::ast::Unit::Mm => Some(val * 0.1),
                typst_syntax::ast::Unit::Pt => Some(val / PT_PER_CM),
                typst_syntax::ast::Unit::In => Some(val * 2.54),
                _ => None,
            }
        }
        Expr::Int(i) => Some(i.get() as f64),
        Expr::Float(fl) => Some(fl.get()),
        _ => None,
    }
}

/// Accumulated parse state.
#[derive(Default)]
struct ParseCtx {
    /// local name -> original Candy symbol (resolved through imports).
    symbol_map: HashMap<String, String>,
    /// label -> raw body source text.
    items: HashMap<Label, String>,
    /// label -> frame-0 visual state.
    initial: HashMap<Label, FrameData>,
    slides: Vec<Slide>,
    audio: Vec<AudioTrack>,
    cursor: u32,
    block_counter: usize,
    /// Page size in cm, detected from `#set page(width:.., height:..)`.
    page_size_cm: Option<(f64, f64)>,
    /// Top-level `@preview`/package import lines (raw source) to re-inject into
    /// per-object compile snippets so mobject bodies can use external packages.
    imports: Vec<String>,
}

/// Recursively walk the syntax tree.
fn walk(node: &LinkedNode, raw: &str, ctx: &mut ParseCtx) {
    // Detect `#set page(width: X, height: Y)` to extract the page size.
    if let Some(set_rule) = node.get().cast::<ast::SetRule>() {
        let target = set_rule.target();
        if matches!(target, Expr::Ident(ref id) if id.as_str() == "page") {
            extract_page_size(node, ctx);
        }
    }
    if let Some(imp) = node.get().cast::<ast::ModuleImport>() {
        // Capture package imports (paths starting with '@') so they can be
        // re-injected into candy's per-object compile snippets (which are
        // detached Typst modules and would otherwise lose the binding). Local
        // relative imports are skipped — they cannot resolve in a detached module.
        if let Some(src) = module_import_path(&imp) {
            if src.starts_with('@') {
                // The ModuleImport AST node's range excludes the leading `#`
                // escape, so re-add it so the injected line is valid Typst.
                let text = format!("#{}", raw[node.range()].trim());
                if !ctx.imports.contains(&text) {
                    ctx.imports.push(text);
                }
            }
        }
        process_import(imp, ctx);
    } else if let Some(call) = node.get().cast::<ast::FuncCall>() {
        process_call(call, node, raw, ctx);
    }
    for child in node.children() {
        walk(&child, raw, ctx);
    }
}

/// Extract the package/path string from a `#import "..."` statement.
fn module_import_path(imp: &ast::ModuleImport) -> Option<String> {
    match imp.source() {
        Expr::Str(s) => Some(s.get().to_string()),
        _ => None,
    }
}

/// Record imported Candy symbols so later calls can be resolved.
fn process_import(imp: ast::ModuleImport, ctx: &mut ParseCtx) {
    match imp.imports() {
        Some(ast::Imports::Wildcard) => {
            for c in CANDY {
                ctx.symbol_map
                    .entry((*c).to_string())
                    .or_insert_with(|| (*c).to_string());
            }
        }
        Some(ast::Imports::Items(items)) => {
            for it in items.iter() {
                let orig = it.original_name().as_str().to_string();
                let bound = it.bound_name().as_str().to_string();
                ctx.symbol_map.insert(bound, orig);
            }
        }
        None => {}
    }
}

/// Resolve and dispatch a single Candy function call.
fn process_call(call: ast::FuncCall, node: &LinkedNode, raw: &str, ctx: &mut ParseCtx) {
    let callee = call.callee();
    let sym: Option<String> = match callee {
        Expr::Ident(id) => {
            let name = id.as_str();
            ctx.symbol_map
                .get(name)
                .filter(|o| CANDY.contains(&o.as_str()))
                .cloned()
        }
        // Handles `candy.mobject(...)` style calls regardless of module name.
        Expr::FieldAccess(fa) => {
            let field = fa.field().as_str();
            if CANDY.contains(&field) {
                Some(field.to_string())
            } else {
                None
            }
        }
        _ => None,
    };
    let Some(sym) = sym else { return };

    let args = call.args();
    let mut pos: Vec<Expr> = Vec::new();
    let mut named: HashMap<String, Expr> = HashMap::new();
    for a in args.items() {
        match a {
            ast::Arg::Pos(e) => pos.push(e),
            ast::Arg::Named(n) => {
                named.insert(n.name().as_str().to_string(), n.expr());
            }
            ast::Arg::Spread(_) => {}
        }
    }

    match sym.as_str() {
        "mobject" => process_mobject(&pos, &named, node, raw, ctx),
        "animate" => process_animate(&pos, &named, node, raw, ctx),
        "pause" => process_pause(&named, ctx),
        "audio" => process_audio(&pos, &named, node, raw, ctx),
        "play" => process_play(&pos, &named, node, raw, ctx),
        // Manim-inspired directives.
        "save_state" => process_save_state(&pos, &named, ctx),
        "restore" => process_restore(&pos, &named, ctx),
        "indicate" => process_indicate(&pos, &named, ctx),
        "flash" => process_flash(&pos, &named, ctx),
        "wiggle" => process_wiggle(&pos, &named, ctx),
        "appear" => process_appear_disappear(&pos, true, ctx),
        "disappear" => process_appear_disappear(&pos, false, ctx),
        "set_color" => process_set_color(&pos, &named, ctx),
        // Manim-inspired composite animations.
        "blink" => process_blink(&pos, &named, ctx),
        "spiral_in" => process_spiral_in(&pos, &named, ctx),
        "focus_on" => process_focus_on(&pos, &named, ctx),
        "fade_transform" => process_fade_transform(&pos, &named, ctx),
        "move_along_path" => process_move_along_path(&pos, &named, node, raw, ctx),
        "morph" => process_morph(&pos, &named, ctx),
        _ => {}
    }
}

/// `mobject(label, body)`: register `items[label] = body` (raw source) with a
/// default frame-0 state (opacity 1). Position is left to the renderer.
fn process_mobject(
    pos: &[Expr],
    named: &HashMap<String, Expr>,
    node: &LinkedNode,
    raw: &str,
    ctx: &mut ParseCtx,
) {
    let label_expr = pos
        .first()
        .or_else(|| named.get("label"))
        .and_then(|e| match e {
            Expr::Str(s) => Some(s.get().to_string()),
            _ => None,
        });
    let Some(label_str) = label_expr else { return };
    let body_expr = pos.get(1).or_else(|| named.get("body"));
    let Some(body_expr) = body_expr else { return };
    let body = expr_src(raw, node, body_expr).to_string();

    let label = Label(label_str);
    ctx.items.insert(label.clone(), body);
    ctx.initial.insert(
        label.clone(),
        FrameData {
            time_ms: 0,
            target: label.clone(),
            x: 0.0,
            y: 0.0,
            scale: 1.0,
            opacity: 1.0,
            rotation: 0.0,
            easing: Easing::Linear,
        },
    );
}

/// `animate(target, to:, scale:, opacity:, duration:, easing:)`.
///
/// The `easing` named argument accepts a string (`"linear"`, `"smooth"`,
/// `"ease-in-out"`, …) and falls back to `Easing::Linear` if missing or
/// unrecognized. Unrecognized names emit a warning to stderr and continue.
fn process_animate(
    pos: &[Expr],
    named: &HashMap<String, Expr>,
    node: &LinkedNode,
    raw: &str,
    ctx: &mut ParseCtx,
) {
    let target_expr = pos.first().or_else(|| named.get("target"));
    let Some(target_expr) = target_expr else { return };
    let label = match target_expr {
        Expr::Str(s) => Label(s.get().to_string()),
        _ => return,
    };
    let duration = named
        .get("duration")
        .and_then(expr_to_f64)
        .unwrap_or(30.0)
        .max(1.0) as u32;

    let easing = match named.get("easing") {
        Some(Expr::Str(s)) => {
            let name = s.get();
            match Easing::from_str(name.as_str()) {
                Some(e) => e,
                None => {
                    eprintln!(
                        "warn: unknown easing '{name}' for @{}, falling back to linear",
                        label.0
                    );
                    Easing::Linear
                }
            }
        }
        // Missing or non-string easing → linear (candy v0.1 behavior).
        _ => Easing::Linear,
    };

    let mut actions = Vec::new();
    // Absolute move: `to: (x, y)`.
    if let Some(to_e) = named.get("to") {
        if let Some((x, y)) = tuple_cm(to_e, raw, node) {
            actions.push(Action::MoveTo {
                target: label.clone(),
                to: (x, y),
                easing,
            });
        }
    }
    // Relative move: `dx:` / `dy:` (cm). Either or both may be given.
    let dx = named.get("dx").and_then(expr_to_f64);
    let dy = named.get("dy").and_then(expr_to_f64);
    if dx.is_some() || dy.is_some() {
        actions.push(Action::MoveBy {
            target: label.clone(),
            delta: (dx.unwrap_or(0.0), dy.unwrap_or(0.0)),
            easing,
        });
    }
    // Absolute scale: `scale: 1.5`.
    if let Some(s) = named.get("scale").and_then(expr_to_f64) {
        actions.push(Action::Scale {
            target: label.clone(),
            to: s,
            easing,
        });
    }
    // Relative scale: `scale-by: 1.5` (multiply current scale).
    if let Some(f) = named.get("scale-by").and_then(expr_to_f64) {
        actions.push(Action::ScaleBy {
            target: label.clone(),
            factor: f,
            easing,
        });
    }
    // Absolute rotate: `rotate: 90`.
    if let Some(deg) = named.get("rotate").and_then(expr_to_f64) {
        actions.push(Action::Rotate {
            target: label.clone(),
            degrees: deg,
            easing,
        });
    }
    // Relative rotate: `rotate-by: 15` (add to current rotation).
    if let Some(d) = named.get("rotate-by").and_then(expr_to_f64) {
        actions.push(Action::RotateBy {
            target: label.clone(),
            delta_degrees: d,
            easing,
        });
    }
    if let Some(o) = named.get("opacity").and_then(expr_to_f64) {
        actions.push(Action::FadeTo {
            target: label.clone(),
            opacity: o.clamp(0.0, 1.0),
            easing,
        });
    }
    ctx.slides.push(Slide {
        duration_ms: duration,
        actions,
    });
    ctx.cursor += duration;
}

/// `pause(duration:)` — a no-op hold in standard Typst; a blank slide here.
fn process_pause(named: &HashMap<String, Expr>, ctx: &mut ParseCtx) {
    let duration = named
        .get("duration")
        .and_then(expr_to_f64)
        .unwrap_or(500.0)
        .max(1.0) as u32;
    ctx.slides.push(Slide {
        duration_ms: duration,
        actions: Vec::new(),
    });
    ctx.cursor += duration;
}

/// `audio(path, blocking:, loop:, volume:, slice:)`.
fn process_audio(
    pos: &[Expr],
    named: &HashMap<String, Expr>,
    node: &LinkedNode,
    raw: &str,
    ctx: &mut ParseCtx,
) {
    let path = match pos.first() {
        Some(Expr::Str(s)) => s.get().to_string(),
        _ => return,
    };
    let blocking = named
        .get("blocking")
        .and_then(expr_to_bool)
        .unwrap_or(false);
    let loop_track = named.get("loop").and_then(expr_to_bool).unwrap_or(false);
    let volume = named.get("volume").and_then(expr_to_f64).unwrap_or(1.0);
    let slice = named.get("slice").and_then(|e| tuple_cm(e, raw, node));
    ctx.audio.push(AudioTrack {
        path,
        start_ms: ctx.cursor,
        blocking,
        loop_track,
        volume,
        slice,
    });
}

/// `play(body, duration:)` — a block-level animation unit, hidden until its
/// slide fades it in.
fn process_play(
    pos: &[Expr],
    named: &HashMap<String, Expr>,
    node: &LinkedNode,
    raw: &str,
    ctx: &mut ParseCtx,
) {
    let body_expr = pos.first().or_else(|| named.get("body"));
    let Some(body_expr) = body_expr else { return };
    let body = expr_src(raw, node, body_expr).to_string();
    let duration = named
        .get("duration")
        .and_then(expr_to_f64)
        .unwrap_or(30.0)
        .max(1.0) as u32;

    let label = Label(format!("__block_{}", ctx.block_counter));
    ctx.block_counter += 1;
    ctx.items.insert(label.clone(), body);
    ctx.initial.insert(
        label.clone(),
        FrameData {
            time_ms: 0,
            target: label.clone(),
            x: 0.0,
            y: 0.0,
            scale: 1.0,
            opacity: 0.0,
            rotation: 0.0,
            easing: Easing::Linear,
        },
    );
    ctx.slides.push(Slide {
        duration_ms: duration,
        actions: vec![Action::FadeIn {
            target: label.clone(),
            easing: Easing::Linear,
        }],
    });
    ctx.cursor += duration;
}

// ---- Manim-inspired directive parsers ----

/// Resolve the easing named arg, falling back to Linear with a warning.
fn resolve_easing(named: &HashMap<String, Expr>, label: &Label) -> Easing {
    match named.get("easing") {
        Some(Expr::Str(s)) => {
            let name = s.get();
            match Easing::from_str(name.as_str()) {
                Some(e) => e,
                None => {
                    eprintln!(
                        "warn: unknown easing '{name}' for @{}, falling back to linear",
                        label.0
                    );
                    Easing::Linear
                }
            }
        }
        _ => Easing::Linear,
    }
}

/// Extract the target label (positional string arg or `target:` named arg).
fn target_arg(pos: &[Expr], named: &HashMap<String, Expr>) -> Option<Label> {
    let e = pos.first().or_else(|| named.get("target"))?;
    match e {
        Expr::Str(s) => Some(Label(s.get().to_string())),
        _ => None,
    }
}

/// `save_state(target, slot: "name")` — snapshot the target's current state.
/// Inert under standard Typst. Produces no slide (0-duration); the action is
/// attached to a 1-frame slide at the current cursor so the scheduler sees it.
fn process_save_state(pos: &[Expr], named: &HashMap<String, Expr>, ctx: &mut ParseCtx) {
    let Some(label) = target_arg(pos, named) else { return };
    let slot = named
        .get("slot")
        .and_then(|e| match e {
            Expr::Str(s) => Some(s.get().to_string()),
            _ => None,
        })
        .unwrap_or_else(|| "default".to_string());
    // SaveState is instantaneous — emit a 1-frame slide so the scheduler
    // processes the action at the current cursor position.
    ctx.slides.push(Slide {
        duration_ms: 1,
        actions: vec![Action::SaveState { target: label, slot }],
    });
    ctx.cursor += 1;
}

/// `restore(target, slot: "name", duration: 30, easing: "linear")` —
/// interpolate back to a previously saved state.
fn process_restore(pos: &[Expr], named: &HashMap<String, Expr>, ctx: &mut ParseCtx) {
    let Some(label) = target_arg(pos, named) else { return };
    let slot = named
        .get("slot")
        .and_then(|e| match e {
            Expr::Str(s) => Some(s.get().to_string()),
            _ => None,
        })
        .unwrap_or_else(|| "default".to_string());
    let duration = named
        .get("duration")
        .and_then(expr_to_f64)
        .unwrap_or(30.0)
        .max(1.0) as u32;
    let easing = resolve_easing(named, &label);
    ctx.slides.push(Slide {
        duration_ms: duration,
        actions: vec![Action::Restore { target: label, slot, easing }],
    });
    ctx.cursor += duration;
}

/// `indicate(target, factor: 1.1, dx: 0, dy: 0, duration: 24, easing: "smooth")`
/// — briefly scale + shift, then return to original.
fn process_indicate(pos: &[Expr], named: &HashMap<String, Expr>, ctx: &mut ParseCtx) {
    let Some(label) = target_arg(pos, named) else { return };
    let duration = named.get("duration").and_then(expr_to_f64).unwrap_or(800.0).max(1.0) as u32;
    let factor = named.get("factor").and_then(expr_to_f64).unwrap_or(1.1);
    let dx = named.get("dx").and_then(expr_to_f64).unwrap_or(0.0);
    let dy = named.get("dy").and_then(expr_to_f64).unwrap_or(0.0);
    let easing = resolve_easing(named, &label);
    ctx.slides.push(Slide {
        duration_ms: duration,
        actions: vec![Action::Indicate { target: label, factor, dx, dy, easing }],
    });
    ctx.cursor += duration;
}

/// `flash(target, factor: 2.0, duration: 18, easing: "smooth")` —
/// briefly enlarge + fade, then return to original.
fn process_flash(pos: &[Expr], named: &HashMap<String, Expr>, ctx: &mut ParseCtx) {
    let Some(label) = target_arg(pos, named) else { return };
    let duration = named.get("duration").and_then(expr_to_f64).unwrap_or(600.0).max(1.0) as u32;
    let factor = named.get("factor").and_then(expr_to_f64).unwrap_or(2.0);
    let easing = resolve_easing(named, &label);
    ctx.slides.push(Slide {
        duration_ms: duration,
        actions: vec![Action::Flash { target: label, factor, easing }],
    });
    ctx.cursor += duration;
}

/// `wiggle(target, degrees: 15, duration: 20, easing: "wiggle")` —
/// oscillate rotation, then return to original.
fn process_wiggle(pos: &[Expr], named: &HashMap<String, Expr>, ctx: &mut ParseCtx) {
    let Some(label) = target_arg(pos, named) else { return };
    let duration = named.get("duration").and_then(expr_to_f64).unwrap_or(667.0).max(1.0) as u32;
    let degrees = named.get("degrees").and_then(expr_to_f64).unwrap_or(500.0);
    let easing = resolve_easing(named, &label);
    ctx.slides.push(Slide {
        duration_ms: duration,
        actions: vec![Action::Wiggle { target: label, degrees, easing }],
    });
    ctx.cursor += duration;
}

/// `appear(target)` / `disappear(target)` — instantaneous visibility toggle.
/// Emits a 1-frame slide. (`show`/`hide` would conflict with Typst keywords.)
fn process_appear_disappear(pos: &[Expr], appear: bool, ctx: &mut ParseCtx) {
    let Some(label) = target_arg(pos, &HashMap::new()) else { return };
    let action = if appear {
        Action::Show { target: label }
    } else {
        Action::Hide { target: label }
    };
    ctx.slides.push(Slide {
        duration_ms: 1,
        actions: vec![action],
    });
    ctx.cursor += 1;
}

/// `set_color(target, color: "red", duration: 1, easing: "linear")` —
/// record a color change (tracked, renderer no-op for now).
fn process_set_color(pos: &[Expr], named: &HashMap<String, Expr>, ctx: &mut ParseCtx) {
    let Some(label) = target_arg(pos, named) else { return };
    let color = named
        .get("color")
        .and_then(|e| match e {
            Expr::Str(s) => Some(s.get().to_string()),
            _ => None,
        })
        .unwrap_or_else(|| "black".to_string());
    let duration = named.get("duration").and_then(expr_to_f64).unwrap_or(33.0).max(1.0) as u32;
    let easing = resolve_easing(named, &label);
    ctx.slides.push(Slide {
        duration_ms: duration,
        actions: vec![Action::SetColor { target: label, color, easing }],
    });
    ctx.cursor += duration;
}

// ---- Manim-inspired composite animation parsers ----

/// `blink(target, blinks: 3, duration: 30, easing: "linear")` — alternate
/// opacity 1↔0 N times. Mirrors Manim's `Blink`.
fn process_blink(pos: &[Expr], named: &HashMap<String, Expr>, ctx: &mut ParseCtx) {
    let Some(label) = target_arg(pos, named) else { return };
    let blinks = named.get("blinks").and_then(expr_to_f64).unwrap_or(3.0).max(1.0) as u32;
    let duration = named.get("duration").and_then(expr_to_f64).unwrap_or(1000.0).max(1.0) as u32;
    let per_blink = (duration / (blinks * 2)).max(1);
    let easing = resolve_easing(named, &label);
    // Each blink = FadeTo(0) + FadeTo(1).
    for _ in 0..blinks {
        ctx.slides.push(Slide {
            duration_ms: per_blink,
            actions: vec![Action::FadeTo {
                target: label.clone(),
                opacity: 0.0,
                easing,
            }],
        });
        ctx.slides.push(Slide {
            duration_ms: per_blink,
            actions: vec![Action::FadeTo {
                target: label.clone(),
                opacity: 1.0,
                easing,
            }],
        });
    }
    ctx.cursor += per_blink * blinks * 2;
}

/// `spiral_in(target, scale: 3.0, rotate: 360, duration: 24, easing: "smooth")`
/// — fly in from a scaled-up, rotated state to the natural position, fading in.
/// Mirrors Manim's `SpiralIn`.
fn process_spiral_in(pos: &[Expr], named: &HashMap<String, Expr>, ctx: &mut ParseCtx) {
    let Some(label) = target_arg(pos, named) else { return };
    let scale = named.get("scale").and_then(expr_to_f64).unwrap_or(3.0);
    let rotate = named.get("rotate").and_then(expr_to_f64).unwrap_or(360.0);
    let duration = named.get("duration").and_then(expr_to_f64).unwrap_or(800.0).max(1.0) as u32;
    let easing = resolve_easing(named, &label);
    // Set initial state: scaled up, rotated, invisible.
    ctx.slides.push(Slide {
        duration_ms: 1,
        actions: vec![
            Action::ScaleBy { target: label.clone(), factor: scale, easing },
            Action::RotateBy { target: label.clone(), delta_degrees: rotate, easing },
            Action::Hide { target: label.clone() },
        ],
    });
    // Animate to natural state: scale 1, rotate 0, visible.
    ctx.slides.push(Slide {
        duration_ms: duration,
        actions: vec![
            Action::Scale { target: label.clone(), to: 1.0, easing },
            Action::Rotate { target: label.clone(), degrees: 0.0, easing },
            Action::FadeIn { target: label, easing },
        ],
    });
    ctx.cursor += 1 + duration;
}

/// `focus_on(target, factor: 0.5, duration: 20, easing: "smooth")` —
/// shrink a "spotlight" onto the target. Implemented as a scale-down + fade
/// on the target. Mirrors Manim's `FocusOn`.
fn process_focus_on(pos: &[Expr], named: &HashMap<String, Expr>, ctx: &mut ParseCtx) {
    let Some(label) = target_arg(pos, named) else { return };
    let factor = named.get("factor").and_then(expr_to_f64).unwrap_or(0.5);
    let duration = named.get("duration").and_then(expr_to_f64).unwrap_or(667.0).max(1.0) as u32;
    let easing = resolve_easing(named, &label);
    ctx.slides.push(Slide {
        duration_ms: duration,
        actions: vec![
            Action::ScaleBy { target: label.clone(), factor, easing },
            Action::FadeTo { target: label, opacity: 0.3, easing },
        ],
    });
    ctx.cursor += duration;
}

/// `fade_transform(from: "old", to: "new", duration: 20, easing: "smooth")`
/// — crossfade two mobjects: fade out `from` while fading in `to`. Both
/// must be registered via `mobject`. Mirrors Manim's `FadeTransform` (simple
/// crossfade variant).
fn process_fade_transform(_pos: &[Expr], named: &HashMap<String, Expr>, ctx: &mut ParseCtx) {
    let from = named
        .get("from")
        .and_then(|e| match e {
            Expr::Str(s) => Some(Label(s.get().to_string())),
            _ => None,
        });
    let to = named
        .get("to")
        .and_then(|e| match e {
            Expr::Str(s) => Some(Label(s.get().to_string())),
            _ => None,
        });
    let (Some(from), Some(to)) = (from, to) else { return };
    let duration = named.get("duration").and_then(expr_to_f64).unwrap_or(667.0).max(1.0) as u32;
    let easing = resolve_easing(named, &from);
    // Fade out `from` and fade in `to` in the same slide (parallel).
    ctx.slides.push(Slide {
        duration_ms: duration,
        actions: vec![
            Action::FadeOut { target: from, easing },
            Action::FadeIn { target: to, easing },
        ],
    });
    ctx.cursor += duration;
}

/// `move_along_path(target, path: ((x1,y1), (x2,y2), ...), duration: 30, easing: "linear")`
/// — move the target along a polyline through the given points (cm, absolute).
/// Mirrors Manim's `MoveAlongPath`.
fn process_move_along_path(
    pos: &[Expr],
    named: &HashMap<String, Expr>,
    node: &LinkedNode,
    raw: &str,
    ctx: &mut ParseCtx,
) {
    let Some(label) = target_arg(pos, named) else { return };
    let duration = named.get("duration").and_then(expr_to_f64).unwrap_or(1000.0).max(1.0) as u32;
    let easing = resolve_easing(named, &label);

    // Parse the `path` named arg as an array of (x, y) tuples.
    let points: Vec<(f64, f64)> = match named.get("path") {
        Some(Expr::Array(arr)) => {
            arr.items()
                .filter_map(|item| match item {
                    ast::ArrayItem::Pos(e) => tuple_cm(&e, raw, node),
                    ast::ArrayItem::Spread(_) => None,
                })
                .collect()
        }
        _ => Vec::new(),
    };
    if points.is_empty() {
        return;
    }
    ctx.slides.push(Slide {
        duration_ms: duration,
        actions: vec![Action::MoveAlongPath {
            target: label,
            points,
            easing,
        }],
    });
    ctx.cursor += duration;
}

/// `morph(from, to, duration: 24, easing: "smooth")` — crossfade + scale
/// transform from one mobject to another. The `from` object shrinks and fades
/// out while the `to` object grows and fades in. Both must be registered via
/// `mobject`. A simplified approximation of Manim's `Transform`.
fn process_morph(pos: &[Expr], named: &HashMap<String, Expr>, ctx: &mut ParseCtx) {
    let from = pos.first().and_then(|e| match e {
        Expr::Str(s) => Some(Label(s.get().to_string())),
        _ => None,
    });
    let to = pos.get(1).and_then(|e| match e {
        Expr::Str(s) => Some(Label(s.get().to_string())),
        _ => None,
    });
    let (Some(from), Some(to)) = (from, to) else { return };
    let duration = named.get("duration").and_then(expr_to_f64).unwrap_or(800.0).max(1.0) as u32;
    let easing = resolve_easing(named, &from);

    // Hide the `to` object initially (it will fade in).
    ctx.slides.push(Slide {
        duration_ms: 1,
        actions: vec![Action::Hide { target: to.clone() }],
    });

    // Morph: `from` shrinks + fades out, `to` grows + fades in (parallel).
    ctx.slides.push(Slide {
        duration_ms: duration,
        actions: vec![
            Action::ScaleBy { target: from.clone(), factor: 0.01, easing },
            Action::FadeOut { target: from, easing },
            Action::ScaleBy { target: to.clone(), factor: 100.0, easing },
            Action::FadeIn { target: to, easing },
        ],
    });
    ctx.cursor += 1 + duration;
}

/// Recover the source byte range of `target` by identity (pointer) within the
/// `LinkedNode` subtree. This works even for *detached* sources, where Typst
/// assigns no resolvable span numbers and `LinkedNode::find` would fail.
fn range_of(node: &LinkedNode, target: &SyntaxNode) -> Option<Range<usize>> {
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
fn expr_src<'a>(raw: &'a str, node: &LinkedNode, e: &Expr) -> &'a str {
    match range_of(node, e.to_untyped()) {
        Some(r) => &raw[r],
        None => "",
    }
}

/// Evaluate a unitless numeric expression to `f64`.
fn expr_to_f64(e: &Expr) -> Option<f64> {
    match e {
        Expr::Int(i) => Some(i.get() as f64),
        Expr::Float(f) => Some(f.get()),
        Expr::Numeric(n) => Some(n.get().0),
        _ => None,
    }
}

/// Evaluate a boolean expression.
fn expr_to_bool(e: &Expr) -> Option<bool> {
    match e {
        Expr::Bool(b) => Some(b.get()),
        _ => None,
    }
}

/// Evaluate a `(x, y)` length tuple to centimeters.
fn tuple_cm(e: &Expr, _raw: &str, _node: &LinkedNode) -> Option<(f64, f64)> {
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


#[cfg(test)]
mod tests {
    use super::*;

    const DOT: &str = r#"
#import "candy": *
#mobject("dot", circle(radius: 1cm, fill: blue))
#mobject("dot2", rect(width: 1cm, height: 1cm))
#animate("dot", to: (4cm, 0pt), duration: 30, easing: "linear")
#animate("dot2", scale: 1.5, duration: 20)
#pause(duration: 15)
#audio("voice.opus", blocking: false, loop: false, volume: 0.9, slice: none)
"#;

    #[test]
    fn parses_dot_ast() {
        let tmp = std::env::temp_dir().join("candy_test_dot.tyx");
        std::fs::write(&tmp, DOT).unwrap();
        let scene = parse_tyx(&tmp).unwrap();
        assert_eq!(scene.slides.len(), 3); // 2 animate + pause
        // play not used here; but dot + dot2 registered
        assert!(scene.items.contains_key(&Label("dot".into())));
        assert!(scene.items.contains_key(&Label("dot2".into())));
        // body captured as raw source, not a string
        assert_eq!(scene.items[&Label("dot".into())], "circle(radius: 1cm, fill: blue)");
        assert_eq!(scene.slides[0].duration_ms, 30);
        assert_eq!(scene.slides[2].duration_ms, 15);
        assert_eq!(scene.audio.len(), 1);
        assert_eq!(scene.audio[0].path, "voice.opus");
        assert_eq!(scene.audio[0].start_ms, 65); // 30 + 20 + 15 (pause)
        std::fs::remove_file(&tmp).ok();
    }

    #[test]
    fn parses_field_access_import() {
        // candy imported as a module, called via candy.mobject(...)
        let src = r#"
#import "candy"
#candy.mobject("box", rect(width: 2cm, height: 2cm, fill: red))
#candy.animate("box", to: (3cm, 2cm), duration: 20)
"#;
        let tmp = std::env::temp_dir().join("candy_test_field.tyx");
        std::fs::write(&tmp, src).unwrap();
        let scene = parse_tyx(&tmp).unwrap();
        assert!(scene.items.contains_key(&Label("box".into())));
        assert_eq!(
            scene.items[&Label("box".into())],
            "rect(width: 2cm, height: 2cm, fill: red)"
        );
        assert_eq!(scene.slides.len(), 1);
        std::fs::remove_file(&tmp).ok();
    }

    #[test]
    fn parses_box() {
        let src = r#"
#import "candy": animate as anim, mobject as mob
#mob("box", rect(width: 2cm, height: 2cm, fill: red))
#anim("box", to: (3cm, 2cm), duration: 20)
#anim("box", scale: 1.5, duration: 20)
"#;
        let tmp = std::env::temp_dir().join("candy_test_box.tyx");
        std::fs::write(&tmp, src).unwrap();
        let scene = parse_tyx(&tmp).unwrap();
        assert!(scene.items.contains_key(&Label("box".into())));
        assert_eq!(scene.slides[0].actions.len(), 1); // move
        assert_eq!(scene.slides[1].actions.len(), 1); // scale
        std::fs::remove_file(&tmp).ok();
    }

    #[test]
    fn parses_play_block() {
        let src = r#"
#import "candy": *
#mobject("a", circle(radius: 1cm))
#play(rect(width: 2cm, height: 1cm, fill: green), duration: 25)
"#;
        let tmp = std::env::temp_dir().join("candy_test_play.tyx");
        std::fs::write(&tmp, src).unwrap();
        let scene = parse_tyx(&tmp).unwrap();
        // one synthetic block label
        let blocks: usize = scene
            .items
            .keys()
            .filter(|l| l.0.starts_with("__block_"))
            .count();
        assert_eq!(blocks, 1);
        assert_eq!(scene.slides.len(), 1);
        assert_eq!(scene.slides[0].duration_ms, 25);
        std::fs::remove_file(&tmp).ok();
    }

    /// Confirm the shipped `lib.typ` is valid standard Typst: inlining it and
    /// calling every directive must compile with the `typst` compiler.
    #[test]
    fn std_typst_api_compiles() {
        let lib = std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../typst/src/lib.typ"),
        )
        .expect("lib.typ should exist");
        let src = format!(
            "{lib}\n\
             #mobject(\"dot\", circle(radius: 1cm, fill: blue))\n\
             #mobject(\"box\", rect(width: 2cm, height: 2cm, fill: red))\n\
             #animate(\"dot\", to: (4cm, 0pt), duration: 30)\n\
             #animate(\"box\", rotate: 45, opacity: 0.5, easing: \"smooth\", duration: 20)\n\
             #pause(duration: 15)\n\
             #audio(\"voice.opus\", blocking: false, loop: false, volume: 0.9)\n\
             #play(circle(radius: 1cm), duration: 10)\n"
        );
        let out = crate::renderer::compile_svg_for_test(&src);
        assert!(out.is_ok(), "std Typst failed to compile: {out:?}");
    }

    /// Verify the new `rotate` and `opacity` (FadeTo) actions parse correctly.
    #[test]
    fn parses_rotate_and_fadeto() {
        let src = r#"
#import "candy": *
#mobject("sq", rect(width: 2cm, height: 2cm))
#animate("sq", rotate: 90, opacity: 0.3, duration: 25, easing: "cubic-in-out")
"#;
        let tmp = std::env::temp_dir().join("candy_test_rotate.tyx");
        std::fs::write(&tmp, src).unwrap();
        let scene = parse_tyx(&tmp).unwrap();
        assert_eq!(scene.slides.len(), 1);
        let actions = &scene.slides[0].actions;
        // rotate + opacity → 2 actions
        assert_eq!(actions.len(), 2);
        let has_rotate = actions.iter().any(|a| matches!(a, Action::Rotate { degrees: 90.0, .. }));
        let has_fadeto = actions
            .iter()
            .any(|a| matches!(a, Action::FadeTo { opacity: 0.3, .. }));
        assert!(has_rotate, "expected Rotate(90) action, got {actions:?}");
        assert!(has_fadeto, "expected FadeTo(0.3) action, got {actions:?}");
        // Easing must propagate to both actions.
        for a in actions {
            assert_eq!(a.easing(), crate::core::easing::Easing::CubicInOut);
        }
        std::fs::remove_file(&tmp).ok();
    }

    /// Verify the Manim-inspired directives parse to the correct Action variants.
    #[test]
    fn parses_manim_directives() {
        let src = r#"
#import "candy": *
#mobject("dot", circle(radius: 1cm))
#save_state("dot", slot: "home")
#animate("dot", to: (4cm, 0pt), duration: 20)
#restore("dot", slot: "home", duration: 20, easing: "smooth")
#indicate("dot", factor: 1.2, duration: 18)
#flash("dot", factor: 1.8, duration: 12)
#wiggle("dot", degrees: 12, duration: 16)
#disappear("dot")
#appear("dot")
#set_color("dot", color: "red", duration: 1)
"#;
        let tmp = std::env::temp_dir().join("candy_test_manim.tyx");
        std::fs::write(&tmp, src).unwrap();
        let scene = parse_tyx(&tmp).unwrap();
        // 1 mobject (no slide) + 1 save_state(1f) + 1 animate(20f) + 1 restore(20f)
        // + 1 indicate(18f) + 1 flash(12f) + 1 wiggle(16f) + 1 hide(1f) + 1 show(1f)
        // + 1 set_color(1f) = 9 slides
        assert_eq!(scene.slides.len(), 9, "slides: {:?}", scene.slides);

        // Verify each action variant.
        assert!(matches!(scene.slides[0].actions[0], Action::SaveState { ref slot, .. } if slot == "home"));
        assert!(matches!(scene.slides[1].actions[0], Action::MoveTo { .. }));
        assert!(matches!(scene.slides[2].actions[0], Action::Restore { ref slot, .. } if slot == "home"));
        assert!(matches!(scene.slides[3].actions[0], Action::Indicate { factor: 1.2, .. }));
        assert!(matches!(scene.slides[4].actions[0], Action::Flash { factor: 1.8, .. }));
        assert!(matches!(scene.slides[5].actions[0], Action::Wiggle { degrees: 12.0, .. }));
        assert!(matches!(scene.slides[6].actions[0], Action::Hide { .. }));
        assert!(matches!(scene.slides[7].actions[0], Action::Show { .. }));
        assert!(matches!(scene.slides[8].actions[0], Action::SetColor { ref color, .. } if color == "red"));
        std::fs::remove_file(&tmp).ok();
    }

    /// Verify the new directives compile as valid standard Typst (lib.typ
    /// defines them all as no-ops).
    #[test]
    fn std_typst_manim_api_compiles() {
        let lib = std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../typst/src/lib.typ"),
        )
        .expect("lib.typ should exist");
        let src = format!(
            "{lib}\n\
             #mobject(\"dot\", circle(radius: 1cm))\n\
             #save_state(\"dot\", slot: \"home\")\n\
             #restore(\"dot\", slot: \"home\", duration: 10, easing: \"smooth\")\n\
             #indicate(\"dot\", factor: 1.2, duration: 12)\n\
             #flash(\"dot\", factor: 2.0, duration: 10)\n\
             #wiggle(\"dot\", degrees: 10, duration: 14)\n\
             #disappear(\"dot\")\n\
             #appear(\"dot\")\n\
             #set_color(\"dot\", color: \"red\", duration: 1)\n"
        );
        let out = crate::renderer::compile_svg_for_test(&src);
        assert!(out.is_ok(), "std Typst failed to compile manim API: {out:?}");
    }
}

