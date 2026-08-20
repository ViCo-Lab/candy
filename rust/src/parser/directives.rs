//! Per-directive handlers for the `.tyx` parser.
//!
//! Every Candy directive (`mobject`, `animate`, `pause`, `track`, `reveal`,
//! `subtitle`, the easing-counter family, …) has a `process_*` function here
//! that reads its positional / named arguments off the Typst AST and appends
//! the corresponding [`crate::core::ast::Slide`] / metadata to [`ParseCtx`].
//!
//! [`process_call`] is the single dispatcher: it resolves the call's Candy
//! symbol (via [`crate::parser::expr::call_symbol`]) and routes to the right
//! handler.

use typst_syntax::LinkedNode;
use typst_syntax::ast::{self, AstNode, Expr};

use crate::core::ast::{
    Action, AudioTrack, CounterDef, CounterEvent, CounterEventKind, FrameData, Keyframe,
    KeyframeCounterDef, Label, PathMode, Slide, Subtitle, Timing, TrackKey,
};
use crate::core::diag::{CandyError, CandyWarn, SourceLoc};
use crate::core::easing::Easing;
use crate::warn;

use crate::parser::ast_walk::ParseCtx;
use crate::parser::expr::{
    call_symbol, current_scope, expr_key_desc, expr_src, expr_to_angle, expr_to_bool, expr_to_f64,
    expr_to_i64, expr_to_key, expr_to_ratio, is_valid_typst_ident, parse_sub_pos, range_of,
    resolve_easing, strip_string_literal, target_arg, track_key_from_expr, tuple_cm,
};

/// Parse the `timing` named argument. Returns `None` when absent (the caller
/// treats `None` as the sequential `After` default); `"with"` maps to
/// [`Timing::With`], anything else (incl. `"after"`) to [`Timing::After`].
fn parse_timing(named: &std::collections::HashMap<String, Expr>) -> Option<Timing> {
    match named.get("timing") {
        Some(Expr::Str(s)) => match s.get().as_str() {
            "with" => Some(Timing::With),
            _ => Some(Timing::After),
        },
        _ => None,
    }
}

/// Parse the `delay` named argument (milliseconds; defaults to `0`).
fn parse_delay(named: &std::collections::HashMap<String, Expr>) -> u32 {
    named
        .get("delay")
        .and_then(expr_to_f64)
        .unwrap_or(0.0)
        .max(0.0) as u32
}

/// Emit a single-slide directive entry at its resolved absolute start time,
/// then close the entry. This is the common case for object animations that
/// produce exactly one slide.
fn emit_slide(
    ctx: &mut ParseCtx,
    timing: Option<Timing>,
    delay: u32,
    duration: u32,
    actions: Vec<Action>,
) {
    let start = ctx.entry_start(timing, delay);
    ctx.slides.push(Slide {
        start_ms: start,
        duration_ms: duration,
        actions,
        loc: ctx.current_directive_loc.clone(),
    });
    ctx.entry_advance(duration);
    ctx.entry_close();
}

/// Register `label` as owned by `scene`, recording its first-seen (declaration)
/// position in `label_order` so mobjects can later be laid out / painted in
/// source order. `HashMap` iteration is not stable, so this explicit order is
/// what preventsparallel mobjects from coming out in a scrambled arrangement.
fn register_label(ctx: &mut ParseCtx, label: Label, scene: usize) {
    if !ctx.label_scene.contains_key(&label) {
        ctx.label_order.push(label.clone());
    }
    ctx.label_scene.insert(label, scene);
}

/// Resolve a named ratio argument (e.g. `opacity: 50%`).
///
/// Returns `None` when the argument is absent. When the argument *is* present
/// but is not a valid Typst `ratio` literal (Typst does not treat a bare number
/// as a ratio), an `E007 InvalidKey` is raised so the mistake is reported up
/// front here, instead of being silently coerced or surfacing as a confusing
/// Typst panic at compile time.
fn ratio_arg(
    named: &std::collections::HashMap<String, Expr>,
    key: &str,
    ctx: &mut ParseCtx,
) -> Option<f64> {
    let e = named.get(key)?;
    match expr_to_ratio(e) {
        Some(v) => Some(v),
        None => {
            ctx.pending_error = Some(CandyError::InvalidKey {
                what: format!("`{key}` (must be a ratio, e.g. `50%`)"),
                value: expr_key_desc(e),
                not_ident: false,
                loc: ctx.current_directive_loc.clone(),
            });
            None
        }
    }
}

/// Resolve a named angle argument (e.g. `rotate: 90deg`).
///
/// See [`ratio_arg`] for the error-handling contract; a bare number is *not* an
/// angle in Typst.
fn angle_arg(
    named: &std::collections::HashMap<String, Expr>,
    key: &str,
    ctx: &mut ParseCtx,
) -> Option<f64> {
    let e = named.get(key)?;
    match expr_to_angle(e) {
        Some(v) => Some(v),
        None => {
            ctx.pending_error = Some(CandyError::InvalidKey {
                what: format!("`{key}` (must be an angle, e.g. `90deg`)"),
                value: expr_key_desc(e),
                not_ident: false,
                loc: ctx.current_directive_loc.clone(),
            });
            None
        }
    }
}

/// Resolve and dispatch a single Candy function call.
pub(crate) fn process_call(call: ast::FuncCall, node: &LinkedNode, raw: &str, ctx: &mut ParseCtx) {
    let Some(sym) = call_symbol(&call, ctx) else {
        return;
    };

    // Record this directive's source location so `emit_slide` can attach it to
    // the `Slide`(s) it produces — used by the `E002`/`Parse` `duration_ms` error.
    ctx.current_directive_loc = Some(ctx.loc(node.range()));

    let args = call.args();
    let mut pos: Vec<Expr> = Vec::new();
    let mut named: std::collections::HashMap<String, Expr> = std::collections::HashMap::new();
    for a in args.items() {
        match a {
            ast::Arg::Pos(e) => pos.push(e),
            ast::Arg::Named(n) => {
                named.insert(n.name().as_str().to_string(), n.expr());
            }
            ast::Arg::Spread(_) => {}
        }
    }

    // Record the source location of the directive's name reference (target
    // label / counter name) so name-anomaly errors can point at the usage.
    record_name_refs(node, &sym, &pos, &named, raw, ctx);

    match sym.as_str() {
        "track" => process_track(&pos, &named, ctx),
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
        "appear" => process_appear_disappear(&pos, true, &named, ctx),
        "disappear" => process_appear_disappear(&pos, false, &named, ctx),
        "set-color" => process_set_color(&pos, &named, node, raw, ctx),
        // Manim-inspired composite animations.
        "blink" => process_blink(&pos, &named, ctx),
        "spiral-in" => process_spiral_in(&pos, &named, ctx),
        "focus-on" => process_focus_on(&pos, &named, ctx),
        "fade-transform" => process_fade_transform(&pos, &named, ctx),
        "move-along-path" => process_move_along_path(&pos, &named, node, raw, ctx),
        "morph" => process_morph(&pos, &named, ctx),
        "transform" => process_transform(&pos, &named, node, raw, ctx),
        // Multi-keyframe camera + grouping + text reveal.
        "camera" => process_camera(&pos, &named, ctx),
        "group" => process_group(&pos, &named, ctx),
        "reveal" | "typewriter" => process_reveal(&pos, &named, sym.as_str(), ctx),
        // Subtitle + easing-counter modules.
        "subtitle" => process_subtitle(&pos, &named, node, raw, ctx),
        "ecnew" => process_ecnew(&pos, &named, node, raw, ctx),
        "scene-switch" => process_scene_switch(&pos, &named, node, raw, ctx),
        "ecval" => { /* read; value substituted per-frame by the renderer */ }
        "ecpause" => {
            process_counter_event(&pos, &named, node, raw, ctx, CounterEventKind::Pause, false)
        }
        "ecresume" => process_counter_event(
            &pos,
            &named,
            node,
            raw,
            ctx,
            CounterEventKind::Resume,
            false,
        ),
        "ecdestroy" => process_counter_event(
            &pos,
            &named,
            node,
            raw,
            ctx,
            CounterEventKind::Destroy,
            false,
        ),
        // Keyframe-counter module.
        "kcnew" => process_kcnew(&pos, &named, node, raw, ctx),
        "kcval" => { /* read; value substituted per-frame by the renderer */ }
        "kcpush" => process_kcpush(&pos, &named, node, raw, ctx),
        "kcpause" => {
            process_counter_event(&pos, &named, node, raw, ctx, CounterEventKind::Pause, true)
        }
        "kcresume" => {
            process_counter_event(&pos, &named, node, raw, ctx, CounterEventKind::Resume, true)
        }
        "kcdestroy" => process_counter_event(
            &pos,
            &named,
            node,
            raw,
            ctx,
            CounterEventKind::Destroy,
            true,
        ),
        // Snake-case Candy private functions — warn but still parse.
        sym if sym.starts_with('_') => {
            let loc = ctx
                .current_directive_loc
                .clone()
                .unwrap_or_else(|| SourceLoc::at(&ctx.file_path, &ctx.source, 0..0));
            warn!(CandyWarn::CallingPrivate(sym.to_string(), loc));
        }
        _ => {}
    }
}

/// Record the source location of a *name reference* (a target label or
/// easing-counter name written inside a directive such as `#animate(target:
/// "x")`, `#ecpause("c")`, …) so name-anomaly errors (`E004` LabelNotFound /
/// `E006` UnknownKey) can point at the *usage* site rather than only at a
/// declaration (which does not exist for an unknown name). We resolve the
/// directive's name argument, then locate the matching string-literal node in
/// the call's syntax subtree.
fn record_name_refs(
    node: &LinkedNode,
    sym: &str,
    pos: &[Expr],
    named: &std::collections::HashMap<String, Expr>,
    _raw: &str,
    ctx: &mut ParseCtx,
) {
    // Directives whose first positional or `target:`/`name:` argument is a name
    // reference. The resolution below mirrors each directive's actual argument
    // handling (see `target_arg` in `expr.rs` and the per-directive parsers) so a
    // reference is recorded whether the user wrote it positionally
    // (`#animate("ghost", …)`) or as a named arg (`#animate(target: "ghost")`).
    // Declarations / directives without a name argument (`mobject`, `group`,
    // `subtitle`, `pause`, `audio`, `camera`) are skipped to avoid recording
    // non-name string literals (e.g. subtitle text) as name references.
    let name_expr: Option<&Expr> = match sym {
        // Resolve via `target_arg`: positional OR `target:`.
        "animate" | "track" | "indicate" | "flash" | "wiggle" | "appear" | "disappear"
        | "set-color" | "blink" | "spiral-in" | "focus-on" | "fade-transform"
        | "move-along-path" | "morph" | "transform" | "reveal" | "typewriter" => {
            pos.first().or_else(|| named.get("target"))
        }
        // scene-switch: `target:` OR `name:` (no positional form).
        "scene-switch" => named.get("target").or_else(|| named.get("name")),
        // Easing-counter directives: positional OR `name:`.
        "ecnew" | "ecval" | "ecpause" | "ecresume" | "ecdestroy" | "ecadd" | "ecset" => {
            pos.first().or_else(|| named.get("name"))
        }
        // Declarations / no name argument — never a usage site, skip.
        "mobject" | "group" | "subtitle" | "pause" | "audio" | "camera" => return,
        // Anything else — be conservative and don't record a phantom name ref.
        _ => return,
    };
    let name = match name_expr {
        Some(Expr::Str(s)) => s.get().to_string(),
        _ => return,
    };
    if let Some(loc) = find_str_loc(node, &name, ctx) {
        ctx.name_ref_locs.insert(name, loc);
    }
}

/// Recursively search `node`'s subtree for an `ast::Str` whose value equals
/// `name`, returning its source range as a [`SourceLoc`].
fn find_str_loc(node: &LinkedNode, name: &str, ctx: &ParseCtx) -> Option<SourceLoc> {
    if let Some(s) = node.get().cast::<ast::Str>() {
        if s.get() == name {
            return Some(ctx.loc(node.range()));
        }
    }
    for child in node.children() {
        if let Some(loc) = find_str_loc(&child, name, ctx) {
            return Some(loc);
        }
    }
    None
}

/// `mobject(label, body)`: register `items[label] = body` (raw source) with a
/// default frame-0 state (opacity 1). Position is left to the renderer.
fn process_mobject(
    pos: &[Expr],
    named: &std::collections::HashMap<String, Expr>,
    node: &LinkedNode,
    raw: &str,
    ctx: &mut ParseCtx,
) {
    let label_expr = pos.first().or_else(|| named.get("label"));
    let Some(label_str) = label_expr.and_then(|e| expr_to_key(e)) else {
        if let Some(e) = label_expr {
            ctx.pending_error = Some(CandyError::InvalidKey {
                what: "mobject label".into(),
                value: expr_key_desc(e),
                not_ident: false,
                loc: Some(ctx.loc(range_of(node, e.to_untyped()).unwrap_or_else(|| node.range()))),
            });
        }
        return;
    };
    if !is_valid_typst_ident(&label_str) {
        // `label_expr` is `Some` here (we just extracted `label_str` from it).
        if let Some(e) = label_expr {
            ctx.pending_error = Some(CandyError::InvalidKey {
                what: "mobject label".into(),
                value: label_str.clone(),
                not_ident: true,
                loc: Some(ctx.loc(range_of(node, e.to_untyped()).unwrap_or_else(|| node.range()))),
            });
        }
        return;
    }
    let body_expr = pos.get(1).or_else(|| named.get("body"));
    let Some(body_expr) = body_expr else { return };
    let body = expr_src(raw, node, body_expr).to_string();
    // Record the body's absolute source range so the per-frame whole-document
    // recompiler (Phase 2) can splice the wrapped body back into the source.
    let body_range = range_of(node, body_expr.to_untyped()).map(|r| (r.start, r.end));

    let label = Label(label_str);
    // Record the declaration's source location so later diagnostics (e.g.
    // `E004` LabelNotFound) can point at the exact code.
    let loc = ctx.loc(node.range());
    ctx.label_locs.insert(label.clone(), loc.clone());
    // Duplicate-name detection (respecting scope): a label redefined in the
    // *same* lexical scope is almost certainly a typo, so warn and let the
    // later definition shadow the earlier (the `insert` below overwrites). A
    // redefinition inside a *nested* scope is legitimate Typst shadowing and
    // must NOT warn.
    let scope = current_scope(ctx);
    if ctx
        .mobject_names
        .entry(scope.clone())
        .or_default()
        .contains(&label.0)
    {
        warn!(CandyWarn::DuplicateName(
            "mobject".into(),
            label.0.clone(),
            loc
        ));
    } else {
        ctx.mobject_names
            .get_mut(&scope)
            .unwrap()
            .insert(label.0.clone());
    }
    ctx.items.insert(label.clone(), body);
    if let Some(r) = body_range {
        ctx.mobject_body_ranges.insert(label.clone(), r);
    }
    register_label(ctx, label.clone(), ctx.current_scene);
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

/// `animate(target, to:, dx:, dy:, scale:, scale-by:, rotate:, rotate-by:,
/// opacity:, duration:, easing:)`.
///
/// The `easing` named argument accepts a string (`"linear"`, `"smooth"`,
/// `"ease-in-out"`, …). Its default is `"smooth"` — matching the `animate`
/// signature declared in the Typst package (`typst/src/core.typ`). Unrecognized
/// names emit a warning to stderr and fall back to `linear`.
fn process_animate(
    pos: &[Expr],
    named: &std::collections::HashMap<String, Expr>,
    _node: &LinkedNode,
    _raw: &str,
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
                    let loc = ctx
                        .current_directive_loc
                        .clone()
                        .unwrap_or_else(|| SourceLoc::at(&ctx.file_path, &ctx.source, 0..0));
                    warn!(CandyWarn::UnknownEasing(
                        format!("'{name}' for @{}", label.0),
                        loc
                    ));
                    Easing::Linear
                }
            }
        }
        // Missing or non-string easing → "smooth" (the Typst `animate` default).
        _ => Easing::Smooth,
    };

    let mut actions = Vec::new();
    // Absolute move: `to: (x, y)`.
    if let Some(to_e) = named.get("to") {
        if let Some((x, y)) = tuple_cm(to_e) {
            actions.push(Action::MoveTo {
                target: label.clone(),
                to: (x, y),
                easing: easing.clone(),
            });
        }
    }
    // Relative move: `dx:` / `dy:` (cm) — the canonical names, matching the
    // `animate` signature declared in the Typst package (`typst/src/core.typ`).
    // The Rust parser must accept exactly the named arguments the Typst
    // signature declares; it does not invent extra aliases.
    let dx = named.get("dx").and_then(expr_to_f64);
    let dy = named.get("dy").and_then(expr_to_f64);
    if dx.is_some() || dy.is_some() {
        actions.push(Action::MoveBy {
            target: label.clone(),
            delta: (dx.unwrap_or(0.0), dy.unwrap_or(0.0)),
            easing: easing.clone(),
        });
    }
    // Absolute scale: `scale: 150%` (a ratio, e.g. `150%` = 1.5×).
    if let Some(s) = ratio_arg(named, "scale", ctx) {
        actions.push(Action::Scale {
            target: label.clone(),
            to: s,
            easing: easing.clone(),
        });
    }
    // Relative scale: `scale-by: 130%` (a ratio; multiplies current scale).
    if let Some(f) = ratio_arg(named, "scale-by", ctx) {
        actions.push(Action::ScaleBy {
            target: label.clone(),
            factor: f,
            easing: easing.clone(),
        });
    }
    // Absolute rotate: `rotate: 90deg` (degrees).
    if let Some(deg) = angle_arg(named, "rotate", ctx) {
        actions.push(Action::Rotate {
            target: label.clone(),
            degrees: deg,
            easing: easing.clone(),
        });
    }
    // Relative rotate: `rotate-by: 15deg` (add to current rotation, degrees).
    if let Some(d) = angle_arg(named, "rotate-by", ctx) {
        actions.push(Action::RotateBy {
            target: label.clone(),
            delta_degrees: d,
            easing: easing.clone(),
        });
    }
    if let Some(o) = ratio_arg(named, "opacity", ctx) {
        actions.push(Action::FadeTo {
            target: label.clone(),
            opacity: o.clamp(0.0, 1.0),
            easing: easing.clone(),
        });
    }
    emit_slide(
        ctx,
        parse_timing(named),
        parse_delay(named),
        duration,
        actions,
    );
}

/// `pause(duration:)` — a no-op hold in standard Typst; a blank slide here.
fn process_pause(named: &std::collections::HashMap<String, Expr>, ctx: &mut ParseCtx) {
    let duration = named
        .get("duration")
        .and_then(expr_to_f64)
        .unwrap_or(500.0)
        .max(1.0) as u32;
    emit_slide(ctx, None, 0, duration, Vec::new());
}

/// `audio(path, blocking:, loop:, volume:, slice:)`.
fn process_audio(
    pos: &[Expr],
    named: &std::collections::HashMap<String, Expr>,
    _node: &LinkedNode,
    _raw: &str,
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
    let slice = named.get("slice").and_then(|e| tuple_cm(e));
    ctx.audio.push(AudioTrack {
        path,
        start_ms: ctx.audio_start(parse_timing(named), parse_delay(named)),
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
    named: &std::collections::HashMap<String, Expr>,
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
    register_label(ctx, label.clone(), ctx.current_scene);
    // Record the body's source range so the whole-document recompiler wraps the
    // `play` block with the per-frame transform (a `play` block is just a
    // synthetic mobject and must be animated/positioned exactly like a real one
    // — without this it renders as inert static `block(body)` and ignores its
    // `FadeIn`/transform).
    if let Some(r) = range_of(node, body_expr.to_untyped()).map(|r| (r.start, r.end)) {
        ctx.mobject_body_ranges.insert(label.clone(), r);
    }
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
    emit_slide(
        ctx,
        None,
        0,
        duration,
        vec![Action::FadeIn {
            target: label.clone(),
            easing: Easing::Linear,
        }],
    );
    // Hide the block at the end of its window so sequential `play` steps don't
    // overlap — the block is only visible during [start, start+duration].
    emit_slide(ctx, None, 0, 1, vec![Action::Hide { target: label }]);
}

/// `save_state(target, slot: "name")` — snapshot the target's current state.
/// Inert under standard Typst. Produces no slide (0-duration); the action is
/// attached to a 1 ms slide at the current cursor so the scheduler sees it.
fn process_save_state(
    pos: &[Expr],
    named: &std::collections::HashMap<String, Expr>,
    ctx: &mut ParseCtx,
) {
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
    // SaveState is instantaneous — emit a 1 ms slide so the scheduler
    // processes the action at the current cursor position.
    emit_slide(
        ctx,
        parse_timing(named),
        parse_delay(named),
        1,
        vec![Action::SaveState {
            target: label,
            slot,
        }],
    );
}

/// `restore(target, slot: "name", duration: 500, easing: "smooth")` —
/// interpolate back to a previously saved state.
fn process_restore(
    pos: &[Expr],
    named: &std::collections::HashMap<String, Expr>,
    ctx: &mut ParseCtx,
) {
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
    let easing = resolve_easing(named, &label, Easing::Smooth, ctx);
    emit_slide(
        ctx,
        parse_timing(named),
        parse_delay(named),
        duration,
        vec![Action::Restore {
            target: label,
            slot,
            easing: easing.clone(),
        }],
    );
}

/// `indicate(target, factor: 110%, dx: 0, dy: 0, duration: 300, easing: "smooth")`
/// — briefly scale + shift, then return to original.
fn process_indicate(
    pos: &[Expr],
    named: &std::collections::HashMap<String, Expr>,
    ctx: &mut ParseCtx,
) {
    let Some(label) = target_arg(pos, named) else {
        return;
    };
    let duration = named
        .get("duration")
        .and_then(expr_to_f64)
        .unwrap_or(300.0)
        .max(1.0) as u32;
    let factor = ratio_arg(named, "factor", ctx).unwrap_or(1.1);
    let dx = named.get("dx").and_then(expr_to_f64).unwrap_or(0.0);
    let dy = named.get("dy").and_then(expr_to_f64).unwrap_or(0.0);
    let easing = resolve_easing(named, &label, Easing::Smooth, ctx);
    emit_slide(
        ctx,
        parse_timing(named),
        parse_delay(named),
        duration,
        vec![Action::Indicate {
            target: label,
            factor,
            dx,
            dy,
            easing: easing.clone(),
        }],
    );
}

/// `flash(target, factor: 200%, duration: 200, easing: "smooth")` —
/// briefly enlarge + fade, then return to original.
fn process_flash(
    pos: &[Expr],
    named: &std::collections::HashMap<String, Expr>,
    ctx: &mut ParseCtx,
) {
    let Some(label) = target_arg(pos, named) else {
        return;
    };
    let duration = named
        .get("duration")
        .and_then(expr_to_f64)
        .unwrap_or(200.0)
        .max(1.0) as u32;
    let factor = ratio_arg(named, "factor", ctx).unwrap_or(2.0);
    let easing = resolve_easing(named, &label, Easing::Smooth, ctx);
    emit_slide(
        ctx,
        parse_timing(named),
        parse_delay(named),
        duration,
        vec![Action::Flash {
            target: label,
            factor,
            easing: easing.clone(),
        }],
    );
}

/// `wiggle(target, degrees: 15deg, duration: 500, easing: "wiggle")` —
/// oscillate rotation, then return to original.
fn process_wiggle(
    pos: &[Expr],
    named: &std::collections::HashMap<String, Expr>,
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
    let degrees = angle_arg(named, "degrees", ctx).unwrap_or(15.0);
    let easing = resolve_easing(named, &label, Easing::Wiggle, ctx);
    emit_slide(
        ctx,
        parse_timing(named),
        parse_delay(named),
        duration,
        vec![Action::Wiggle {
            target: label,
            degrees,
            easing: easing.clone(),
        }],
    );
}

/// `appear(target)` / `disappear(target)` — instantaneous visibility toggle.
/// Emits a 1 ms slide. (`show`/`hide` would conflict with Typst keywords.)
fn process_appear_disappear(
    pos: &[Expr],
    appear: bool,
    named: &std::collections::HashMap<String, Expr>,
    ctx: &mut ParseCtx,
) {
    let Some(label) = target_arg(pos, &std::collections::HashMap::new()) else {
        return;
    };
    let action = if appear {
        Action::Show { target: label }
    } else {
        Action::Hide { target: label }
    };
    emit_slide(
        ctx,
        parse_timing(named),
        parse_delay(named),
        1,
        vec![action],
    );
}

/// `set_color(target, color: black, duration: 1, easing: "linear")` —
/// record a color change; the renderer lerps the mobject's `fill:`/`stroke:`
/// from its current paint to `color` over `duration` along `easing`.
fn process_set_color(
    pos: &[Expr],
    named: &std::collections::HashMap<String, Expr>,
    node: &LinkedNode,
    raw: &str,
    ctx: &mut ParseCtx,
) {
    let Some(label) = target_arg(pos, named) else {
        return;
    };
    // The Typst contract requires a *native* color value (e.g. `red`,
    // `rgb(255,0,0)`, `luma(50)`), not a string. Recover the source text of
    // whatever color expression was passed; legacy string literals are kept
    // verbatim for the Rust-only parse path that skips Typst validation.
    let color = named
        .get("color")
        .map(|e| match e {
            Expr::Str(s) => s.get().to_string(),
            _ => expr_src(raw, node, e).to_string(),
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "black".to_string());
    let duration = named
        .get("duration")
        .and_then(expr_to_f64)
        .unwrap_or(1.0)
        .max(1.0) as u32;
    let easing = resolve_easing(named, &label, Easing::Linear, ctx);
    emit_slide(
        ctx,
        parse_timing(named),
        parse_delay(named),
        duration,
        vec![Action::SetColor {
            target: label,
            color,
            easing: easing.clone(),
        }],
    );
}

/// `blink(target, blinks: 3, duration: 500, easing: "linear")` — alternate
/// opacity 1↔0 N times. Mirrors Manim's `Blink`.
fn process_blink(
    pos: &[Expr],
    named: &std::collections::HashMap<String, Expr>,
    ctx: &mut ParseCtx,
) {
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
    let easing = resolve_easing(named, &label, Easing::Smooth, ctx);
    // Each blink = FadeTo(0) + FadeTo(1).
    let _start = ctx.entry_start(parse_timing(named), parse_delay(named));
    for _ in 0..blinks {
        ctx.slides.push(Slide {
            start_ms: ctx.entry_end,
            duration_ms: per_blink,
            actions: vec![Action::FadeTo {
                target: label.clone(),
                opacity: 0.0,
                easing: easing.clone(),
            }],
            loc: None,
        });
        ctx.entry_advance(per_blink);
        ctx.slides.push(Slide {
            start_ms: ctx.entry_end,
            duration_ms: per_blink,
            actions: vec![Action::FadeTo {
                target: label.clone(),
                opacity: 1.0,
                easing: easing.clone(),
            }],
            loc: None,
        });
        ctx.entry_advance(per_blink);
    }
    ctx.entry_close();
}

/// `spiral_in(target, scale: 300%, rotate: 360deg, duration: 300, easing: "smooth")`
/// — fly in from a scaled-up, rotated state to the flow position, fading in.
/// Mirrors Manim's `SpiralIn`.
fn process_spiral_in(
    pos: &[Expr],
    named: &std::collections::HashMap<String, Expr>,
    ctx: &mut ParseCtx,
) {
    let Some(label) = target_arg(pos, named) else {
        return;
    };
    let scale = ratio_arg(named, "scale", ctx).unwrap_or(3.0);
    let rotate = angle_arg(named, "rotate", ctx).unwrap_or(360.0);
    let duration = named
        .get("duration")
        .and_then(expr_to_f64)
        .unwrap_or(300.0)
        .max(1.0) as u32;
    let easing = resolve_easing(named, &label, Easing::Smooth, ctx);
    let _start = ctx.entry_start(parse_timing(named), parse_delay(named));
    // Set initial state: scaled up, rotated, invisible.
    ctx.slides.push(Slide {
        start_ms: ctx.entry_end,
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
        loc: None,
    });
    ctx.entry_advance(1);
    // Animate to natural state: scale 1, rotate 0, visible.
    ctx.slides.push(Slide {
        start_ms: ctx.entry_end,
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
        loc: None,
    });
    ctx.entry_advance(duration);
    ctx.entry_close();
}

/// `focus_on(target, factor: 1.25, duration: 300, easing: "smooth")` —
/// zoom in (enlarge) onto the target to emphasize it. Implemented as a
/// scale-up on the target. Mirrors Manim's `FocusOn`.
fn process_focus_on(
    pos: &[Expr],
    named: &std::collections::HashMap<String, Expr>,
    ctx: &mut ParseCtx,
) {
    let Some(label) = target_arg(pos, named) else {
        return;
    };
    let factor = ratio_arg(named, "factor", ctx).unwrap_or(1.25);
    let duration = named
        .get("duration")
        .and_then(expr_to_f64)
        .unwrap_or(300.0)
        .max(1.0) as u32;
    let easing = resolve_easing(named, &label, Easing::Smooth, ctx);
    emit_slide(
        ctx,
        parse_timing(named),
        parse_delay(named),
        duration,
        vec![Action::ScaleBy {
            target: label,
            factor,
            easing,
        }],
    );
}

/// `fade_transform(from: "old", to: "new", duration: 300, easing: "smooth")`
/// — crossfade two mobjects: fade out `from` while fading in `to`. Both
/// must be registered via `mobject`. Mirrors Manim's `FadeTransform`.
fn process_fade_transform(
    pos: &[Expr],
    named: &std::collections::HashMap<String, Expr>,
    ctx: &mut ParseCtx,
) {
    // `from` / `to` are positional per the Typst signature
    // (`#fade-transform(from, to, ...)`); accept named as a fallback.
    let from = pos
        .first()
        .or_else(|| named.get("from"))
        .and_then(|e| match e {
            Expr::Str(s) => Some(Label(s.get().to_string())),
            _ => None,
        });
    let to = pos
        .get(1)
        .or_else(|| named.get("to"))
        .and_then(|e| match e {
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
    let easing = resolve_easing(named, &from, Easing::Smooth, ctx);

    // `FadeIn` interpolates the target's *current* opacity → 1. Since a plain
    // mobject starts fully opaque, the crossfade would be a no-op on `to`
    // (rendered at full opacity throughout). Keep `to` hidden from the very
    // start of the timeline unless something already controls it (an earlier
    // `appear`/`animate`/… targeting it) — mirrors Manim's FadeTransform where
    // the incoming object is not on screen before the transform.
    let appeared_earlier = ctx
        .slides
        .iter()
        .any(|s| s.actions.iter().any(|a| a.target() == Some(&to)));
    if !appeared_earlier {
        ctx.slides.push(Slide {
            start_ms: 0,
            duration_ms: 1,
            actions: vec![Action::Hide { target: to.clone() }],
            loc: None,
        });
    }

    let _start = ctx.entry_start(parse_timing(named), parse_delay(named));
    // Force `to` to opacity 0 right before the crossfade (same pattern as
    // `morph`) so the FadeIn below always has a 0 → 1 window, even when the
    // object was made visible earlier.
    ctx.slides.push(Slide {
        start_ms: ctx.entry_end,
        duration_ms: 1,
        actions: vec![Action::Hide { target: to.clone() }],
        loc: None,
    });
    ctx.entry_advance(1);

    // Fade out `from` and fade in `to` in the same slide (parallel).
    ctx.slides.push(Slide {
        start_ms: ctx.entry_end,
        duration_ms: duration,
        actions: vec![
            Action::FadeOut {
                target: from,
                easing: easing.clone(),
            },
            Action::FadeIn { target: to, easing },
        ],
        loc: None,
    });
    ctx.entry_advance(duration);
    ctx.entry_close();
}

/// `move_along_path(target, path, duration: 500, easing: "linear", mode: "polyline", orient: false)`
/// — move the target along a polyline through the given points (cm, absolute).
/// Mirrors Manim's `MoveAlongPath`.
fn process_move_along_path(
    pos: &[Expr],
    named: &std::collections::HashMap<String, Expr>,
    _node: &LinkedNode,
    _raw: &str,
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
    let easing = resolve_easing(named, &label, Easing::Smooth, ctx);

    // The path is the 2nd positional arg per the Typst signature
    // (`#move-along-path(target, path, ...)`), but we also accept a named
    // `path:` for flexibility. Either way it's an array of `(x, y)` tuples (cm).
    let path_e: Option<&Expr> = named.get("path").or_else(|| pos.get(1));
    let points: Vec<(f64, f64)> = match path_e {
        Some(Expr::Array(arr)) => arr
            .items()
            .filter_map(|item| match item {
                ast::ArrayItem::Pos(e) => tuple_cm(&e),
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

    emit_slide(
        ctx,
        parse_timing(named),
        parse_delay(named),
        duration,
        vec![Action::MoveAlongPath {
            target: label,
            points,
            mode,
            orient,
            easing: easing.clone(),
        }],
    );
}

/// `#track(target, ((t, (x, y, scale, opacity, rotation)), ...), duration:,
/// easing:)` — a multi-keyframe timeline for one target. Each keyframe is a
/// tuple `(t_ms, (x, y, scale, opacity, rotation))`; omitted properties carry
/// their previous value forward. `t` is relative to the slide start (ms).
fn process_track(
    pos: &[Expr],
    named: &std::collections::HashMap<String, Expr>,
    ctx: &mut ParseCtx,
) {
    let Some(label) = target_arg(pos, named) else {
        return;
    };
    let duration = named
        .get("duration")
        .and_then(expr_to_f64)
        .unwrap_or(1000.0)
        .max(1.0) as u32;
    let easing = resolve_easing(named, &label, Easing::Smooth, ctx);

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
    emit_slide(
        ctx,
        parse_timing(named),
        parse_delay(named),
        duration,
        vec![Action::Track {
            target: label,
            keyframes,
            easing,
        }],
    );
}

/// `#camera(x:, y:, zoom:, rotate:, duration:, easing:)` — a global pan, zoom,
/// and rotate applied to the whole scene. `rotate` is a clockwise tilt in
/// degrees (e.g. `90deg`). Implemented via a synthetic `__camera__` mobject so
/// it flows through the normal scheduler / interpolator pipeline; the renderer
/// reads it once per frame and never draws it.
fn process_camera(
    _pos: &[Expr],
    named: &std::collections::HashMap<String, Expr>,
    ctx: &mut ParseCtx,
) {
    let duration = named
        .get("duration")
        .and_then(expr_to_f64)
        .unwrap_or(1000.0)
        .max(1.0) as u32;
    let easing = match named.get("easing") {
        Some(Expr::Str(s)) => Easing::from_str(s.get().as_str()).unwrap_or(Easing::Linear),
        _ => Easing::Smooth,
    };
    let x = named.get("x").and_then(expr_to_f64).unwrap_or(0.0);
    let y = named.get("y").and_then(expr_to_f64).unwrap_or(0.0);
    let zoom = ratio_arg(named, "zoom", ctx).unwrap_or(1.0).max(1e-3);
    let rotate = angle_arg(named, "rotate", ctx).unwrap_or(0.0);

    let cam = Label("__camera__".into());
    register_synthetic_mobject(ctx, &cam, "none");
    emit_slide(
        ctx,
        None,
        0,
        duration,
        vec![Action::Camera {
            target: cam,
            x,
            y,
            zoom,
            rotate,
            easing,
        }],
    );
}

/// `#group(name, ("child1", "child2", ...))` — declare `name` as a synthetic
/// mobject that owns the listed children. A group is just a special kind of
/// mobject: an mobject may own child mobjects, and animating the parent
/// (`#animate(name, ...)`) moves / rotates / scales all of its children together
/// via parent→child transform inheritance. The renderer treats the group label
/// like any other mobject (it is registered in `items` / `initial` / scene
/// ownership) but never draws the empty parent body itself — only the children
/// are painted. Groups may be nested (a child may itself be a group).
fn process_group(
    pos: &[Expr],
    named: &std::collections::HashMap<String, Expr>,
    ctx: &mut ParseCtx,
) {
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
fn process_reveal(
    pos: &[Expr],
    named: &std::collections::HashMap<String, Expr>,
    sym: &str,
    ctx: &mut ParseCtx,
) {
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
    let _ = resolve_easing(named, &label, Easing::Smooth, ctx);

    // The body must be a string literal ("...") for char/word reveal.
    let Some(body) = ctx.items.get(&label) else {
        return;
    };
    let Some(inner) = strip_string_literal(body) else {
        let loc = ctx
            .current_directive_loc
            .clone()
            .unwrap_or_else(|| SourceLoc::at(&ctx.file_path, &ctx.source, 0..0));
        warn!(CandyWarn::RevealFallback(format!("@{0}", label.0), loc));
        emit_slide(
            ctx,
            parse_timing(named),
            parse_delay(named),
            duration,
            vec![Action::FadeIn {
                target: label,
                easing: Easing::Linear,
            }],
        );
        return;
    };

    let chunks: Vec<String> = if by == "word" {
        inner.split_whitespace().map(|s| s.to_string()).collect()
    } else {
        inner.chars().map(|c| c.to_string()).collect()
    };
    let n = chunks.len().max(1);
    let step = (duration as f64 / n as f64).ceil().max(1.0) as u32;
    let start = ctx.entry_start(parse_timing(named), parse_delay(named));

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
        .any(|s| s.actions.iter().any(|a| a.target() == Some(&label)));
    if !controlled_earlier && !appeared_earlier && start > 0 {
        tl.insert(0, (0, "none".to_string()));
    }

    ctx.slides.push(Slide {
        start_ms: start,
        duration_ms: duration,
        actions: vec![],
        loc: None,
    });
    ctx.entry_advance(duration);
    ctx.entry_close();
}

/// Register a synthetic mobject (e.g. the camera or a group parent) with an
/// empty body, without overwriting an existing one.
fn register_synthetic_mobject(ctx: &mut ParseCtx, label: &Label, body: &str) {
    if !ctx.items.contains_key(label) {
        ctx.items.insert(label.clone(), body.to_string());
        register_label(ctx, label.clone(), ctx.current_scene);
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

/// `morph(from, to, duration: 500, easing: "smooth")` — crossfade + scale
/// transform from one mobject to another. The `from` object shrinks and fades
/// out while the `to` object grows and fades in. Both must be registered via
/// `mobject`.
fn process_morph(
    pos: &[Expr],
    named: &std::collections::HashMap<String, Expr>,
    ctx: &mut ParseCtx,
) {
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
        .unwrap_or(500.0)
        .max(1.0) as u32;
    let easing = resolve_easing(named, &from, Easing::Smooth, ctx);

    let _start = ctx.entry_start(parse_timing(named), parse_delay(named));
    // Hide the `to` object initially (it will fade in as the shape morphs in).
    ctx.slides.push(Slide {
        start_ms: ctx.entry_end,
        duration_ms: 1,
        actions: vec![Action::Hide { target: to.clone() }],
        loc: None,
    });
    ctx.entry_advance(1);

    // The shape morph itself is rendered by the renderer (a `MorphPlan`
    // precomputed from the two bodies' outlines). Here we only drive the
    // *opacity* crossfade so `from` fades/shrinks out while `to` fades in.
    let start_ms = ctx.entry_end;
    let end_ms = start_ms + duration;
    ctx.slides.push(Slide {
        start_ms,
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
        loc: None,
    });
    ctx.morph_pairs.push(crate::core::ast::MorphPair {
        from: from.clone(),
        to: to.clone(),
        to_body: None,
        start_ms,
        end_ms,
        easing,
    });
    ctx.entry_advance(duration);
    ctx.entry_close();
}

/// Whether a mobject body is *inline content* (a formula or plain text) that
/// can be split into independent glyph fragments for a Manim-style `Transform`.
/// Returns `false` for shape constructors (`circle(…)`, `rect(…)`, …) — those
/// keep the outline-blob morph instead.
fn is_inline_content(body: &str) -> bool {
    let b = body.trim();
    let inner = b
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(b)
        .trim();
    // Math mode is always inline content.
    if inner.starts_with('$') {
        return true;
    }
    // Shape constructors are NOT inline content → keep the blob morph.
    for kw in [
        "circle(",
        "rect(",
        "ellipse(",
        "square(",
        "triangle(",
        "polygon(",
        "line(",
        "path(",
        "arrow(",
        "arc(",
        "image(",
    ] {
        if inner.contains(kw) {
            return false;
        }
    }
    // Anything else (plain text, unknown call) is treated as inline content.
    true
}

/// `transform(target, to: <content>, duration: 500, easing: "smooth")` —
/// Manim's `Transform` / `ReplacementTransform`: morph a single mobject's
/// content into a new inline `content` (a Typst body). Keeps the **original
/// label** holding the new content afterwards.
fn process_transform(
    pos: &[Expr],
    named: &std::collections::HashMap<String, Expr>,
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
        .unwrap_or(500.0)
        .max(1.0) as u32;
    let easing = resolve_easing(named, &label, Easing::Smooth, ctx);

    // Capture the current content of `target` before we replace it.
    // Capture the *currently displayed* content of `target` before we replace
    // it. `items[label]` keeps the original body (transforms swap content via
    // `content_timeline`, never overwriting `items`), so for a *chained*
    // transform we must read the latest `content_timeline` entry instead —
    // otherwise a second `#transform` would morph from the original body, not
    // the intermediate result just shown.
    let old_body = ctx
        .content_timeline
        .get(&label)
        .and_then(|v| v.last().map(|(_, b)| b.clone()))
        .or_else(|| ctx.items.get(&label).cloned())
        .unwrap_or_default();

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
        let start = ctx.entry_start(parse_timing(named), parse_delay(named));
        ctx.slides.push(Slide {
            start_ms: start,
            duration_ms: duration,
            actions: vec![Action::FadeIn {
                target: label,
                easing: easing.clone(),
            }],
            loc: None,
        });
        ctx.entry_advance(duration);
        ctx.entry_close();
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
    let sid = ctx
        .label_scene
        .get(&label)
        .copied()
        .unwrap_or(ctx.current_scene);
    register_label(ctx, tmp.clone(), sid);
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
    // content. Instead we record a *content switch* on the timeline.
    let start = ctx.entry_start(parse_timing(named), parse_delay(named));
    let switch_at = start + 1;
    ctx.content_timeline
        .entry(label.clone())
        .or_default()
        .push((switch_at, new_body.clone()));

    // Decide between a per-glyph fragment morph (inline content: formulas /
    // text) and the outline blob morph (shapes). The fragment morph is what
    // makes formula transitions look like a real Manim `Transform` — the old
    // equation disassembles glyph-by-glyph and reassembles into the new one —
    // instead of the whole block dissolving at once (the previous "stiff"
    // crossfade) or being replaced by a single largest-outline polygon blob.
    let is_inline = is_inline_content(&old_body) && is_inline_content(&new_body);
    if is_inline {
        // The renderer splits both bodies into glyph fragments and lays them
        // out; `fragments` is filled in by `ensure_flow`. No shape blob.
        ctx.transform_plans.push(crate::core::ast::TransformPlan {
            target: label.clone(),
            old: tmp.clone(),
            old_body: old_body.clone(),
            new_body: new_body.clone(),
            fragments: Vec::new(),
            start_ms: switch_at,
            end_ms: switch_at + duration,
            easing: easing.clone(),
        });
    } else {
        // Real shape morph: precompute a `MorphPlan` between the old content's
        // outline and the new content's outline (the blob).
        ctx.morph_pairs.push(crate::core::ast::MorphPair {
            from: tmp.clone(),
            to: label.clone(),
            to_body: Some(new_body.clone()),
            start_ms: switch_at,
            end_ms: switch_at + duration,
            easing: easing.clone(),
        });
    }

    // Single morph slide: the scheduler's native `Transform` action crossfades
    // `old` out while `target` (now showing `new_body`) fades in.
    ctx.slides.push(Slide {
        start_ms: switch_at,
        duration_ms: duration,
        actions: vec![Action::Transform {
            target: label.clone(),
            old: tmp,
            easing: easing.clone(),
        }],
        loc: None,
    });
    ctx.entry_advance(duration);
    ctx.entry_close();
}

/// `subtitle(body, duration:, position:, easing:)` — register a caption overlay.
fn process_subtitle(
    pos: &[Expr],
    named: &std::collections::HashMap<String, Expr>,
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
    let easing = resolve_easing(named, &Label("subtitle".into()), Easing::Linear, ctx);

    let id = format!("__sub_{}", ctx.subtitle_id);
    ctx.subtitle_id += 1;
    let start_ms = ctx.cursor;
    let end_ms = duration.map(|d| start_ms + d);

    ctx.subtitles.push(Subtitle {
        id: id.clone(),
        scope: current_scope(ctx),
        body,
        start_ms,
        end_ms,
        position,
        easing: easing.clone(),
    });
    // Record the `#subtitle(...)` call's source range (including the leading
    // `#`) so the whole-document recompiler can blank it out of the base
    // document (`#none`). The caption is drawn as a separate, camera-independent
    // overlay; leaving it in the base double-renders it (see `ParseArtifacts`).
    let cr = node.range();
    let mut s = cr.start;
    if s > 0 && raw.as_bytes()[s - 1] == b'#' {
        s -= 1;
    }
    ctx.subtitle_call_ranges.insert(id, (s, cr.end));
}

/// `ecnew(name, seed:, step:, duration:, easing:)` — define a named integer
/// counter. Its default easing is `"linear"` (per the Typst signature in
/// `typst/src/counter.typ`); an explicit `easing:` overrides it.
fn process_ecnew(
    pos: &[Expr],
    named: &std::collections::HashMap<String, Expr>,
    node: &LinkedNode,
    _raw: &str,
    ctx: &mut ParseCtx,
) {
    let name_expr = pos.first().or_else(|| named.get("name"));
    let Some(name) = name_expr.and_then(|e| expr_to_key(e)) else {
        if let Some(e) = name_expr {
            ctx.pending_error = Some(CandyError::InvalidKey {
                what: "easing-counter name".into(),
                value: expr_key_desc(e),
                not_ident: false,
                loc: Some(ctx.loc(range_of(node, e.to_untyped()).unwrap_or_else(|| node.range()))),
            });
        }
        return;
    };
    if !is_valid_typst_ident(&name) {
        if let Some(e) = name_expr {
            ctx.pending_error = Some(CandyError::InvalidKey {
                what: "easing-counter name".into(),
                value: name.clone(),
                not_ident: true,
                loc: Some(ctx.loc(range_of(node, e.to_untyped()).unwrap_or_else(|| node.range()))),
            });
        }
        return;
    }
    let seed = named.get("seed").and_then(expr_to_i64).unwrap_or(0);
    let step = named.get("step").and_then(expr_to_i64).unwrap_or(1);
    let duration_ms = named
        .get("duration")
        .and_then(expr_to_f64)
        .map(|d| d.max(1.0) as u32);
    let easing = resolve_easing(
        named,
        &Label(format!("counter:{name}")),
        Easing::Linear,
        ctx,
    );
    let scope = current_scope(ctx);
    // Record the declaration's source location so later diagnostics can point
    // at the exact code.
    let loc = ctx.loc(node.range());
    ctx.label_locs
        .insert(Label(format!("counter:{name}")), loc.clone());

    // Duplicate-name detection (respecting scope): an ecnew redefined in the
    // *same* lexical scope warns and the later definition shadows the earlier
    // (we replace the prior same-scope `CounterDef` so the new one wins). A
    // redefinition inside a *nested* scope is legitimate Typst shadowing and is
    // resolved at runtime by scope depth, so it must NOT warn.
    let def = CounterDef {
        name: name.clone(),
        scope: scope.clone(),
        seed,
        step,
        duration_ms,
        easing,
        start_ms: ctx.cursor,
    };
    if ctx
        .ecnew_names
        .entry(scope.clone())
        .or_default()
        .contains(&name)
    {
        warn!(CandyWarn::DuplicateName("ecnew".into(), name.clone(), loc));
        if let Some(slot) = ctx
            .counters
            .iter()
            .position(|c| c.name == name && c.scope == scope)
        {
            ctx.counters[slot] = def;
        } else {
            ctx.counters.push(def);
        }
    } else {
        ctx.ecnew_names.get_mut(&scope).unwrap().insert(name);
        ctx.counters.push(def);
    }
}

/// `ecpause(name)` / `ecresume(name)` / `ecdestroy(name)` (and their `kc*`
/// counterparts) — record a lifecycle event on a named counter at the current
/// timeline. `kc` selects the keyframe-counter namespace (`kc_events`) instead
/// of the easing-counter one (`counter_events`); keyframe-counter operations are
/// also scope-restricted — an operation on a name not visible in the current
/// lexical scope is an invalid name and surfaces as `E006 UnknownKey`.
fn process_counter_event(
    pos: &[Expr],
    named: &std::collections::HashMap<String, Expr>,
    node: &LinkedNode,
    _raw: &str,
    ctx: &mut ParseCtx,
    kind: CounterEventKind,
    kc: bool,
) {
    let name_expr = pos.first().or_else(|| named.get("name"));
    let key_what = if kc {
        "keyframe-counter name"
    } else {
        "easing-counter name"
    };
    let Some(name) = name_expr.and_then(|e| expr_to_key(e)) else {
        if let Some(e) = name_expr {
            ctx.pending_error = Some(CandyError::InvalidKey {
                what: key_what.into(),
                value: expr_key_desc(e),
                not_ident: false,
                loc: Some(ctx.loc(range_of(node, e.to_untyped()).unwrap_or_else(|| node.range()))),
            });
        }
        return;
    };
    if !is_valid_typst_ident(&name) {
        if let Some(e) = name_expr {
            ctx.pending_error = Some(CandyError::InvalidKey {
                what: key_what.into(),
                value: name.clone(),
                not_ident: true,
                loc: Some(ctx.loc(range_of(node, e.to_untyped()).unwrap_or_else(|| node.range()))),
            });
        }
        return;
    }
    if kc {
        if !kc_name_visible(ctx, &name) {
            ctx.pending_error = Some(CandyError::UnknownKey(
                "kcnew".to_string(),
                name.clone(),
                ctx.name_ref_locs.get(&name).cloned(),
            ));
            return;
        }
        ctx.kc_events.push(CounterEvent {
            name,
            kind,
            at_ms: ctx.cursor,
        });
    } else {
        ctx.counter_events.push(CounterEvent {
            name,
            kind,
            at_ms: ctx.cursor,
        });
    }
}

/// `kcnew(name, seed: 0, easing: "linear")` — register a keyframe counter.
/// Returns `none` under standard Typst (same as `ecnew`). Mirrors
/// [`process_ecnew`] for scope / duplicate-name handling.
fn process_kcnew(
    pos: &[Expr],
    named: &std::collections::HashMap<String, Expr>,
    node: &LinkedNode,
    _raw: &str,
    ctx: &mut ParseCtx,
) {
    let name_expr = pos.first().or_else(|| named.get("name"));
    let Some(name) = name_expr.and_then(|e| expr_to_key(e)) else {
        if let Some(e) = name_expr {
            ctx.pending_error = Some(CandyError::InvalidKey {
                what: "keyframe-counter name".into(),
                value: expr_key_desc(e),
                not_ident: false,
                loc: Some(ctx.loc(range_of(node, e.to_untyped()).unwrap_or_else(|| node.range()))),
            });
        }
        return;
    };
    if !is_valid_typst_ident(&name) {
        if let Some(e) = name_expr {
            ctx.pending_error = Some(CandyError::InvalidKey {
                what: "keyframe-counter name".into(),
                value: name.clone(),
                not_ident: true,
                loc: Some(ctx.loc(range_of(node, e.to_untyped()).unwrap_or_else(|| node.range()))),
            });
        }
        return;
    }
    let seed = named.get("seed").and_then(expr_to_i64).unwrap_or(0);
    let easing = resolve_easing(named, &Label(format!("kc:{name}")), Easing::Linear, ctx);
    let scope = current_scope(ctx);
    let loc = ctx.loc(node.range());
    ctx.label_locs
        .insert(Label(format!("kc:{name}")), loc.clone());

    let def = KeyframeCounterDef {
        name: name.clone(),
        scope: scope.clone(),
        seed,
        easing,
        start_ms: ctx.cursor,
        keyframes: Vec::new(),
    };
    // Duplicate-name detection (respecting scope), mirroring `process_ecnew`.
    let exists = ctx
        .kcnew_names
        .get(&scope)
        .is_some_and(|set| set.contains(&name));
    if exists {
        warn!(CandyWarn::DuplicateName("kcnew".into(), name.clone(), loc));
        if let Some(slot) = ctx
            .kcdefs
            .iter()
            .position(|c| c.name == name && c.scope == scope)
        {
            ctx.kcdefs[slot] = def;
        } else {
            ctx.kcdefs.push(def);
        }
    } else {
        ctx.kcnew_names
            .entry(scope.clone())
            .or_default()
            .insert(name);
        ctx.kcdefs.push(def);
    }
}

/// `kcpush(name, value, offset: 0, easing: "inherit")` — append a keyframe at
/// the call site's natural timeline position (`ctx.cursor` + `offset`), with
/// `value` as the integer reached there. The `easing` (default `inherit`) shapes
/// the segment starting at this keyframe; `inherit` takes the previous node's
/// effective easing, else the counter-level default. An `offset` that would make
/// the keyframe pierce a neighbouring one is clamped into the valid interval
/// (with a `W017` warning). Operations on a name not visible in the current
/// scope are invalid (`E006 UnknownKey`).
fn process_kcpush(
    pos: &[Expr],
    named: &std::collections::HashMap<String, Expr>,
    node: &LinkedNode,
    _raw: &str,
    ctx: &mut ParseCtx,
) {
    let name_expr = pos.first().or_else(|| named.get("name"));
    let Some(name) = name_expr.and_then(|e| expr_to_key(e)) else {
        if let Some(e) = name_expr {
            ctx.pending_error = Some(CandyError::InvalidKey {
                what: "keyframe-counter name".into(),
                value: expr_key_desc(e),
                not_ident: false,
                loc: Some(ctx.loc(range_of(node, e.to_untyped()).unwrap_or_else(|| node.range()))),
            });
        }
        return;
    };
    // Scope restriction: the target must be visible in the current scope.
    if !kc_name_visible(ctx, &name) {
        ctx.pending_error = Some(CandyError::UnknownKey(
            "kcnew".to_string(),
            name.clone(),
            ctx.name_ref_locs.get(&name).cloned(),
        ));
        return;
    }
    let value = named
        .get("value")
        .or_else(|| pos.get(1))
        .and_then(expr_to_i64);
    let Some(value) = value else { return };
    let offset = named
        .get("offset")
        .or_else(|| pos.get(2))
        .and_then(expr_to_i64)
        .unwrap_or(0);
    let at_ms = (ctx.cursor as i64 + offset).max(0) as u32;

    // Resolve the active (innermost visible) keyframe-counter definition.
    let Some(idx) = active_kcdef_index(ctx, &name) else {
        return;
    };
    // Snapshot the relevant data (immutable) for inherit + neighbour detection.
    let (prev_easing, prev_t, next_t) = {
        let def = &ctx.kcdefs[idx];
        let pe = def
            .keyframes
            .iter()
            .filter(|k| k.at_ms < at_ms)
            .max_by_key(|k| k.at_ms)
            .map(|k| k.easing.clone())
            .unwrap_or(def.easing.clone());
        let insert_at = def
            .keyframes
            .iter()
            .position(|k| k.at_ms > at_ms)
            .unwrap_or(def.keyframes.len());
        let prev_t = if insert_at > 0 {
            Some(def.keyframes[insert_at - 1].at_ms)
        } else {
            None
        };
        let next_t = if insert_at < def.keyframes.len() {
            Some(def.keyframes[insert_at].at_ms)
        } else {
            None
        };
        (pe, prev_t, next_t)
    };
    // Resolve the effective easing (inherit handling).
    let easing = resolve_kc_easing(named, ctx, prev_easing);

    // Pierce-through detection: clamp into the valid interval between neighbours.
    let mut final_at = at_ms;
    let mut pierced = false;
    if let Some(pt) = prev_t {
        if final_at <= pt {
            final_at = pt + 1;
            pierced = true;
        }
    }
    if let Some(nt) = next_t {
        if final_at >= nt {
            final_at = nt.saturating_sub(1);
            pierced = true;
        }
    }
    if pierced {
        let loc = ctx
            .current_directive_loc
            .clone()
            .unwrap_or_else(|| SourceLoc::at(&ctx.file_path, &ctx.source, 0..0));
        warn!(CandyWarn::KeyframeOffsetClamp(
            format!(
                "kcpush '{name}' offset made its keyframe pierce a neighbouring keyframe; \
                 clamped to {final_at}ms (original effective time was {at_ms}ms)"
            ),
            loc
        ));
    }

    // Mutate the target definition's keyframe list (sorted, dedup on collision).
    let kfs = &mut ctx.kcdefs[idx].keyframes;
    if kfs.iter().any(|k| k.at_ms == final_at) {
        // No room between neighbours after clamping: drop the push (warning fired).
        return;
    }
    let insert_at = kfs
        .iter()
        .position(|k| k.at_ms > final_at)
        .unwrap_or(kfs.len());
    kfs.insert(
        insert_at,
        Keyframe {
            at_ms: final_at,
            value,
            easing,
        },
    );
}

/// Resolve the effective easing for a `kcpush`, handling the `inherit` default:
/// `inherit` (or an absent easing) takes `prev_easing` (the previous node's
/// effective easing, or the counter-level default for the first node). An unknown
/// easing name warns (`W009`) and falls back to `prev_easing`.
fn resolve_kc_easing(
    named: &std::collections::HashMap<String, Expr>,
    ctx: &ParseCtx,
    prev_easing: Easing,
) -> Easing {
    let raw = named.get("easing").and_then(|e| match e {
        Expr::Str(s) => Some(s.get().to_string()),
        _ => None,
    });
    match raw {
        None => prev_easing,
        Some(s) if s.trim().eq_ignore_ascii_case("inherit") => prev_easing,
        Some(s) => match Easing::from_str(&s) {
            Some(e) => e,
            None => {
                let loc = ctx
                    .current_directive_loc
                    .clone()
                    .unwrap_or_else(|| SourceLoc::at(&ctx.file_path, &ctx.source, 0..0));
                warn!(CandyWarn::UnknownEasing(format!("'{s}' for @kc"), loc));
                prev_easing
            }
        },
    }
}

/// Whether a keyframe-counter `name` is visible in the current lexical scope
/// (current scope or any ancestor). Mirrors the scope-restriction rule for `kc*`:
/// operating on a name not visible here is an invalid name.
fn kc_name_visible(ctx: &ParseCtx, name: &str) -> bool {
    let mut s = ctx.scope_stack.last().copied();
    while let Some(sid) = s {
        let key = sid.to_string();
        if ctx
            .kcnew_names
            .get(&key)
            .is_some_and(|set| set.contains(name))
        {
            return true;
        }
        s = ctx
            .scopes
            .iter()
            .find(|sc| sc.id == sid)
            .and_then(|sc| sc.parent);
    }
    false
}

/// Index of the active (innermost visible) keyframe-counter definition for
/// `name`, or `None` if no visible definition exists. Walks the scope chain from
/// the current (innermost) scope outward and returns the first match, mirroring
/// the shadowing resolution used at render time by `Scene::kc_value_at`.
fn active_kcdef_index(ctx: &ParseCtx, name: &str) -> Option<usize> {
    let mut s = ctx.scope_stack.last().copied();
    while let Some(sid) = s {
        let key = sid.to_string();
        if let Some(i) = ctx
            .kcdefs
            .iter()
            .position(|c| c.name == name && c.scope == key)
        {
            return Some(i);
        }
        s = ctx
            .scopes
            .iter()
            .find(|sc| sc.id == sid)
            .and_then(|sc| sc.parent);
    }
    None
}

/// `scene-switch(target, duration: 0, easing: "smooth")` — switch to a named
/// scene. This creates a `SceneSwitch` action that the scheduler handles as a
/// timeline jump (the cursor jumps to the target scene's `start_ms`).
///
/// The target scene must have been previously defined via `#scene(name: "foo",
/// ...)`. Anonymous scenes (without a `name:` argument) are auto-assigned
/// UUID-like names and can also be targeted.
fn process_scene_switch(
    _pos: &[Expr],
    named: &std::collections::HashMap<String, Expr>,
    node: &LinkedNode,
    _raw: &str,
    ctx: &mut ParseCtx,
) {
    // Accept `target:` or `name:` as the scene reference.
    let target_expr = named.get("target").or_else(|| named.get("name"));
    let target_loc = target_expr
        .map(|e| ctx.loc(range_of(node, e.to_untyped()).unwrap_or_else(|| node.range())));
    let target = target_expr.and_then(|e| match e {
        Expr::Str(s) => Some(s.get().to_string()),
        _ => None,
    });
    let Some(target) = target else {
        return;
    };
    if let Some(loc) = target_loc {
        ctx.scene_switch_locs.insert(target.clone(), loc);
    }

    let duration = named
        .get("duration")
        .and_then(expr_to_f64)
        .unwrap_or(0.0)
        .max(0.0) as u32;

    let easing = match named.get("easing") {
        Some(Expr::Str(s)) => {
            let name = s.get();
            match Easing::from_str(name.as_str()) {
                Some(e) => e,
                None => Easing::Linear,
            }
        }
        _ => Easing::Smooth,
    };

    // SceneSwitch is instantaneous by default (0 duration). Emit a 1 ms slide
    // so the scheduler sees the action.
    emit_slide(
        ctx,
        None,
        0,
        duration.max(1),
        vec![Action::SceneSwitch {
            target,
            duration_ms: duration,
            easing,
        }],
    );
}
