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
    Action, AudioTrack, CounterDef, CounterEvent, CounterEventKind, FrameData, Label, PathMode,
    Slide, Subtitle, TrackKey,
};
use crate::core::diag::{CandyWarn, SourceLoc};
use crate::core::easing::Easing;
use crate::warn;

use crate::parser::ast_walk::ParseCtx;
use crate::parser::expr::{
    call_symbol, current_scope, expr_src, expr_to_bool, expr_to_f64, expr_to_i64, parse_sub_pos,
    range_of, resolve_easing, strip_string_literal, target_arg, track_key_from_expr, tuple_cm,
};

/// Register `label` as owned by `scene`, recording its first-seen (declaration)
/// position in `label_order` so mobjects can later be laid out / painted in
/// source order. `HashMap` iteration is not stable, so this explicit order is
/// what prevents并列 mobjects from coming out in a scrambled arrangement.
fn register_label(ctx: &mut ParseCtx, label: Label, scene: usize) {
    if !ctx.label_scene.contains_key(&label) {
        ctx.label_order.push(label.clone());
    }
    ctx.label_scene.insert(label, scene);
}

/// Resolve and dispatch a single Candy function call.
pub(crate) fn process_call(call: ast::FuncCall, node: &LinkedNode, raw: &str, ctx: &mut ParseCtx) {
    let Some(sym) = call_symbol(&call, ctx) else {
        return;
    };

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
        // Multi-keyframe camera + grouping + text reveal.
        "camera" => process_camera(&pos, &named, ctx),
        "group" => process_group(&pos, &named, ctx),
        "reveal" | "typewriter" => process_reveal(&pos, &named, sym.as_str(), ctx),
        // Subtitle + easing-counter modules.
        "subtitle" => process_subtitle(&pos, &named, node, raw, ctx),
        "ecounter" => process_ecounter(&pos, &named, node, raw, ctx),
        "ecval" => { /* read; value substituted per-frame by the renderer */ }
        "counter-pause" => process_counter_event(&pos, &named, ctx, CounterEventKind::Pause),
        "counter-resume" => process_counter_event(&pos, &named, ctx, CounterEventKind::Resume),
        "counter-destroy" => process_counter_event(&pos, &named, ctx, CounterEventKind::Destroy),
        _ => {}
    }
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
    // Record the body's absolute source range so the per-frame whole-document
    // recompiler (Phase 2) can splice the wrapped body back into the source.
    let body_range = range_of(node, body_expr.to_untyped()).map(|r| (r.start, r.end));

    let label = Label(label_str);
    // Record the declaration's source location so later diagnostics (e.g.
    // `E004` LabelNotFound) can point at the exact code.
    let loc = SourceLoc::at(&ctx.file_path, raw, node.range());
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

/// `animate(target, to:, scale:, opacity:, duration:, easing:)`.
///
/// The `easing` named argument accepts a string (`"linear"`, `"smooth"`,
/// `"ease-in-out"`, …) and falls back to `Easing::Linear` if missing or
/// unrecognized. Unrecognized names emit a warning to stderr and continue.
fn process_animate(
    pos: &[Expr],
    named: &std::collections::HashMap<String, Expr>,
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
                    warn!(CandyWarn::UnknownEasing(format!(
                        "'{name}' for @{}",
                        label.0
                    )));
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
fn process_pause(named: &std::collections::HashMap<String, Expr>, ctx: &mut ParseCtx) {
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
    named: &std::collections::HashMap<String, Expr>,
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
    ctx.slides.push(Slide {
        duration_ms: duration,
        actions: vec![Action::FadeIn {
            target: label.clone(),
            easing: Easing::Linear,
        }],
    });
    ctx.cursor += duration;
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
/// Emits a 1 ms slide. (`show`/`hide` would conflict with Typst keywords.)
fn process_appear_disappear(pos: &[Expr], appear: bool, ctx: &mut ParseCtx) {
    let Some(label) = target_arg(pos, &std::collections::HashMap::new()) else {
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
fn process_set_color(
    pos: &[Expr],
    named: &std::collections::HashMap<String, Expr>,
    ctx: &mut ParseCtx,
) {
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

/// `spiral_in(target, scale: 3.0, rotate: 360, duration: 300, easing: "smooth")`
/// — fly in from a scaled-up, rotated state to the natural position, fading in.
/// Mirrors Manim's `SpiralIn`.
fn process_spiral_in(
    pos: &[Expr],
    named: &std::collections::HashMap<String, Expr>,
    ctx: &mut ParseCtx,
) {
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
    ctx.cursor += duration;
}

/// `focus_on(target, factor: 0.5, duration: 300, easing: "smooth")` —
/// shrink a "spotlight" onto the target. Implemented as a scale-down + fade
/// on the target. Mirrors Manim's `FocusOn`.
fn process_focus_on(
    pos: &[Expr],
    named: &std::collections::HashMap<String, Expr>,
    ctx: &mut ParseCtx,
) {
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
/// must be registered via `mobject`. Mirrors Manim's `FadeTransform`.
fn process_fade_transform(
    _pos: &[Expr],
    named: &std::collections::HashMap<String, Expr>,
    ctx: &mut ParseCtx,
) {
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
    named: &std::collections::HashMap<String, Expr>,
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

/// `#camera(x:, y:, zoom:, rotate:, duration:, easing:)` — a global pan, zoom,
/// and rotate applied to the whole scene. Implemented via a synthetic
/// `__camera__` mobject so it flows through the normal scheduler / interpolator
/// pipeline; the renderer reads it once per frame and never draws it.
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
    let _ = resolve_easing(named, &label);

    // The body must be a string literal ("...") for char/word reveal.
    let Some(body) = ctx.items.get(&label) else {
        return;
    };
    let Some(inner) = strip_string_literal(body) else {
        warn!(CandyWarn::RevealFallback(format!("@{0}", label.0)));
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

/// `morph(from, to, duration: 24, easing: "smooth")` — crossfade + scale
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
    // *opacity* crossfade so `from` fades/shrinks out while `to` fades in.
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
    ctx.cursor += duration;
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

/// `transform(target, to: <content>, duration: 24, easing: "smooth")` —
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
        .unwrap_or(24.0)
        .max(1.0) as u32;
    let easing = resolve_easing(named, &label);

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
    let switch_at = ctx.cursor + 1;
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
        // out; `fragments` is filled in by `ensure_natural`. No shape blob.
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
        duration_ms: duration,
        actions: vec![Action::Transform {
            target: label.clone(),
            old: tmp,
            easing: easing.clone(),
        }],
    });
    ctx.cursor += duration;
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
    let easing = resolve_easing(named, &Label("subtitle".into()));

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

/// `ecounter(name, seed:, step:, duration:, easing:)` — define a named integer
/// counter.
fn process_ecounter(
    pos: &[Expr],
    named: &std::collections::HashMap<String, Expr>,
    node: &LinkedNode,
    raw: &str,
    ctx: &mut ParseCtx,
) {
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
    let scope = current_scope(ctx);
    // Record the declaration's source location so later diagnostics can point
    // at the exact code.
    let loc = SourceLoc::at(&ctx.file_path, raw, node.range());
    ctx.label_locs
        .insert(Label(format!("counter:{name}")), loc.clone());

    // Duplicate-name detection (respecting scope): an ecounter redefined in the
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
        .ecounter_names
        .entry(scope.clone())
        .or_default()
        .contains(&name)
    {
        warn!(CandyWarn::DuplicateName(
            "ecounter".into(),
            name.clone(),
            loc
        ));
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
        ctx.ecounter_names.get_mut(&scope).unwrap().insert(name);
        ctx.counters.push(def);
    }
}

/// `counter_pause(name)` / `counter_resume(name)` / `counter_destroy(name)` —
/// record a lifecycle event on a named counter at the current timeline.
fn process_counter_event(
    pos: &[Expr],
    named: &std::collections::HashMap<String, Expr>,
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
