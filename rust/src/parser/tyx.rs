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
        private_metadata: private,
    };
    scene.validate().map_err(CandyError::Parse)?; // E002
    Ok(scene)
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
}

/// Recursively walk the syntax tree.
fn walk(node: &LinkedNode, raw: &str, ctx: &mut ParseCtx) {
    if let Some(imp) = node.get().cast::<ast::ModuleImport>() {
        process_import(imp, ctx);
    } else if let Some(call) = node.get().cast::<ast::FuncCall>() {
        process_call(call, node, raw, ctx);
    }
    for child in node.children() {
        walk(&child, raw, ctx);
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
            frame_idx: 0,
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
        duration_frames: duration,
        actions,
    });
    ctx.cursor += duration;
}

/// `pause(duration:)` — a no-op hold in standard Typst; a blank slide here.
fn process_pause(named: &HashMap<String, Expr>, ctx: &mut ParseCtx) {
    let duration = named
        .get("duration")
        .and_then(expr_to_f64)
        .unwrap_or(15.0)
        .max(1.0) as u32;
    ctx.slides.push(Slide {
        duration_frames: duration,
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
        start_frame: ctx.cursor,
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
            frame_idx: 0,
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
        duration_frames: duration,
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
        duration_frames: 1,
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
        duration_frames: duration,
        actions: vec![Action::Restore { target: label, slot, easing }],
    });
    ctx.cursor += duration;
}

/// `indicate(target, factor: 1.1, dx: 0, dy: 0, duration: 24, easing: "smooth")`
/// — briefly scale + shift, then return to original.
fn process_indicate(pos: &[Expr], named: &HashMap<String, Expr>, ctx: &mut ParseCtx) {
    let Some(label) = target_arg(pos, named) else { return };
    let duration = named.get("duration").and_then(expr_to_f64).unwrap_or(24.0).max(1.0) as u32;
    let factor = named.get("factor").and_then(expr_to_f64).unwrap_or(1.1);
    let dx = named.get("dx").and_then(expr_to_f64).unwrap_or(0.0);
    let dy = named.get("dy").and_then(expr_to_f64).unwrap_or(0.0);
    let easing = resolve_easing(named, &label);
    ctx.slides.push(Slide {
        duration_frames: duration,
        actions: vec![Action::Indicate { target: label, factor, dx, dy, easing }],
    });
    ctx.cursor += duration;
}

/// `flash(target, factor: 2.0, duration: 18, easing: "smooth")` —
/// briefly enlarge + fade, then return to original.
fn process_flash(pos: &[Expr], named: &HashMap<String, Expr>, ctx: &mut ParseCtx) {
    let Some(label) = target_arg(pos, named) else { return };
    let duration = named.get("duration").and_then(expr_to_f64).unwrap_or(18.0).max(1.0) as u32;
    let factor = named.get("factor").and_then(expr_to_f64).unwrap_or(2.0);
    let easing = resolve_easing(named, &label);
    ctx.slides.push(Slide {
        duration_frames: duration,
        actions: vec![Action::Flash { target: label, factor, easing }],
    });
    ctx.cursor += duration;
}

/// `wiggle(target, degrees: 15, duration: 20, easing: "wiggle")` —
/// oscillate rotation, then return to original.
fn process_wiggle(pos: &[Expr], named: &HashMap<String, Expr>, ctx: &mut ParseCtx) {
    let Some(label) = target_arg(pos, named) else { return };
    let duration = named.get("duration").and_then(expr_to_f64).unwrap_or(20.0).max(1.0) as u32;
    let degrees = named.get("degrees").and_then(expr_to_f64).unwrap_or(15.0);
    let easing = resolve_easing(named, &label);
    ctx.slides.push(Slide {
        duration_frames: duration,
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
        duration_frames: 1,
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
    let duration = named.get("duration").and_then(expr_to_f64).unwrap_or(1.0).max(1.0) as u32;
    let easing = resolve_easing(named, &label);
    ctx.slides.push(Slide {
        duration_frames: duration,
        actions: vec![Action::SetColor { target: label, color, easing }],
    });
    ctx.cursor += duration;
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
fn tuple_cm(e: &Expr, raw: &str, node: &LinkedNode) -> Option<(f64, f64)> {
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
        // Re-slice the literal text from the node's source span (units live there).
        let text = match range_of(node, it.to_untyped()) {
            Some(r) => &raw[r],
            None => "",
        };
        vals.push(parse_length_cm(text)?);
    }
    if vals.len() == 2 {
        Some((vals[0], vals[1]))
    } else {
        None
    }
}

/// Parse a length such as `4cm`, `0pt`, `10mm`, `1in` into centimeters.
/// A bare number is treated as centimeters.
fn parse_length_cm(s: &str) -> Option<f64> {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() && !matches!(bytes[i], b'0'..=b'9' | b'.' | b'-' | b'+') {
        i += 1;
    }
    if i >= bytes.len() {
        return None;
    }
    let start = i;
    while i < bytes.len() && matches!(bytes[i], b'0'..=b'9' | b'.' | b'-' | b'+' | b'e' | b'E') {
        i += 1;
    }
    let num: f64 = s[start..i].parse().ok()?;
    let rest = s[i..].trim_start();
    let unit = rest
        .chars()
        .take_while(|c| c.is_alphabetic())
        .collect::<String>()
        .to_lowercase();
    let factor = match unit.as_str() {
        "cm" | "" => 1.0,
        "mm" => 0.1,
        "pt" => 1.0 / 28.346_456_692_913_385,
        "in" => 2.54,
        "px" => 1.0 / 28.346_456_692_913_385 * 72.0 / 96.0,
        _ => 1.0,
    };
    Some(num * factor)
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
        assert_eq!(scene.slides[0].duration_frames, 30);
        assert_eq!(scene.slides[2].duration_frames, 15);
        assert_eq!(scene.audio.len(), 1);
        assert_eq!(scene.audio[0].path, "voice.opus");
        assert_eq!(scene.audio[0].start_frame, 65); // 30 + 20 + 15 (pause)
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
        assert_eq!(scene.slides[0].duration_frames, 25);
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

