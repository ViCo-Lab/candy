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

use typst_syntax::LinkedNode;
use typst_syntax::SyntaxNode;
use typst_syntax::ast::{self, AstNode, Expr};
use typst_syntax::parse;

use crate::core::ast::{
    Action, AudioTrack, CounterDef, CounterEvent, CounterEventKind, FrameData, Label, PathMode,
    Scene, SceneInfo, Slide, SubPos, Subtitle, TrackKey,
};
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
    // The whole document is the implicit root scope (id 0).
    ctx.scope_stack.push(0);
    ctx.scope_starts.insert(0, 0);
    ctx.next_scope_id = 1;
    // The whole document is also the implicit root *scene* (id 0). Every
    // mobject / action not declared inside an explicit `#scene(...)` belongs
    // to it. This is the "no root scene → whole document is one scene" rule.
    ctx.scenes.push(crate::core::ast::SceneInfo {
        id: 0,
        parent: None,
        scope: 0,
        page_size: ctx
            .page_size_cm
            .map(|(w, h)| (w * PT_PER_CM, h * PT_PER_CM)),
        start_ms: 0,
        end_ms: 0,
        owns_labels: Vec::new(),
    });
    ctx.current_scene = 0;
    ctx.next_scene_id = 1;
    walk(&node, &raw, &mut ctx);
    // Finalize the root scope's interval [0, cursor].
    ctx.scopes.push(crate::core::ast::ScopeInfo {
        id: 0,
        parent: None,
        start_ms: 0,
        end_ms: ctx.cursor,
    });
    // Finalize the root scene's interval and attribute every mobject to the
    // scene that owns it (defaulting to the root).
    if let Some(root) = ctx.scenes.iter_mut().find(|s| s.id == 0) {
        root.end_ms = ctx.cursor;
    }
    for (label, sid) in &ctx.label_scene {
        if let Some(s) = ctx.scenes.iter_mut().find(|s| s.id == *sid) {
            s.owns_labels.push(label.clone());
        }
    }

    let private = PrivateMeta::default();
    let scene = Scene {
        slides: ctx.slides,
        items: ctx.items,
        content_timeline: ctx.content_timeline,
        morph_pairs: ctx.morph_pairs,
        initial: ctx.initial,
        audio: ctx.audio,
        imports: ctx.imports.clone(),
        page_size: ctx
            .page_size_cm
            .map(|(w, h)| (w * PT_PER_CM, h * PT_PER_CM)),
        subtitles: ctx.subtitles,
        counters: ctx.counters,
        counter_events: ctx.counter_events,
        scopes: ctx.scopes,
        scenes: ctx.scenes,
        root_scene: Some(0),
        groups: ctx.groups.clone(),
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
    collect_named_lengths(node, &mut |name, cm| match name {
        "width" => width = Some(cm),
        "height" => height = Some(cm),
        _ => {}
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
/// `5pt`, bare numbers (treated as cm), and **signed** lengths like `-4cm`
/// (which Typst parses as `Expr::Math`, not `Expr::Numeric` — without this
/// fallback a `#animate(to: (-4cm, 0cm))` would silently drop the move and the
/// object would never leave its initial position).
fn expr_length_cm(e: &Expr) -> Option<f64> {
    match e {
        Expr::Numeric(n) => {
            let (val, unit) = n.get();
            return unit_to_cm(val, unit);
        }
        Expr::Int(i) => return Some(i.get() as f64),
        Expr::Float(fl) => return Some(fl.get()),
        // Signed lengths like `-4cm` / `+4cm` parse as a *unary operation*
        // wrapping the inner `Numeric` (Typst surfaces them as `Expr::Unary`,
        // not as a signed `Numeric`). Without handling this, `#animate(to:
        // (-4cm, 0cm))` silently dropped the move and the object never left
        // its initial position.
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
fn unit_to_cm(val: f64, unit: typst_syntax::ast::Unit) -> Option<f64> {
    match unit {
        typst_syntax::ast::Unit::Cm => Some(val),
        typst_syntax::ast::Unit::Mm => Some(val * 0.1),
        typst_syntax::ast::Unit::Pt => Some(val / PT_PER_CM),
        typst_syntax::ast::Unit::In => Some(val * 2.54),
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
    /// Per-label content switches recorded by `transform` (`(time_ms, new_body)`).
    content_timeline: HashMap<Label, Vec<(u32, String)>>,
    /// Real shape-morph pairs recorded by `#morph(from, to)`.
    morph_pairs: Vec<crate::core::ast::MorphPair>,
    /// Monotonic counter for synthetic `__xf_<label>_<n>` mobjects created by
    /// `transform`, so repeated transforms on the same label don't clash.
    xf_counter: usize,
    /// Lexical Typst scope tracking. `scope_stack` is the current nesting
    /// (top = innermost scope). `next_scope_id` assigns fresh ids. `scope_starts`
    /// records each scope's start `cursor` so the interval `[start, cursor-at-exit]`
    /// can be recorded on scope exit. Scopes drive subtitle auto-destroy and
    /// counter/subtitle shadowing.
    scope_stack: Vec<usize>,
    next_scope_id: usize,
    scope_starts: HashMap<usize, u32>,
    /// Subtitle overlays (字幕模块).
    subtitles: Vec<crate::core::ast::Subtitle>,
    /// Easing counters (缓动计数器模块).
    counters: Vec<crate::core::ast::CounterDef>,
    counter_events: Vec<crate::core::ast::CounterEvent>,
    /// Lexical scope intervals (finalized on scope exit / at end of parse).
    scopes: Vec<crate::core::ast::ScopeInfo>,
    /// Nested scene tree (see `SceneInfo`). `current_scene` is the scene that
    /// owns mobjects declared right now; `scene_stack` tracks open scenes.
    scenes: Vec<crate::core::ast::SceneInfo>,
    /// Parent→child grouping links (`child → parent`), recorded by `#group`.
    groups: HashMap<Label, Label>,
    /// Next fresh scene id (root is `0`, assigned in `parse_tyx`).
    next_scene_id: usize,
    /// Open scene ids (top = innermost active scene).
    scene_stack: Vec<usize>,
    /// The scene that currently owns newly-declared mobjects.
    current_scene: usize,
    /// label -> owning scene id (populated as mobjects are declared).
    label_scene: HashMap<Label, usize>,
    /// Monotonic id for synthetic subtitles.
    subtitle_id: usize,
}

/// Recursively walk the syntax tree.
fn walk(node: &LinkedNode, raw: &str, ctx: &mut ParseCtx) {
    // Scene scoping: a `scene` call opens a *nested scene* around its body.
    // Every mobject declared inside the body belongs to this scene, and the
    // renderer shows only the innermost active scene at any frame (the parent
    // auto-hides). We open the scene, recurse into the body's children, then
    // close it — so the scene's `[start_ms, end_ms]` interval tracks exactly
    // where its content sits on the timeline.
    if let Some(call) = node.get().cast::<ast::FuncCall>() {
        if call_symbol(&call, ctx).as_deref() == Some("scene") {
            let id = ctx.next_scene_id;
            ctx.next_scene_id += 1;
            let parent = ctx.current_scene;
            let scope = ctx.next_scope_id;
            ctx.next_scope_id += 1;
            // Read the scene's own width/height (non-recursive: only the call's
            // direct named args, so a nested scene's size doesn't leak up).
            let mut w_cm: Option<f64> = None;
            let mut h_cm: Option<f64> = None;
            for a in call.args().items() {
                if let ast::Arg::Named(n) = a {
                    if let Some(cm) = expr_length_cm(&n.expr()) {
                        match n.name().as_str() {
                            "width" => w_cm = Some(cm),
                            "height" => h_cm = Some(cm),
                            _ => {}
                        }
                    }
                }
            }
            let page_size = match (w_cm, h_cm) {
                (Some(w), Some(h)) => Some((w * PT_PER_CM, h * PT_PER_CM)),
                _ => None,
            };
            let start = ctx.cursor;
            ctx.scenes.push(SceneInfo {
                id,
                parent: Some(parent),
                scope,
                page_size,
                start_ms: start,
                end_ms: start,
                owns_labels: Vec::new(),
            });
            ctx.scene_stack.push(id);
            ctx.current_scene = id;
            for child in node.children() {
                walk(&child, raw, ctx);
            }
            if let Some(s) = ctx.scenes.iter_mut().find(|s| s.id == id) {
                s.end_ms = ctx.cursor;
            }
            ctx.scene_stack.pop();
            ctx.current_scene = parent;
            return;
        }
    }

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

    // Lexical scope: a Typst code block `{ ... }` opens a child scope. We push
    // a fresh scope id (recording its start `cursor`), recurse into the block's
    // children, then pop and record the scope's `[start, cursor-at-exit]`
    // interval. This drives subtitle auto-destroy and counter/subtitle
    // shadowing. The top-level document node is not a code block — it is the
    // implicit root scope finalized in `parse_tyx`.
    let opened_scope: Option<usize> = node.get().cast::<ast::CodeBlock>().map(|_| {
        let id = ctx.next_scope_id;
        ctx.next_scope_id += 1;
        ctx.scope_starts.insert(id, ctx.cursor);
        ctx.scope_stack.push(id);
        id
    });
    for child in node.children() {
        walk(&child, raw, ctx);
    }
    if let Some(id) = opened_scope {
        let start = ctx.scope_starts.get(&id).copied().unwrap_or(0);
        let parent = ctx
            .scope_stack
            .get(ctx.scope_stack.len().saturating_sub(2))
            .copied();
        ctx.scope_stack.pop();
        ctx.scopes.push(crate::core::ast::ScopeInfo {
            id,
            parent,
            start_ms: start,
            end_ms: ctx.cursor,
        });
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
                // Canonicalize the resolved symbol to kebab-case (the `CANDY`
                // convention) and also accept the alternative naming
                // convention for the bound name, so both `save_state` and
                // `save-state` resolve to the same directive.
                let canon = orig.replace('_', "-");
                ctx.symbol_map.insert(bound.clone(), canon.clone());
                ctx.symbol_map.insert(bound.replace('_', "-"), canon);
            }
        }
        None => {}
    }
}

/// Resolve and dispatch a single Candy function call.
fn process_call(call: ast::FuncCall, node: &LinkedNode, raw: &str, ctx: &mut ParseCtx) {
    let Some(sym) = call_symbol(&call, ctx) else {
        return;
    };

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
        "save-state" => process_save_state(&pos, &named, ctx),
        "restore" => process_restore(&pos, &named, ctx),
        "indicate" => process_indicate(&pos, &named, ctx),
        "flash" => process_flash(&pos, &named, ctx),
        "wiggle" => process_wiggle(&pos, &named, ctx),
        "appear" => process_appear_disappear(&pos, true, ctx),
        "disappear" => process_appear_disappear(&pos, false, ctx),
        "set-color" => process_set_color(&pos, &named, ctx),
        // Manim-inspired composite animations.
        "blink" => process_blink(&pos, &named, ctx),
        "spiral-in" => process_spiral_in(&pos, &named, ctx),
        "focus-on" => process_focus_on(&pos, &named, ctx),
        "fade-transform" => process_fade_transform(&pos, &named, ctx),
        "move-along-path" => process_move_along_path(&pos, &named, node, raw, ctx),
        "morph" => process_morph(&pos, &named, ctx),
        "transform" => process_transform(&pos, &named, node, raw, ctx),
        // Multi-keyframe track + camera + grouping + text reveal.
        "track" => process_track(&pos, &named, ctx),
        "camera" => process_camera(&pos, &named, ctx),
        "group" => process_group(&pos, &named, ctx),
        "reveal" | "typewriter" => process_reveal(&pos, &named, sym.as_str(), ctx),
        // Subtitle + easing-counter modules.
        "subtitle" => process_subtitle(&pos, &named, node, raw, ctx),
        "ecounter" => process_ecounter(&pos, &named, ctx),
        "ecval" => { /* read; value substituted per-frame by the renderer */ }
        "counter-pause" => {
            process_counter_event(&pos, &named, ctx, crate::core::ast::CounterEventKind::Pause)
        }
        "counter-resume" => process_counter_event(
            &pos,
            &named,
            ctx,
            crate::core::ast::CounterEventKind::Resume,
        ),
        "counter-destroy" => process_counter_event(
            &pos,
            &named,
            ctx,
            crate::core::ast::CounterEventKind::Destroy,
        ),
        _ => {}
    }
}

/// The current (innermost) lexical scope id, as a string.
fn current_scope(ctx: &ParseCtx) -> String {
    ctx.scope_stack.last().copied().unwrap_or(0).to_string()
}

/// Resolve a function call to its Candy symbol (or `None` if it isn't one).
/// Works for `mobject(...)` (imported via `#import "candy": *`),
/// `candy.mobject(...)` (field access), and renamed imports.
fn call_symbol(call: &ast::FuncCall, ctx: &ParseCtx) -> Option<String> {
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
    ctx.label_scene.insert(label.clone(), ctx.current_scene);
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
    let Some(target_expr) = target_expr else {
        return;
    };
    let label = match target_expr {
        Expr::Str(s) => Label(s.get().to_string()),
        _ => return,
    };
    let duration = named
        .get("duration")
        .and_then(expr_to_f64)
        .unwrap_or(500.0)
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
                easing: easing.clone(),
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
            easing: easing.clone(),
        });
    }
    // Absolute scale: `scale: 1.5`.
    if let Some(s) = named.get("scale").and_then(expr_to_f64) {
        actions.push(Action::Scale {
            target: label.clone(),
            to: s,
            easing: easing.clone(),
        });
    }
    // Relative scale: `scale-by: 1.5` (multiply current scale).
    if let Some(f) = named.get("scale-by").and_then(expr_to_f64) {
        actions.push(Action::ScaleBy {
            target: label.clone(),
            factor: f,
            easing: easing.clone(),
        });
    }
    // Absolute rotate: `rotate: 90`.
    if let Some(deg) = named.get("rotate").and_then(expr_to_f64) {
        actions.push(Action::Rotate {
            target: label.clone(),
            degrees: deg,
            easing: easing.clone(),
        });
    }
    // Relative rotate: `rotate-by: 15` (add to current rotation).
    if let Some(d) = named.get("rotate-by").and_then(expr_to_f64) {
        actions.push(Action::RotateBy {
            target: label.clone(),
            delta_degrees: d,
            easing: easing.clone(),
        });
    }
    if let Some(o) = named.get("opacity").and_then(expr_to_f64) {
        actions.push(Action::FadeTo {
            target: label.clone(),
            opacity: o.clamp(0.0, 1.0),
            easing: easing.clone(),
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
        .unwrap_or(500.0)
        .max(1.0) as u32;

    let label = Label(format!("__block_{}", ctx.block_counter));
    ctx.block_counter += 1;
    ctx.items.insert(label.clone(), body);
    ctx.label_scene.insert(label.clone(), ctx.current_scene);
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
    let Some(label) = target_arg(pos, named) else {
        return;
    };
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
        actions: vec![Action::SaveState {
            target: label,
            slot,
        }],
    });
    ctx.cursor += 1;
}

/// `restore(target, slot: "name", duration: 500, easing: "smooth")` —
/// interpolate back to a previously saved state.
fn process_restore(pos: &[Expr], named: &HashMap<String, Expr>, ctx: &mut ParseCtx) {
    let Some(label) = target_arg(pos, named) else {
        return;
    };
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
        .unwrap_or(500.0)
        .max(1.0) as u32;
    let easing = resolve_easing(named, &label);
    ctx.slides.push(Slide {
        duration_ms: duration,
        actions: vec![Action::Restore {
            target: label,
            slot,
            easing: easing.clone(),
        }],
    });
    ctx.cursor += duration;
}

/// `indicate(target, factor: 1.1, dx: 0, dy: 0, duration: 300, easing: "smooth")`
/// — briefly scale + shift, then return to original.
fn process_indicate(pos: &[Expr], named: &HashMap<String, Expr>, ctx: &mut ParseCtx) {
    let Some(label) = target_arg(pos, named) else {
        return;
    };
    let duration = named
        .get("duration")
        .and_then(expr_to_f64)
        .unwrap_or(300.0)
        .max(1.0) as u32;
    let factor = named.get("factor").and_then(expr_to_f64).unwrap_or(1.1);
    let dx = named.get("dx").and_then(expr_to_f64).unwrap_or(0.0);
    let dy = named.get("dy").and_then(expr_to_f64).unwrap_or(0.0);
    let easing = resolve_easing(named, &label);
    ctx.slides.push(Slide {
        duration_ms: duration,
        actions: vec![Action::Indicate {
            target: label,
            factor,
            dx,
            dy,
            easing: easing.clone(),
        }],
    });
    ctx.cursor += duration;
}

/// `flash(target, factor: 2.0, duration: 200, easing: "smooth")` —
/// briefly enlarge + fade, then return to original.
fn process_flash(pos: &[Expr], named: &HashMap<String, Expr>, ctx: &mut ParseCtx) {
    let Some(label) = target_arg(pos, named) else {
        return;
    };
    let duration = named
        .get("duration")
        .and_then(expr_to_f64)
        .unwrap_or(200.0)
        .max(1.0) as u32;
    let factor = named.get("factor").and_then(expr_to_f64).unwrap_or(2.0);
    let easing = resolve_easing(named, &label);
    ctx.slides.push(Slide {
        duration_ms: duration,
        actions: vec![Action::Flash {
            target: label,
            factor,
            easing: easing.clone(),
        }],
    });
    ctx.cursor += duration;
}

/// `wiggle(target, degrees: 15, duration: 500, easing: "wiggle")` —
/// oscillate rotation, then return to original.
fn process_wiggle(pos: &[Expr], named: &HashMap<String, Expr>, ctx: &mut ParseCtx) {
    let Some(label) = target_arg(pos, named) else {
        return;
    };
    let duration = named
        .get("duration")
        .and_then(expr_to_f64)
        .unwrap_or(500.0)
        .max(1.0) as u32;
    let degrees = named.get("degrees").and_then(expr_to_f64).unwrap_or(15.0);
    let easing = resolve_easing(named, &label);
    ctx.slides.push(Slide {
        duration_ms: duration,
        actions: vec![Action::Wiggle {
            target: label,
            degrees,
            easing: easing.clone(),
        }],
    });
    ctx.cursor += duration;
}

/// `appear(target)` / `disappear(target)` — instantaneous visibility toggle.
/// Emits a 1-frame slide. (`show`/`hide` would conflict with Typst keywords.)
fn process_appear_disappear(pos: &[Expr], appear: bool, ctx: &mut ParseCtx) {
    let Some(label) = target_arg(pos, &HashMap::new()) else {
        return;
    };
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

/// `set_color(target, color: "black", duration: 1, easing: "linear")` —
/// record a color change (tracked, renderer no-op for now).
fn process_set_color(pos: &[Expr], named: &HashMap<String, Expr>, ctx: &mut ParseCtx) {
    let Some(label) = target_arg(pos, named) else {
        return;
    };
    let color = named
        .get("color")
        .and_then(|e| match e {
            Expr::Str(s) => Some(s.get().to_string()),
            _ => None,
        })
        .unwrap_or_else(|| "black".to_string());
    let duration = named
        .get("duration")
        .and_then(expr_to_f64)
        .unwrap_or(1.0)
        .max(1.0) as u32;
    let easing = resolve_easing(named, &label);
    ctx.slides.push(Slide {
        duration_ms: duration,
        actions: vec![Action::SetColor {
            target: label,
            color,
            easing: easing.clone(),
        }],
    });
    ctx.cursor += duration;
}

// ---- Manim-inspired composite animation parsers ----

/// `blink(target, blinks: 3, duration: 30, easing: "linear")` — alternate
/// opacity 1↔0 N times. Mirrors Manim's `Blink`.
fn process_blink(pos: &[Expr], named: &HashMap<String, Expr>, ctx: &mut ParseCtx) {
    let Some(label) = target_arg(pos, named) else {
        return;
    };
    let blinks = named
        .get("blinks")
        .and_then(expr_to_f64)
        .unwrap_or(3.0)
        .max(1.0) as u32;
    let duration = named
        .get("duration")
        .and_then(expr_to_f64)
        .unwrap_or(500.0)
        .max(1.0) as u32;
    let per_blink = (duration / (blinks * 2)).max(1);
    let easing = resolve_easing(named, &label);
    // Each blink = FadeTo(0) + FadeTo(1).
    for _ in 0..blinks {
        ctx.slides.push(Slide {
            duration_ms: per_blink,
            actions: vec![Action::FadeTo {
                target: label.clone(),
                opacity: 0.0,
                easing: easing.clone(),
            }],
        });
        ctx.slides.push(Slide {
            duration_ms: per_blink,
            actions: vec![Action::FadeTo {
                target: label.clone(),
                opacity: 1.0,
                easing: easing.clone(),
            }],
        });
    }
    ctx.cursor += per_blink * blinks * 2;
}

/// `spiral_in(target, scale: 3.0, rotate: 360, duration: 24, easing: "smooth")`
/// — fly in from a scaled-up, rotated state to the natural position, fading in.
/// Mirrors Manim's `SpiralIn`.
fn process_spiral_in(pos: &[Expr], named: &HashMap<String, Expr>, ctx: &mut ParseCtx) {
    let Some(label) = target_arg(pos, named) else {
        return;
    };
    let scale = named.get("scale").and_then(expr_to_f64).unwrap_or(3.0);
    let rotate = named.get("rotate").and_then(expr_to_f64).unwrap_or(360.0);
    let duration = named
        .get("duration")
        .and_then(expr_to_f64)
        .unwrap_or(300.0)
        .max(1.0) as u32;
    let easing = resolve_easing(named, &label);
    // Set initial state: scaled up, rotated, invisible.
    ctx.slides.push(Slide {
        duration_ms: 1,
        actions: vec![
            Action::ScaleBy {
                target: label.clone(),
                factor: scale,
                easing: easing.clone(),
            },
            Action::RotateBy {
                target: label.clone(),
                delta_degrees: rotate,
                easing: easing.clone(),
            },
            Action::Hide {
                target: label.clone(),
            },
        ],
    });
    // Animate to natural state: scale 1, rotate 0, visible.
    ctx.slides.push(Slide {
        duration_ms: duration,
        actions: vec![
            Action::Scale {
                target: label.clone(),
                to: 1.0,
                easing: easing.clone(),
            },
            Action::Rotate {
                target: label.clone(),
                degrees: 0.0,
                easing: easing.clone(),
            },
            Action::FadeIn {
                target: label,
                easing: easing.clone(),
            },
        ],
    });
    ctx.cursor += 1 + duration;
}

/// `focus_on(target, factor: 0.5, duration: 300, easing: "smooth")` —
/// shrink a "spotlight" onto the target. Implemented as a scale-down + fade
/// on the target. Mirrors Manim's `FocusOn`.
fn process_focus_on(pos: &[Expr], named: &HashMap<String, Expr>, ctx: &mut ParseCtx) {
    let Some(label) = target_arg(pos, named) else {
        return;
    };
    let factor = named.get("factor").and_then(expr_to_f64).unwrap_or(0.5);
    let duration = named
        .get("duration")
        .and_then(expr_to_f64)
        .unwrap_or(300.0)
        .max(1.0) as u32;
    let easing = resolve_easing(named, &label);
    ctx.slides.push(Slide {
        duration_ms: duration,
        actions: vec![
            Action::ScaleBy {
                target: label.clone(),
                factor,
                easing: easing.clone(),
            },
            Action::FadeTo {
                target: label,
                opacity: 0.3,
                easing: easing.clone(),
            },
        ],
    });
    ctx.cursor += duration;
}

/// `fade_transform(from: "old", to: "new", duration: 300, easing: "smooth")`
/// — crossfade two mobjects: fade out `from` while fading in `to`. Both
/// must be registered via `mobject`. Mirrors Manim's `FadeTransform` (simple
/// crossfade variant).
fn process_fade_transform(_pos: &[Expr], named: &HashMap<String, Expr>, ctx: &mut ParseCtx) {
    let from = named.get("from").and_then(|e| match e {
        Expr::Str(s) => Some(Label(s.get().to_string())),
        _ => None,
    });
    let to = named.get("to").and_then(|e| match e {
        Expr::Str(s) => Some(Label(s.get().to_string())),
        _ => None,
    });
    let (Some(from), Some(to)) = (from, to) else {
        return;
    };
    let duration = named
        .get("duration")
        .and_then(expr_to_f64)
        .unwrap_or(300.0)
        .max(1.0) as u32;
    let easing = resolve_easing(named, &from);
    // Fade out `from` and fade in `to` in the same slide (parallel).
    ctx.slides.push(Slide {
        duration_ms: duration,
        actions: vec![
            Action::FadeOut {
                target: from,
                easing: easing.clone(),
            },
            Action::FadeIn { target: to, easing },
        ],
    });
    ctx.cursor += duration;
}

/// `move_along_path(target, path, duration: 500, easing: "linear", mode: "polyline", orient: false)`
/// — move the target along a polyline through the given points (cm, absolute).
/// Mirrors Manim's `MoveAlongPath`.
fn process_move_along_path(
    pos: &[Expr],
    named: &HashMap<String, Expr>,
    node: &LinkedNode,
    raw: &str,
    ctx: &mut ParseCtx,
) {
    let Some(label) = target_arg(pos, named) else {
        return;
    };
    let duration = named
        .get("duration")
        .and_then(expr_to_f64)
        .unwrap_or(500.0)
        .max(1.0) as u32;
    let easing = resolve_easing(named, &label);

    // The path is the 2nd positional arg per the Typst signature
    // (`#move-along-path(target, path, ...)`), but we also accept a named
    // `path:` for flexibility. Either way it's an array of `(x, y)` tuples (cm).
    let path_e: Option<&Expr> = named.get("path").or_else(|| pos.get(1));
    let points: Vec<(f64, f64)> = match path_e {
        Some(Expr::Array(arr)) => arr
            .items()
            .filter_map(|item| match item {
                ast::ArrayItem::Pos(e) => tuple_cm(&e, raw, node),
                ast::ArrayItem::Spread(_) => None,
            })
            .collect(),
        _ => Vec::new(),
    };
    if points.is_empty() {
        return;
    }

    // Respect the `mode:` and `orient:` named args from the Typst API.
    let mode = match named.get("mode") {
        Some(Expr::Str(s)) => {
            if s.get() == "bezier" {
                PathMode::Bezier
            } else {
                PathMode::Polyline
            }
        }
        _ => PathMode::Polyline,
    };
    let orient = named
        .get("orient")
        .and_then(|e| match e {
            Expr::Bool(b) => Some(b.get()),
            _ => None,
        })
        .unwrap_or(false);

    ctx.slides.push(Slide {
        duration_ms: duration,
        actions: vec![Action::MoveAlongPath {
            target: label,
            points,
            mode,
            orient,
            easing: easing.clone(),
        }],
    });
    ctx.cursor += duration;
}

/// `#track(target, ((t, (x, y, scale, opacity, rotation)), ...), duration:,
/// easing:)` — a multi-keyframe timeline for one target. Each keyframe is a
/// tuple `(t_ms, (x, y, scale, opacity, rotation))`; omitted properties carry
/// their previous value forward. `t` is relative to the slide start (ms).
fn process_track(pos: &[Expr], named: &HashMap<String, Expr>, ctx: &mut ParseCtx) {
    let Some(label) = target_arg(pos, named) else {
        return;
    };
    let duration = named
        .get("duration")
        .and_then(expr_to_f64)
        .unwrap_or(1000.0)
        .max(1.0) as u32;
    let easing = resolve_easing(named, &label);

    // Keyframes come from the 2nd positional arg (an array of tuples) or
    // `keys:`. Each tuple is `(t, (x, y, scale, opacity, rotation))`.
    let keys_e: Option<&Expr> = named.get("keys").or_else(|| pos.get(1));
    let keyframes: Vec<TrackKey> = match keys_e {
        Some(Expr::Array(arr)) => arr
            .items()
            .filter_map(|item| match item {
                ast::ArrayItem::Pos(e) => track_key_from_expr(&e),
                ast::ArrayItem::Spread(_) => None,
            })
            .collect(),
        _ => Vec::new(),
    };
    if keyframes.is_empty() {
        return;
    }
    ctx.slides.push(Slide {
        duration_ms: duration,
        actions: vec![Action::Track {
            target: label,
            keyframes,
            easing,
        }],
    });
    ctx.cursor += duration;
}

/// `#camera(x:, y:, zoom:, rotate:, duration:, easing:)` — a global pan + zoom
/// + rotate applied to the whole scene. Implemented via a synthetic
/// `__camera__` mobject so it flows through the normal scheduler / interpolator
/// pipeline; the renderer reads it once per frame and never draws it.
fn process_camera(_pos: &[Expr], named: &HashMap<String, Expr>, ctx: &mut ParseCtx) {
    let duration = named
        .get("duration")
        .and_then(expr_to_f64)
        .unwrap_or(1000.0)
        .max(1.0) as u32;
    let easing = match named.get("easing") {
        Some(Expr::Str(s)) => Easing::from_str(s.get().as_str()).unwrap_or(Easing::Linear),
        _ => Easing::Linear,
    };
    let x = named.get("x").and_then(expr_to_f64).unwrap_or(0.0);
    let y = named.get("y").and_then(expr_to_f64).unwrap_or(0.0);
    let zoom = named
        .get("zoom")
        .and_then(expr_to_f64)
        .unwrap_or(1.0)
        .max(1e-3);
    let rotate = named.get("rotate").and_then(expr_to_f64).unwrap_or(0.0);

    let cam = Label("__camera__".into());
    register_synthetic_mobject(ctx, &cam, "none");
    ctx.slides.push(Slide {
        duration_ms: duration,
        actions: vec![Action::Camera {
            target: cam,
            x,
            y,
            zoom,
            rotate,
            easing,
        }],
    });
    ctx.cursor += duration;
}

/// `#group(name, ("child1", "child2", ...))` — declare `name` as a synthetic
/// parent mobject and attach each listed child to it. Subsequent `#animate(name,
/// ...)` moves / rotates / scales all children together (parent→child transform
/// inheritance). Groups may be nested (a child may itself be a group).
fn process_group(pos: &[Expr], named: &HashMap<String, Expr>, ctx: &mut ParseCtx) {
    let name = pos
        .first()
        .or_else(|| named.get("name"))
        .and_then(|e| match e {
            Expr::Str(s) => Some(s.get().to_string()),
            _ => None,
        });
    let Some(name) = name else {
        return;
    };
    let parent = Label(name);
    register_synthetic_mobject(ctx, &parent, "none");

    // Children from the 2nd positional array or `members:`.
    let members_e: Option<&Expr> = named.get("members").or_else(|| pos.get(1));
    let children: Vec<Label> = match members_e {
        Some(Expr::Array(arr)) => arr
            .items()
            .filter_map(|it| match it {
                ast::ArrayItem::Pos(Expr::Str(s)) => Some(Label(s.get().to_string())),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    };
    for c in children {
        ctx.groups.insert(c, parent.clone());
    }
}

/// `#reveal(target, by: "char"|"word", duration:, easing:)` and
/// `#typewriter(target, duration:, easing:)` — progressively reveal a *string*
/// mobject (e.g. `"Hello"`) by swapping its body to longer and longer prefixes
/// over `duration`. Non-string bodies fall back to a plain FadeIn with a warning
/// (char/word reveal only makes sense for text).
fn process_reveal(pos: &[Expr], named: &HashMap<String, Expr>, sym: &str, ctx: &mut ParseCtx) {
    let Some(label) = target_arg(pos, named) else {
        return;
    };
    let duration = named
        .get("duration")
        .and_then(expr_to_f64)
        .unwrap_or(1000.0)
        .max(1.0) as u32;
    let by = match named.get("by") {
        Some(Expr::Str(s)) => s.get().to_string(),
        _ => {
            if sym == "typewriter" {
                "char".to_string()
            } else {
                "word".to_string()
            }
        }
    };
    let _ = resolve_easing(named, &label);

    // The body must be a string literal ("...") for char/word reveal.
    let Some(body) = ctx.items.get(&label) else {
        return;
    };
    let Some(inner) = strip_string_literal(body) else {
        eprintln!(
            "warn: #reveal/@{} body is not a string literal; falling back to FadeIn",
            label.0
        );
        ctx.slides.push(Slide {
            duration_ms: duration,
            actions: vec![Action::FadeIn {
                target: label,
                easing: Easing::Linear,
            }],
        });
        ctx.cursor += duration;
        return;
    };

    let chunks: Vec<String> = if by == "word" {
        inner.split_whitespace().map(|s| s.to_string()).collect()
    } else {
        inner.chars().map(|c| c.to_string()).collect()
    };
    let n = chunks.len().max(1);
    let step = (duration as f64 / n as f64).ceil().max(1.0) as u32;
    let start = ctx.cursor;

    let tl = ctx.content_timeline.entry(label.clone()).or_default();
    // Hide at the reveal start (use `none` so the body compiles to nothing).
    tl.push((start, "none".to_string()));
    for k in 1..=n {
        let prefix: String = if by == "word" {
            chunks[..k].join(" ")
        } else {
            chunks[..k].concat()
        };
        let at = (start + k as u32 * step).min(start + duration);
        tl.push((at, format!("\"{prefix}\"")));
    }
    tl.push((start + duration, format!("\"{inner}\"")));

    // A `reveal`/`typewriter` is supposed to *introduce* the text from nothing.
    // By default `content_for` falls back to the mobject's original (full) body
    // for any frame *before* the first timeline entry, so the complete string
    // would flash on screen and only then get "revealed" (full → partial →
    // full) — which looks broken. Hide the target from the very start of the
    // timeline unless something already controls its content or visibility
    // earlier (a prior `reveal`/`transform` on the same label, or any earlier
    // action such as `appear`/`animate` targeting it).
    let controlled_earlier = tl.iter().any(|(t, _)| *t < start);
    let appeared_earlier = ctx
        .slides
        .iter()
        .any(|s| s.actions.iter().any(|a| a.target() == &label));
    if !controlled_earlier && !appeared_earlier && start > 0 {
        tl.insert(0, (0, "none".to_string()));
    }

    ctx.slides.push(Slide {
        duration_ms: duration,
        actions: vec![],
    });
    ctx.cursor += duration;
}

/// Register a synthetic mobject (e.g. the camera or a group parent) with an
/// empty body, without overwriting an existing one.
fn register_synthetic_mobject(ctx: &mut ParseCtx, label: &Label, body: &str) {
    if !ctx.items.contains_key(label) {
        ctx.items.insert(label.clone(), body.to_string());
        ctx.label_scene.insert(label.clone(), ctx.current_scene);
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
}

/// Parse a `#track` keyframe tuple `(t, (x, y, scale, opacity, rotation))` into
/// a [`TrackKey`]. `x`/`y` are unit-aware centimeters; `scale`/`opacity`/
/// `rotation` are unitless numbers.
fn track_key_from_expr(e: &Expr) -> Option<TrackKey> {
    // A parenthesized tuple `(a, b, ...)` surfaces as `Expr::Parenthesized`
    // wrapping an `Expr::Array` in typst_syntax, so unwrap either form.
    let arr = match as_array(e) {
        Some(a) => a,
        None => return None,
    };
    let items: Vec<ast::ArrayItem> = arr.items().collect();
    if items.len() < 2 {
        return None;
    }
    let t = match &items[0] {
        ast::ArrayItem::Pos(e) => expr_to_f64(e)?,
        _ => return None,
    } as u32;
    let st: Vec<ast::ArrayItem> = match &items[1] {
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
    Some(TrackKey {
        t,
        x,
        y,
        scale,
        opacity,
        rotation,
    })
}

/// If `body` is a string literal `"..."`, return its inner text; else `None`.
fn strip_string_literal(body: &str) -> Option<String> {
    let b = body.trim();
    if b.starts_with('"') && b.ends_with('"') && b.len() >= 2 {
        Some(b[1..b.len() - 1].to_string())
    } else {
        None
    }
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
    let (Some(from), Some(to)) = (from, to) else {
        return;
    };
    let duration = named
        .get("duration")
        .and_then(expr_to_f64)
        .unwrap_or(24.0)
        .max(1.0) as u32;
    let easing = resolve_easing(named, &from);

    // Hide the `to` object initially (it will fade in as the shape morphs in).
    ctx.slides.push(Slide {
        duration_ms: 1,
        actions: vec![Action::Hide { target: to.clone() }],
    });

    // The shape morph itself is rendered by the renderer (a `MorphPlan`
    // precomputed from the two bodies' outlines). Here we only drive the
    // *opacity* crossfade so `from` fades/shrinks out while `to` fades in at
    // its natural size (no `ScaleBy 100` — that previously left `to` 100×
    // oversized after the morph).
    let start_ms = ctx.cursor + 1;
    let end_ms = start_ms + duration;
    ctx.slides.push(Slide {
        duration_ms: duration,
        actions: vec![
            Action::ScaleBy {
                target: from.clone(),
                factor: 0.01,
                easing: easing.clone(),
            },
            Action::FadeOut {
                target: from.clone(),
                easing: easing.clone(),
            },
            Action::FadeIn {
                target: to.clone(),
                easing: easing.clone(),
            },
        ],
    });
    ctx.morph_pairs.push(crate::core::ast::MorphPair {
        from: from.clone(),
        to: to.clone(),
        to_body: None,
        start_ms,
        end_ms,
        easing,
    });
    ctx.cursor += 1 + duration;
}

/// `transform(target, to: <content>, duration: 24, easing: "smooth")` —
/// Manim's `Transform` / `ReplacementTransform`: morph a single mobject's
/// content into a new inline `content` (a Typst body, e.g. an equation
/// `[$a + b = c$]`). Unlike `morph` (which needs two pre-registered mobjects),
/// `transform` takes the new content inline and keeps the **original label**
/// holding the new content afterwards, so subsequent `#animate` calls operate on
/// the transformed object.
///
/// Implemented as a crossfade + scale (the same mechanism as `morph`), but the
/// old content is parked on a synthetic `__xf_<label>` mobject that fades out and
/// shrinks while the target (now showing the new content) fades in and grows.
/// The synthetic mobject ends invisible, so the final frame shows only the
/// transformed target.
fn process_transform(
    pos: &[Expr],
    named: &HashMap<String, Expr>,
    node: &LinkedNode,
    raw: &str,
    ctx: &mut ParseCtx,
) {
    let label = target_arg(pos, named);
    let Some(label) = label else { return };

    // `to` may be the 2nd positional arg or the `to:` named arg.
    let to_expr = pos.get(1).or_else(|| named.get("to"));
    let Some(to_expr) = to_expr else { return };
    let new_body = expr_src(raw, node, to_expr).to_string();
    if new_body.is_empty() {
        return;
    }

    let duration = named
        .get("duration")
        .and_then(expr_to_f64)
        .unwrap_or(24.0)
        .max(1.0) as u32;
    let easing = resolve_easing(named, &label);

    // Capture the current content of `target` before we replace it.
    let old_body = ctx.items.get(&label).cloned().unwrap_or_default();

    // No existing mobject → just fade the new content in.
    if old_body.is_empty() {
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
        ctx.items.insert(label.clone(), new_body);
        ctx.slides.push(Slide {
            duration_ms: duration,
            actions: vec![Action::FadeIn {
                target: label,
                easing: easing.clone(),
            }],
        });
        ctx.cursor += duration;
        return;
    }

    // Synthetic mobject holding the OLD content. It is invisible until the
    // transform slide (so earlier frames render `target` only, not a duplicate)
    // and uses a *unique* label per transform so repeated transforms on the
    // same label don't clash.
    let tmp = Label(format!("__xf_{}_{}", label.0, ctx.xf_counter));
    ctx.xf_counter += 1;
    ctx.items.insert(tmp.clone(), old_body.clone());
    // The parked old-content mobject belongs to the *target's* scene so it is
    // shown/hidden together with the target across the transform.
    ctx.label_scene.insert(
        tmp.clone(),
        ctx.label_scene
            .get(&label)
            .copied()
            .unwrap_or(ctx.current_scene),
    );
    ctx.initial.insert(
        tmp.clone(),
        FrameData {
            time_ms: 0,
            target: tmp.clone(),
            x: 0.0,
            y: 0.0,
            scale: 1.0,
            opacity: 0.0,
            rotation: 0.0,
            easing: Easing::Linear,
        },
    );

    // IMPORTANT: do NOT overwrite `items[label]`. The original body must stay
    // in `items` so every frame *before* this transform still renders the old
    // content. Instead we record a *content switch* on the timeline: from
    // `old_body` (the current `items[label]`) to `new_body`, taking effect 1ms
    // into the morph slide. The renderer picks the latest switch ≤ frame time.
    let switch_at = ctx.cursor + 1;
    ctx.content_timeline
        .entry(label.clone())
        .or_default()
        .push((switch_at, new_body.clone()));

    // Real shape morph: precompute a `MorphPlan` between the old content's
    // outline and the new content's outline, and render the *target* (keeping
    // its label so later `#animate`s still apply to it) as the interpolated shape
    // during the window. This upgrades `transform` from a plain opacity crossfade
    // to a genuine outline morph (the same machinery as `#morph`). The new
    // content is passed via `to_body` so the plan uses it without spawning a
    // stray mobject.
    ctx.morph_pairs.push(crate::core::ast::MorphPair {
        from: tmp.clone(),
        to: label.clone(),
        to_body: Some(new_body.clone()),
        start_ms: switch_at,
        end_ms: switch_at + duration,
        easing: easing.clone(),
    });

    // Single morph slide: the scheduler's native `Transform` action crossfades
    // `old` out while `target` (now showing `new_body`) fades in, inheriting
    // `target`'s current transform — no positional jump, no scale accumulation.
    ctx.slides.push(Slide {
        duration_ms: duration,
        actions: vec![Action::Transform {
            target: label.clone(),
            old: tmp,
            easing: easing.clone(),
        }],
    });
    ctx.cursor += duration;
}

// ============================================================================
// Subtitle module
// ============================================================================

/// `subtitle(body, duration:, position:, easing:)` — register a caption overlay.
///
/// - `body`: any valid Typst block content (e.g. `[Hello]`, `text(20pt)[Hi]`).
/// - `duration`: optional lifetime in ms. Default = persist until replaced by
///   another subtitle in the **same scope**, or until the scope exits.
/// - `position`: anchor — `"bottom"` (default), `"top"`, `"center"`,
///   `"bottom-left"/"bottom-right"/"top-left"/"top-right"`, or an absolute
///   `(x, y)` in cm.
/// - `easing`: fade curve for the caption's own in/out (default `"linear"`).
///
/// Scoping: only one subtitle per Typst scope is visible at once (a later one
/// replaces an earlier); a parent scope's subtitle is hidden while a child
/// scope shows its own; leaving the scope auto-destroys it. Inert under
/// standard Typst (`#subtitle(...) = none`).
fn process_subtitle(
    pos: &[Expr],
    named: &HashMap<String, Expr>,
    node: &LinkedNode,
    raw: &str,
    ctx: &mut ParseCtx,
) {
    let body_expr = pos.first().or_else(|| named.get("body"));
    let Some(body_expr) = body_expr else { return };
    let body = expr_src(raw, node, body_expr).to_string();
    if body.is_empty() {
        return;
    }
    let duration = named
        .get("duration")
        .and_then(expr_to_f64)
        .map(|d| d.max(1.0) as u32);
    let position = parse_sub_pos(named);
    let easing = resolve_easing(named, &Label("subtitle".into()));

    let id = format!("__sub_{}", ctx.subtitle_id);
    ctx.subtitle_id += 1;
    let start_ms = ctx.cursor;
    let end_ms = duration.map(|d| start_ms + d);

    ctx.subtitles.push(Subtitle {
        id,
        scope: current_scope(ctx),
        body,
        start_ms,
        end_ms,
        position,
        easing: easing.clone(),
    });
}

/// Parse the `position:` named arg of `subtitle` into a [`SubPos`].
fn parse_sub_pos(named: &HashMap<String, Expr>) -> SubPos {
    let Some(e) = named.get("position") else {
        return SubPos::Bottom;
    };
    match e {
        Expr::Str(s) => match s.get().to_ascii_lowercase().as_str() {
            "bottom" => SubPos::Bottom,
            "top" => SubPos::Top,
            "center" | "centre" => SubPos::Center,
            "bottom-left" => SubPos::BottomLeft,
            "bottom-right" => SubPos::BottomRight,
            "top-left" => SubPos::TopLeft,
            "top-right" => SubPos::TopRight,
            _ => SubPos::Bottom,
        },
        // Absolute `(x, y)` in cm.
        _ => match tuple_cm(e, "", &LinkedNode::new(&typst_syntax::parse(""))) {
            Some((x, y)) => SubPos::Absolute(x, y),
            None => SubPos::Bottom,
        },
    }
}

// ============================================================================
// Easing-counter module
// ============================================================================

/// `ecounter(name, seed:, step:, duration:, easing:)` — define a named integer
/// counter. The value is read via `ecval(name)` (substituted per-frame by the
/// renderer). Standard-Typst returns `seed`; animation mode steps the value
/// (eased over `duration`, or once per ms when long-lived). Inert under
/// standard Typst (`#ecounter(...) = none`).
fn process_ecounter(pos: &[Expr], named: &HashMap<String, Expr>, ctx: &mut ParseCtx) {
    let name = pos
        .first()
        .or_else(|| named.get("name"))
        .and_then(|e| match e {
            Expr::Str(s) => Some(s.get().to_string()),
            _ => None,
        });
    let Some(name) = name else { return };
    let seed = named.get("seed").and_then(expr_to_i64).unwrap_or(0);
    let step = named.get("step").and_then(expr_to_i64).unwrap_or(1);
    let duration_ms = named
        .get("duration")
        .and_then(expr_to_f64)
        .map(|d| d.max(1.0) as u32);
    let easing = resolve_easing(named, &Label(format!("counter:{name}")));

    ctx.counters.push(CounterDef {
        name,
        scope: current_scope(ctx),
        seed,
        step,
        duration_ms,
        easing,
        start_ms: ctx.cursor,
    });
}

/// `counter_pause(name)` / `counter_resume(name)` / `counter_destroy(name)` —
/// record a lifecycle event on a named counter at the current timeline.
fn process_counter_event(
    pos: &[Expr],
    named: &HashMap<String, Expr>,
    ctx: &mut ParseCtx,
    kind: CounterEventKind,
) {
    let name = pos
        .first()
        .or_else(|| named.get("name"))
        .and_then(|e| match e {
            Expr::Str(s) => Some(s.get().to_string()),
            _ => None,
        });
    let Some(name) = name else { return };
    ctx.counter_events.push(CounterEvent {
        name,
        kind,
        at_ms: ctx.cursor,
    });
}

/// Evaluate a unit-less numeric expression to `i64` (for counter seed/step).
fn expr_to_i64(e: &Expr) -> Option<i64> {
    match e {
        Expr::Int(i) => Some(i.get() as i64),
        Expr::Float(f) => Some(f.get().round() as i64),
        Expr::Numeric(n) => Some(n.get().0.round() as i64),
        _ => None,
    }
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

/// If `e` is an array literal `(a, b, ...)` — possibly wrapped in
/// `Expr::Parenthesized` — return the inner `Array` node. Used by
/// `track_key_from_expr`, which must accept parenthesized tuples the same way
/// `tuple_cm` does.
fn as_array<'a>(e: &'a Expr<'a>) -> Option<ast::Array<'a>> {
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
        assert_eq!(
            scene.items[&Label("dot".into())],
            "circle(radius: 1cm, fill: blue)"
        );
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
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../typst/src/lib.typ"),
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
        let has_rotate = actions
            .iter()
            .any(|a| matches!(a, Action::Rotate { degrees: 90.0, .. }));
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
        assert!(
            matches!(scene.slides[0].actions[0], Action::SaveState { ref slot, .. } if slot == "home")
        );
        assert!(matches!(scene.slides[1].actions[0], Action::MoveTo { .. }));
        assert!(
            matches!(scene.slides[2].actions[0], Action::Restore { ref slot, .. } if slot == "home")
        );
        assert!(matches!(
            scene.slides[3].actions[0],
            Action::Indicate { factor: 1.2, .. }
        ));
        assert!(matches!(
            scene.slides[4].actions[0],
            Action::Flash { factor: 1.8, .. }
        ));
        assert!(matches!(
            scene.slides[5].actions[0],
            Action::Wiggle { degrees: 12.0, .. }
        ));
        assert!(matches!(scene.slides[6].actions[0], Action::Hide { .. }));
        assert!(matches!(scene.slides[7].actions[0], Action::Show { .. }));
        assert!(
            matches!(scene.slides[8].actions[0], Action::SetColor { ref color, .. } if color == "red")
        );
        std::fs::remove_file(&tmp).ok();
    }

    /// Verify `transform(target, to: <content>)` parks the old content on a
    /// unique synthetic `__xf_<label>_<n>` mobject, keeps `items[target]` as
    /// the ORIGINAL body (so earlier slides still render it), records the
    /// content switch on `content_timeline`, and emits a single `Transform`
    /// slide.
    #[test]
    fn parses_transform() {
        let src = r#"
#import "candy": *
#mobject("eq", [$a + b = c$])
#transform("eq", to: [$a + b + d = c$], duration: 20, easing: "smooth")
#mobject("box", rect(width: 2cm, height: 2cm))
#transform("box", to: circle(radius: 1.5cm, fill: blue))
"#;
        let tmp = std::env::temp_dir().join("candy_test_transform.tyx");
        std::fs::write(&tmp, src).unwrap();
        let scene = parse_tyx(&tmp).unwrap();
        // eq: 1 mobject + 1 Transform slide; box: 1 mobject + 1 Transform slide.
        // total = 2 slides.
        assert_eq!(scene.slides.len(), 2, "slides: {:?}", scene.slides);

        // `items[eq]` / `items[box]` keep their ORIGINAL bodies — the new
        // content is recorded on the timeline instead.
        assert_eq!(scene.items[&Label("eq".into())], "[$a + b = c$]");
        assert_eq!(
            scene.items[&Label("box".into())],
            "rect(width: 2cm, height: 2cm)"
        );

        // The old content is parked on a unique synthetic mobject. The counter
        // is global per parse, so eq→`__xf_eq_0` and box→`__xf_box_1`.
        assert_eq!(scene.items[&Label("__xf_eq_0".into())], "[$a + b = c$]");
        assert_eq!(
            scene.items[&Label("__xf_box_1".into())],
            "rect(width: 2cm, height: 2cm)"
        );

        // The content switch is recorded on the timeline at `cursor + 1`.
        assert_eq!(
            scene.content_timeline[&Label("eq".into())],
            vec![(1u32, "[$a + b + d = c$]".to_string())]
        );
        assert_eq!(
            scene.content_timeline[&Label("box".into())],
            vec![(21u32, "circle(radius: 1.5cm, fill: blue)".to_string())]
        );

        // Each transform emits a single `Transform` action on its target.
        assert!(matches!(
            &scene.slides[0].actions[..],
            [Action::Transform { target, .. }] if target.0 == "eq"
        ));
        assert!(matches!(
            &scene.slides[1].actions[..],
            [Action::Transform { target, .. }] if target.0 == "box"
        ));
        std::fs::remove_file(&tmp).ok();
    }

    /// Regression: a sequence of `transform`s must NOT accumulate `scale`
    /// (the old `ScaleBy 100` approach blew up to 100×100 = 10000), and the
    /// parked old-content mobject must INHERIT the target's current position
    /// (so the old content does not jump to the origin `(0,0)`).
    #[test]
    fn transform_keeps_scale_bounded_and_inherits_position() {
        let src = r#"
#import "candy": *
#mobject("shape", rect(width: 3cm, height: 3cm, fill: blue))
#animate("shape", to: (5cm, 0cm), duration: 30)
#transform("shape", to: circle(radius: 1.6cm, fill: red), duration: 30)
#transform("shape", to: rect(width: 1cm), duration: 30)
"#;
        let tmp = std::env::temp_dir().join("candy_test_transform_sched.tyx");
        std::fs::write(&tmp, src).unwrap();
        let scene = parse_tyx(&tmp).unwrap();
        let frames = crate::core::scheduler::schedule(&scene).unwrap();

        // scale must stay bounded for every label (no explosion).
        for f in &frames {
            assert!(f.scale <= 2.0, "scale blew up to {}", f.scale);
            assert!(f.scale >= 1e-4, "scale shrank to {}", f.scale);
        }

        // The parked `__xf_shape_*` mobject must track the target's position
        // (5cm, 0cm) during the transform, not sit at the origin.
        let xf: Vec<&crate::core::ast::FrameData> = frames
            .iter()
            .filter(|f| f.target.0.starts_with("__xf_shape"))
            .collect();
        assert!(!xf.is_empty(), "old-content mobject missing");
        for f in &xf {
            if f.time_ms > 30 {
                assert!(
                    (f.x - 5.0).abs() < 1e-6,
                    "old content x should inherit target (5cm), got {}",
                    f.x
                );
                assert!(
                    (f.y - 0.0).abs() < 1e-6,
                    "old content y should inherit target (0cm), got {}",
                    f.y
                );
            }
        }
        std::fs::remove_file(&tmp).ok();
    }

    /// Verify the new directives compile as valid standard Typst (lib.typ
    /// defines them all as no-ops).
    #[test]
    fn std_typst_manim_api_compiles() {
        let lib = std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../typst/src/lib.typ"),
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
        assert!(
            out.is_ok(),
            "std Typst failed to compile manim API: {out:?}"
        );
    }

    /// Verify nested `#scene` calls build a scene tree: the inner scene owns
    /// its mobject, the outer scene owns its own, and `active_scene_at`
    /// returns the innermost scene spanning a given frame (parent auto-hide).
    #[test]
    fn parses_nested_scenes() {
        let src = r#"
#import "candy": *
#scene(width: 16cm, height: 9cm)[
  #mobject("a", circle(radius: 1cm))
  #animate("a", to: (4cm, 0pt), duration: 30)
  #scene(width: 10cm, height: 6cm)[
    #mobject("b", rect(width: 1cm))
    #animate("b", to: (2cm, 0pt), duration: 20)
  ]
]
"#;
        let tmp = std::env::temp_dir().join("candy_test_nested_scene.tyx");
        std::fs::write(&tmp, src).unwrap();
        let scene = parse_tyx(&tmp).unwrap();

        // Root (id 0) + outer (id 1) + inner (id 2).
        assert_eq!(scene.scenes.len(), 3, "scenes: {:?}", scene.scenes);
        assert_eq!(scene.root_scene, Some(0));

        let owner = scene.label_scene_map();
        assert_eq!(owner[&Label("a".into())], 1, "a → outer scene");
        assert_eq!(owner[&Label("b".into())], 2, "b → inner scene");

        // Inner scene spans [30, 50] (after the outer animate); outer [0, 50].
        let inner = scene.scenes.iter().find(|s| s.id == 2).unwrap();
        assert_eq!((inner.start_ms, inner.end_ms), (30, 50));
        let outer = scene.scenes.iter().find(|s| s.id == 1).unwrap();
        assert_eq!((outer.start_ms, outer.end_ms), (0, 50));
        assert_eq!(
            inner.page_size,
            Some((10.0 * 28.346_456_692_913_385, 6.0 * 28.346_456_692_913_385))
        );

        // Parent auto-hide: at t=10 only the outer scene is active; at t=40 the
        // innermost (inner) scene is active.
        assert_eq!(scene.active_scene_at(10), 1);
        assert_eq!(scene.active_scene_at(40), 2);
        std::fs::remove_file(&tmp).ok();
    }
}
