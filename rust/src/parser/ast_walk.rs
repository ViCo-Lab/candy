//! Parse a `.tyx` (Typst X-sheet) file into a `Scene` AST — the orchestration
//! layer.
//!
//! The `.tyx` format is **valid standard Typst**: it imports the Candy package
//! and calls plain Candy functions (`mobject`, `animate`, `pause`, `audio`,
//! `play`). This parser is **AST-driven** (built on `typst_syntax`), not a
//! regex scanner: it walks the Typst syntax tree, resolves every call through
//! the file's *imports*, and extracts each directive's arguments from the real
//! expression nodes.
//!
//! Detection is **import-agnostic** for bare identifiers: a call is treated as
//! a Candy directive iff its resolved name matches a Candy symbol that was
//! actually imported. So it works whether the user wrote `#import "candy": *`
//! (then `mobject(...)`), `#import "candy"` (then `candy.mobject(...)`), or
//! renamed an import (`#import "candy": animate as anim`). The binding is what
//! matters, not the literal prefix. See [`crate::parser::expr::call_symbol`]
//! and the directive handlers in [`crate::parser::directives`].

use std::collections::{HashMap, HashSet};
use std::path::Path;

use typst_syntax::ast::{self, Expr};
use typst_syntax::{LinkedNode, parse};

use crate::core::ast::{
    AudioTrack, CounterDef, CounterEvent, FrameData, Label, ParseArtifacts, Scene, SceneInfo,
    Slide, Subtitle,
};
use crate::core::diag::CandyError;
use crate::core::meta::PrivateMeta;

use crate::parser::directives::process_call;
use crate::parser::expr::{CANDY, call_symbol};

/// Centimeters per Typst point. Must match renderer::typst::PT_PER_CM.
pub(crate) const PT_PER_CM: f64 = 28.346_456_692_913_385;

/// Parse `.tyx` file into a `Scene` AST.
///
/// Precondition: `path` exists and is valid UTF-8 (else E001).
/// Postcondition: returns `Ok(Scene)` with validated slides (else E002).
/// `private_metadata` is set to the fixed defaults.
pub fn parse_tyx(path: &Path) -> Result<Scene, CandyError> {
    let raw = std::fs::read_to_string(path)?; // E001 on missing file
    // Parse as standard Typst **markup** — exactly like `typst compile`. A
    // `.tyx` is a valid standard Typst document: it imports the Candy package
    // and calls plain Candy functions; prose, equations, `//` line comments and
    // `#{ … }` code blocks all work natively. Markup mode is the correct
    // interpretation because it preserves the document's natural layout and
    // Z-order, which the per-frame renderer reuses. Critically, markup mode
    // surfaces `#{ … }` blocks as real `ast::CodeBlock` nodes — which drives the
    // lexical shadowing / scope-restore logic in `walk` (see
    // `candy_directive_restored_after_shadow_scope`).
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
    ctx.scenes.push(SceneInfo {
        id: 0,
        parent: None,
        scope: 0,
        page_size: ctx
            .page_size_cm
            .map(|(w, h)| (w * PT_PER_CM, h * PT_PER_CM)),
        bg: None,
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
    // Scenes behave like slides: a scene stays on stage from its `start_ms`
    // until the next sibling scene begins (or until the end of the document,
    // for the last scene). Without this, a scene's interval ends the moment its
    // own animation window ends, so its content would vanish the instant the
    // timeline moved past it — and, worse, when no explicit scene contained the
    // current time the *root* scene became active and every explicit scene's
    // content leaked onto the canvas at once (scenes overlapping each other).
    // Extending each scene to the next sibling keeps scenes mutually exclusive
    // (no overlap) while still letting a scene's content persist for the rest
    // of the timeline. `active_scene_at` already returns the *deepest* enclosing
    // scene, so nested scenes keep hiding their parents.
    let doc_end = ctx.cursor;
    for i in 0..ctx.scenes.len() {
        let (sid, parent, start) = {
            let s = &ctx.scenes[i];
            (s.id, s.parent, s.start_ms)
        };
        let next_start = ctx
            .scenes
            .iter()
            .filter(|o| o.id != sid && o.parent == parent && o.start_ms >= start)
            .map(|o| o.start_ms)
            .min();
        let new_end = next_start.unwrap_or(doc_end);
        if ctx.scenes[i].end_ms < new_end {
            ctx.scenes[i].end_ms = new_end;
        }
    }
    // Attribute every declared label to its owning scene in *declaration*
    // order (`label_order` is recorded the first time each label is registered).
    // This keeps the natural top-to-bottom flow layout and the paint z-order
    // faithful to source order; iterating `label_scene` (a `HashMap`) directly
    // would scramble并列 mobjects on every run.
    for label in &ctx.label_order {
        let sid = ctx.label_scene.get(label).copied().unwrap_or(0);
        if let Some(s) = ctx.scenes.iter_mut().find(|s| s.id == sid) {
            s.owns_labels.push(label.clone());
        }
    }

    // E008: a `.tyx` that never imports the candy package has no root scene to
    // own its static (non-candy) content, so candy cannot render it. This is
    // checked after the full walk so any import style (wildcard / renamed /
    // bare module / published `@preview/candy:<v>`) is recognized.
    if !ctx.candy_imported {
        return Err(CandyError::NoCandyImport(
            "the .tyx does not import the candy package; candy can only render \
             documents that import `@preview/candy` (its static content must be \
             owned by the implicit root scene)"
                .into(),
        ));
    }

    let private = PrivateMeta::default();
    let scene = Scene {
        slides: ctx.slides,
        items: ctx.items,
        content_timeline: ctx.content_timeline,
        morph_pairs: ctx.morph_pairs,
        transform_plans: ctx.transform_plans,
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
        artifacts: ParseArtifacts {
            source: raw,
            mobject_body: ctx.mobject_body_ranges.clone(),
            scene_call: ctx.scene_call_ranges.clone(),
            subtitle_call: ctx.subtitle_call_ranges.clone(),
        },
        private_metadata: private,
    };
    scene.validate().map_err(CandyError::Parse)?; // E002
    Ok(scene)
}

/// Accumulated parse state.
#[derive(Default)]
pub(crate) struct ParseCtx {
    /// local name -> original Candy symbol (resolved through imports).
    pub(crate) symbol_map: HashMap<String, String>,
    /// Candy module alias names (`candy`, `c`, ...) bound by a bare
    /// `#import "candy"` / `#import "candy" as X`. Enables `candy.mobject(...)`
    /// field-access detection while keeping ordinary method calls out.
    pub(crate) candy_aliases: HashSet<String>,
    /// Whether the candy package itself was imported anywhere in the document
    /// (any import style: `#import "candy": *`, `#import "candy": mobject as m`,
    /// `#import "candy"`, or `#import "@preview/candy:<v>"`). Gates the E008
    /// "candy package not imported" error — without it there is no root scene to
    /// own the document's static (non-candy) content.
    pub(crate) candy_imported: bool,
    /// label -> raw body source text.
    pub(crate) items: HashMap<Label, String>,
    /// label -> frame-0 visual state.
    pub(crate) initial: HashMap<Label, FrameData>,
    pub(crate) slides: Vec<Slide>,
    pub(crate) audio: Vec<AudioTrack>,
    pub(crate) cursor: u32,
    pub(crate) block_counter: usize,
    /// Page size in cm, detected from `#set page(width:.., height:..)`.
    pub(crate) page_size_cm: Option<(f64, f64)>,
    /// Top-level `@preview`/package import lines (raw source) to re-inject into
    /// per-object compile snippets so mobject bodies can use external packages.
    pub(crate) imports: Vec<String>,
    /// Per-label content switches recorded by `transform` (`(time_ms, new_body)`).
    pub(crate) content_timeline: HashMap<Label, Vec<(u32, String)>>,
    /// Real shape-morph pairs recorded by `#morph(from, to)`.
    pub(crate) morph_pairs: Vec<crate::core::ast::MorphPair>,
    /// Per-glyph fragment morph plans recorded by `#transform(target, to: …)`
    /// when both bodies are inline content (formulas / text). Empty otherwise.
    pub(crate) transform_plans: Vec<crate::core::ast::TransformPlan>,
    /// Monotonic counter for synthetic `__xf_<label>_<n>` mobjects created by
    /// `transform`, so repeated transforms on the same label don't clash.
    pub(crate) xf_counter: usize,
    /// Lexical Typst scope tracking. `scope_stack` is the current nesting
    /// (top = innermost scope). `next_scope_id` assigns fresh ids. `scope_starts`
    /// records each scope's start `cursor` so the interval `[start, cursor-at-exit]`
    /// can be recorded on scope exit. `scope_symbol_stack` snapshots
    /// `symbol_map` at each code-block entry so a local `let` that shadows a
    /// Candy name can be restored on scope exit (see `walk`).
    pub(crate) scope_stack: Vec<usize>,
    pub(crate) next_scope_id: usize,
    pub(crate) scope_starts: HashMap<usize, u32>,
    pub(crate) scope_symbol_stack: Vec<HashMap<String, String>>,
    /// Subtitle overlays.
    pub(crate) subtitles: Vec<Subtitle>,
    /// Easing counters.
    pub(crate) counters: Vec<CounterDef>,
    pub(crate) counter_events: Vec<CounterEvent>,
    /// Lexical scope intervals (finalized on scope exit / at end of parse).
    pub(crate) scopes: Vec<crate::core::ast::ScopeInfo>,
    /// Nested scene tree (see `SceneInfo`). `current_scene` is the scene that
    /// owns mobjects declared right now; `scene_stack` tracks open scenes.
    pub(crate) scenes: Vec<SceneInfo>,
    /// Parent→child grouping links (`child → parent`), recorded by `#group`.
    pub(crate) groups: HashMap<Label, Label>,
    /// Next fresh scene id (root is `0`, assigned in `parse_tyx`).
    pub(crate) next_scene_id: usize,
    /// Open scene ids (top = innermost active scene).
    pub(crate) scene_stack: Vec<usize>,
    /// The scene that currently owns newly-declared mobjects.
    pub(crate) current_scene: usize,
    /// label -> owning scene id (populated as mobjects are declared).
    pub(crate) label_scene: HashMap<Label, usize>,
    /// Declaration order of every label (mobjects + synthetic `__xf_*`/`__block_*`),
    /// recorded the first time each label is registered. Used to lay out and
    /// paint mobjects in source order — `HashMap` iteration is not stable, so a
    /// deterministic order must be tracked explicitly (otherwise the vertical
    /// arrangement / z-order of并列 mobjects comes out scrambled).
    pub(crate) label_order: Vec<Label>,
    /// Monotonic id for synthetic subtitles.
    pub(crate) subtitle_id: usize,
    /// Source range of each `#mobject(label, body)` call's `body` argument,
    /// keyed by label. Fed into `Scene::artifacts` for the per-frame
    /// whole-document recompiler (Phase 2).
    pub(crate) mobject_body_ranges: HashMap<Label, (usize, usize)>,
    /// Source range of each explicit `#scene(...)` call — the *entire*
    /// `FuncCall` (not just its body), keyed by scene id. Fed into
    /// `Scene::artifacts` so scenes can be gated with `sys.inputs` (only the
    /// active scene emits a page) in the whole-document recompile.
    pub(crate) scene_call_ranges: HashMap<usize, (usize, usize)>,
    /// Source range (including the leading `#`) of each `#subtitle(...)` call,
    /// keyed by the subtitle's generated id. Fed into `Scene::artifacts` so the
    /// whole-document recompiler can blank the caption out of the base document
    /// (it is drawn as a separate, camera-independent overlay).
    pub(crate) subtitle_call_ranges: HashMap<String, (usize, usize)>,
}

/// Recursively walk the syntax tree.
fn walk(node: &LinkedNode, raw: &str, ctx: &mut ParseCtx) {
    // Lexical shadowing: a local `let name = …` / `let f(…) = …` that rebinds a
    // Candy symbol hides the Candy directive for the rest of the enclosing
    // scope, so ordinary user helpers named like Candy directives (e.g.
    // `#let track = …`) are *not* misparsed as Candy pseudo-function calls.
    // The binding is restored when the enclosing code block exits (or stays
    // removed at the top level, which is also correct).
    if let Some(lb) = node.get().cast::<ast::LetBinding>() {
        // A `let name = …` or `let f(…) = …` binding introduces one or more
        // new idents. If any of them shadows a Candy symbol, suspend that
        // symbol for the rest of the enclosing scope.
        for b in lb.kind().bindings() {
            let n = b.as_str();
            if ctx.symbol_map.remove(n).is_some()
                || ctx.symbol_map.remove(&n.replace('_', "-")).is_some()
            {
                // Suspended; will be restored on enclosing scope exit.
            }
        }
        // Capture top-level user-defined helpers (functions / values) so
        // mobject bodies — compiled in *detached* Typst modules — can reference
        // them. A body like `star(white, s: 0.45cm)` only resolves if `star` is
        // re-injected into the per-object compile (otherwise Typst errors with
        // "unknown variable: star"). We take only bindings at the document root
        // (not inside a `#scene` or `{ … }` block), mirroring where `@preview`
        // imports are captured. Candy-named lets are skipped so they don't
        // shadow a real directive in the detached module.
        let is_candy_named = lb.kind().bindings().iter().any(|b| {
            let n = b.as_str();
            CANDY.iter().any(|c| *c == n || *c == n.replace('_', "-"))
        });
        if !is_candy_named && ctx.scene_stack.is_empty() && ctx.scope_stack.len() == 1 {
            let text = format!("#{}", raw[node.range()].trim());
            if !ctx.imports.contains(&text) {
                ctx.imports.push(text);
            }
        }
    }

    // Scene scoping: a `scene` call opens a *nested scene* around its body.
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
            // Raw source of the `bg` argument expression (e.g. `rgb("#05060f")`),
            // if present. Captured as source text because the background is
            // resolved later (by the renderer, against the real Typst library).
            let mut bg_src: Option<String> = None;
            for a in call.args().items() {
                if let ast::Arg::Named(n) = a {
                    if let Some(cm) = collect_named_lengths_here(n.expr()) {
                        match n.name().as_str() {
                            "width" => w_cm = Some(cm),
                            "height" => h_cm = Some(cm),
                            _ => {}
                        }
                    } else if n.name().as_str() == "bg" {
                        // Recover the expression's source text from the
                        // FuncCall's LinkedNode children (the AST `Named` node
                        // only exposes the `Expr`, not its source range).
                        if let Some(args_node) = node
                            .children()
                            .find_map(|c| c.get().cast::<ast::Args>().map(|_| c))
                        {
                            bg_src = args_node.children().find_map(|arg| {
                                arg.get().cast::<ast::Named>().and_then(|nn| {
                                    if nn.name().as_str() == "bg" {
                                        let name = nn.name().as_str();
                                        // Take the value expression, not the
                                        // `bg` name itself (an `Ident` also
                                        // casts to `Expr`, so skip the child
                                        // whose source text equals the name).
                                        arg.children()
                                            .filter_map(|c| c.get().cast::<Expr>().map(|_| c))
                                            .find(|c| raw[c.range()].trim() != name)
                                            .map(|c| raw[c.range()].to_string())
                                    } else {
                                        None
                                    }
                                })
                            });
                        }
                    }
                }
            }
            let page_size = match (w_cm, h_cm) {
                (Some(w), Some(h)) => Some((w * PT_PER_CM, h * PT_PER_CM)),
                _ => None,
            };
            // Capture the *entire* `#scene(...)` call span for Phase 2: the
            // whole-document recompiler gates each scene with
            // `sys.inputs.at("candy:active_scene")` so only the active scene
            // emits a page (keeping every Typst invocation to a single page).
            // Gating the whole call (rather than just its body) is required
            // because `#scene(…)` expands to `page(…)`, which would still emit
            // an (empty) page if only its body were blanked.
            let cr = node.range();
            ctx.scene_call_ranges.insert(id, (cr.start, cr.end));
            let start = ctx.cursor;
            ctx.scenes.push(SceneInfo {
                id,
                parent: Some(parent),
                scope,
                page_size,
                bg: bg_src,
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
    //
    // We also snapshot `symbol_map` here so that any Candy name shadowed by a
    // local `let` inside this block is restored when the block exits.
    let opened_scope: Option<usize> = node.get().cast::<ast::CodeBlock>().map(|_| {
        let id = ctx.next_scope_id;
        ctx.next_scope_id += 1;
        ctx.scope_starts.insert(id, ctx.cursor);
        ctx.scope_stack.push(id);
        ctx.scope_symbol_stack.push(ctx.symbol_map.clone());
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
        // Restore the Candy-symbol bindings that were shadowed inside this block.
        if let Some(saved) = ctx.scope_symbol_stack.pop() {
            ctx.symbol_map = saved;
        }
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
    // Detect whether the candy *package* itself is being imported (any style).
    // This gates the E008 "candy package not imported" error: a `.tyx` that
    // never imports `@preview/candy` (or a local `candy` path) has no root
    // scene to own its static content, so candy cannot render it.
    if let Expr::Str(s) = imp.source() {
        let src = s.get();
        if src == "candy" || src.ends_with("/candy") || src.starts_with("@preview/candy:") {
            ctx.candy_imported = true;
        }
    }
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
        None => {
            // Bare module import (`#import "candy"` / `#import "candy" as c`):
            // the module object itself is bound to a name, enabling
            // `candy.mobject(...)` field-access calls. Record the bound alias so
            // `call_symbol` only treats *that* receiver's Candy fields as Candy.
            if let Expr::Str(s) = imp.source() {
                let src = s.get();
                // Accept the local `candy`, a path `…/candy`, and the published
                // `@preview/candy:<version>` package (so a real `.tyx` that
                // imports the published package as a module resolves too).
                if src == "candy" || src.ends_with("/candy") || src.starts_with("@preview/candy:") {
                    if let Ok(alias) = imp.bare_name() {
                        ctx.candy_aliases.insert(alias.to_string());
                    }
                }
            }
        }
    }
}

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
        if let Some(cm) = collect_named_lengths_here(expr) {
            f(name, cm);
        }
    }
    for child in node.children() {
        collect_named_lengths(&child, f);
    }
}

/// Evaluate a single expression as a length in cm (used by page-size and
/// `scene` width/height extraction). Thin wrapper over [`crate::parser::expr`].
fn collect_named_lengths_here(e: Expr) -> Option<f64> {
    crate::parser::expr::expr_length_cm(&e)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::ast::Action;

    /// Rewrite a test's `#import "candy"` into `#import "@preview/candy:<v>"`,
    /// auto-fetching the published package version from `typst/typst.toml`.
    ///
    /// Project convention: only *test* code needs the Typst package version
    /// auto-fetched (production code must not). Wrapping every test source with
    /// this helper guarantees no test hard-codes a candy version.
    fn with_auto_version(raw: &str) -> String {
        let v = crate::typst_package_version().expect("typst/typst.toml must declare a `version`");
        let pkg = format!("@preview/candy:{v}");
        // `:`-form imports (`#import "candy": *` and renamed imports) keep
        // their explicit bindings, so just rewrite the package path.
        let s = raw.replace("#import \"candy\":", &format!("#import \"{pkg}\":"));
        // Bare module import (`#import "candy"`) must preserve the `candy`
        // binding name so `#candy.mobject(...)` still resolves after the path
        // rewrite — bind it explicitly as `candy`.
        s.replace(
            "#import \"candy\"\n",
            &format!("#import \"{pkg}\" as candy\n"),
        )
    }

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
        std::fs::write(&tmp, with_auto_version(DOT)).unwrap();
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
    fn mobject_declaration_order_is_preserved() {
        //并列 mobjects must keep their source declaration order. The labels are
        // declared `zeta, alpha, mid` (deliberately NOT alphabetical) so a stray
        // `HashMap`-iteration sort is caught. `owns_labels` drives both the
        // natural top-to-bottom layout and the paint z-order.
        let src = with_auto_version(
            r#"
#import "candy": *
#mobject("zeta", text(size: 20pt)[First])
#mobject("alpha", rect(width: 3cm, height: 1cm))
#mobject("mid", text(size: 14pt)[Third])
"#,
        );
        let tmp = std::env::temp_dir().join("candy_test_order.tyx");
        std::fs::write(&tmp, src).unwrap();
        let scene = parse_tyx(&tmp).unwrap();
        std::fs::remove_file(&tmp).ok();

        let root = scene.scenes.iter().find(|s| s.id == 0).expect("root scene");
        assert_eq!(
            root.owns_labels,
            vec![
                Label("zeta".into()),
                Label("alpha".into()),
                Label("mid".into())
            ],
            "mobject declaration order was scrambled"
        );
    }

    #[test]
    fn parses_field_access_import() {
        // candy imported as a module, called via candy.mobject(...)
        let src = with_auto_version(
            r#"
#import "candy"
#candy.mobject("box", rect(width: 2cm, height: 2cm, fill: red))
#candy.animate("box", to: (3cm, 2cm), duration: 20)
"#,
        );
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
        let src = with_auto_version(
            r#"
#import "candy": animate as anim, mobject as mob
#mob("box", rect(width: 2cm, height: 2cm, fill: red))
#anim("box", to: (3cm, 2cm), duration: 20)
#anim("box", scale: 1.5, duration: 20)
"#,
        );
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
        let src = with_auto_version(
            r#"
#import "candy": *
#mobject("a", circle(radius: 1cm))
#play(rect(width: 2cm, height: 1cm, fill: green), duration: 25)
"#,
        );
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

    /// Confirm the shipped `lib.typ` entrypoint is valid standard Typst: it
    /// re-exports every directive from its submodules, and calling them must
    /// compile with the `typst` compiler.
    #[test]
    fn std_typst_api_compiles() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../typst/src");
        let tmp = dir.join("__std_api_check.typ");
        let calls = r#"
#import "lib.typ": *
#mobject("dot", circle(radius: 1cm, fill: blue))
#mobject("box", rect(width: 2cm, height: 2cm, fill: red))
#animate("dot", to: (4cm, 0pt), duration: 30)
#animate("box", rotate: 45, opacity: 0.5, easing: "smooth", duration: 20)
#pause(duration: 15)
#audio("voice.opus", blocking: false, loop: false, volume: 0.9)
#play(circle(radius: 1cm), duration: 10)
"#;
        std::fs::write(&tmp, calls).unwrap();
        let out = crate::renderer::compile_file_for_test(&tmp);
        let _ = std::fs::remove_file(&tmp);
        assert!(out.is_ok(), "std Typst failed to compile: {out:?}");
    }

    /// Verify the new `rotate` and `opacity` (FadeTo) actions parse correctly.
    #[test]
    fn parses_rotate_and_fadeto() {
        let src = with_auto_version(
            r#"
#import "candy": *
#mobject("sq", rect(width: 2cm, height: 2cm))
#animate("sq", rotate: 90, opacity: 0.3, duration: 25, easing: "cubic-in-out")
"#,
        );
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
        let src = with_auto_version(
            r#"
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
"#,
        );
        let tmp = std::env::temp_dir().join("candy_test_manim.tyx");
        std::fs::write(&tmp, src).unwrap();
        let scene = parse_tyx(&tmp).unwrap();
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
    /// the ORIGINAL body, records the content switch, and emits a single
    /// `Transform` slide.
    #[test]
    fn parses_transform() {
        let src = with_auto_version(
            r#"
#import "candy": *
#mobject("eq", [$a + b = c$])
#transform("eq", to: [$a + b + d = c$], duration: 20, easing: "smooth")
#mobject("box", rect(width: 2cm, height: 2cm))
#transform("box", to: circle(radius: 1.5cm, fill: blue))
"#,
        );
        let tmp = std::env::temp_dir().join("candy_test_transform.tyx");
        std::fs::write(&tmp, src).unwrap();
        let scene = parse_tyx(&tmp).unwrap();
        assert_eq!(scene.slides.len(), 2, "slides: {:?}", scene.slides);

        assert_eq!(scene.items[&Label("eq".into())], "[$a + b = c$]");
        assert_eq!(
            scene.items[&Label("box".into())],
            "rect(width: 2cm, height: 2cm)"
        );

        assert_eq!(scene.items[&Label("__xf_eq_0".into())], "[$a + b = c$]");
        assert_eq!(
            scene.items[&Label("__xf_box_1".into())],
            "rect(width: 2cm, height: 2cm)"
        );

        assert_eq!(
            scene.content_timeline[&Label("eq".into())],
            vec![(1u32, "[$a + b + d = c$]".to_string())]
        );
        assert_eq!(
            scene.content_timeline[&Label("box".into())],
            vec![(21u32, "circle(radius: 1.5cm, fill: blue)".to_string())]
        );

        assert!(matches!(
            &scene.slides[0].actions[..],
            [Action::Transform { target, .. }] if target.0 == "eq"
        ));
        assert!(matches!(
            &scene.slides[1].actions[..],
            [Action::Transform { target, .. }] if target.0 == "box"
        ));

        // Inline content (formula) → per-glyph TransformPlan; shape → blob morph.
        assert_eq!(
            scene.transform_plans.len(),
            1,
            "transform_plans: {:?}",
            scene.transform_plans
        );
        assert_eq!(scene.transform_plans[0].target.0, "eq");
        assert_eq!(scene.transform_plans[0].old_body, "[$a + b = c$]");
        assert_eq!(scene.transform_plans[0].new_body, "[$a + b + d = c$]");
        assert_eq!(
            scene.morph_pairs.len(),
            1,
            "morph_pairs: {:?}",
            scene.morph_pairs
        );
        assert_eq!(scene.morph_pairs[0].to.0, "box");
        std::fs::remove_file(&tmp).ok();
    }

    /// Regression: a sequence of `transform`s must NOT accumulate `scale`, and
    /// the parked old-content mobject must INHERIT the target's position.
    #[test]
    fn transform_keeps_scale_bounded_and_inherits_position() {
        let src = with_auto_version(
            r#"
#import "candy": *
#mobject("shape", rect(width: 3cm, height: 3cm, fill: blue))
#animate("shape", to: (5cm, 0cm), duration: 30)
#transform("shape", to: circle(radius: 1.6cm, fill: red), duration: 30)
#transform("shape", to: rect(width: 1cm), duration: 30)
"#,
        );
        let tmp = std::env::temp_dir().join("candy_test_transform_sched.tyx");
        std::fs::write(&tmp, src).unwrap();
        let scene = parse_tyx(&tmp).unwrap();
        let frames = crate::core::scheduler::schedule(&scene).unwrap();

        for f in &frames {
            assert!(f.scale <= 2.0, "scale blew up to {}", f.scale);
            assert!(f.scale >= 1e-4, "scale shrank to {}", f.scale);
        }

        let xf: Vec<&FrameData> = frames
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

    /// Verify the Manim-inspired directives compile as valid standard Typst
    /// (lib.typ re-exports them from `manim.typ`).
    #[test]
    fn std_typst_manim_api_compiles() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../typst/src");
        let tmp = dir.join("__std_manim_api_check.typ");
        let calls = r#"
#import "lib.typ": *
#mobject("dot", circle(radius: 1cm))
#save_state("dot", slot: "home")
#restore("dot", slot: "home", duration: 10, easing: "smooth")
#indicate("dot", factor: 1.2, duration: 12)
#flash("dot", factor: 2.0, duration: 10)
#wiggle("dot", degrees: 10, duration: 14)
#disappear("dot")
#appear("dot")
#set_color("dot", color: "red", duration: 1)
"#;
        std::fs::write(&tmp, calls).unwrap();
        let out = crate::renderer::compile_file_for_test(&tmp);
        let _ = std::fs::remove_file(&tmp);
        assert!(
            out.is_ok(),
            "std Typst failed to compile manim API: {out:?}"
        );
    }

    /// Verify nested `#scene` calls build a scene tree.
    #[test]
    fn parses_nested_scenes() {
        let src = with_auto_version(
            r#"
#import "candy": *
#scene(width: 16cm, height: 9cm)[
  #mobject("a", circle(radius: 1cm))
  #animate("a", to: (4cm, 0pt), duration: 30)
  #scene(width: 10cm, height: 6cm)[
    #mobject("b", rect(width: 1cm))
    #animate("b", to: (2cm, 0pt), duration: 20)
  ]
]
"#,
        );
        let tmp = std::env::temp_dir().join("candy_test_nested_scene.tyx");
        std::fs::write(&tmp, src).unwrap();
        let scene = parse_tyx(&tmp).unwrap();

        assert_eq!(scene.scenes.len(), 3, "scenes: {:?}", scene.scenes);
        assert_eq!(scene.root_scene, Some(0));

        let owner = scene.label_scene_map();
        assert_eq!(owner[&Label("a".into())], 1, "a → outer scene");
        assert_eq!(owner[&Label("b".into())], 2, "b → inner scene");

        let inner = scene.scenes.iter().find(|s| s.id == 2).unwrap();
        assert_eq!((inner.start_ms, inner.end_ms), (30, 50));
        let outer = scene.scenes.iter().find(|s| s.id == 1).unwrap();
        assert_eq!((outer.start_ms, outer.end_ms), (0, 50));
        assert_eq!(inner.page_size, Some((10.0 * PT_PER_CM, 6.0 * PT_PER_CM)));

        assert_eq!(scene.active_scene_at(10), 1);
        assert_eq!(scene.active_scene_at(40), 2);
        std::fs::remove_file(&tmp).ok();
    }

    /// Regression: sibling `#scene` calls must be *sequential, mutually
    /// exclusive* slides — at any moment exactly one is the active scene, and
    /// the timeline never falls back to the root scene while a sibling covers
    /// it. This is what prevents scenes from all rendering on top of each other
    /// ("scene pollution / overlap"). Each scene's interval is also extended to
    /// the next sibling's start (or the document end) so a scene persists until
    /// replaced.
    #[test]
    fn sibling_scenes_are_sequential_and_mutually_exclusive() {
        let src = with_auto_version(
            r#"
#import "candy": *
#scene(width: 16cm, height: 9cm)[
  #mobject("a", circle(radius: 1cm))
  #pause(duration: 50)
]
#scene(width: 16cm, height: 9cm)[
  #mobject("b", rect(width: 1cm))
  #pause(duration: 50)
]
"#,
        );
        let tmp = std::env::temp_dir().join("candy_test_sibling_scene.tyx");
        std::fs::write(&tmp, src).unwrap();
        let scene = parse_tyx(&tmp).unwrap();

        assert_eq!(
            scene.scenes.len(),
            3,
            "root + 2 siblings: {:?}",
            scene.scenes
        );
        let owner = scene.label_scene_map();
        assert_eq!(owner[&Label("a".into())], 1, "a → scene 1");
        assert_eq!(owner[&Label("b".into())], 2, "b → scene 2");

        let s1 = scene.scenes.iter().find(|s| s.id == 1).unwrap();
        let s2 = scene.scenes.iter().find(|s| s.id == 2).unwrap();
        // Sequential: scene 1 ends exactly where scene 2 begins (no overlap).
        assert_eq!(
            s1.end_ms, s2.start_ms,
            "sibling scenes must not overlap: {:?} {:?}",
            s1, s2
        );
        // During scene 1's window only scene 1 is active (never the root).
        assert_eq!(scene.active_scene_at(10), 1);
        assert_ne!(
            scene.active_scene_at(10),
            0,
            "root must not leak over scene 1"
        );
        // During scene 2's window only scene 2 is active.
        assert_eq!(scene.active_scene_at(60), 2);
        assert_ne!(
            scene.active_scene_at(60),
            0,
            "root must not leak over scene 2"
        );
        // Scene 2 (the last) persists to the document end, not just its content.
        assert_eq!(s2.end_ms, 100, "last scene extends to doc end: {:?}", s2);
        std::fs::remove_file(&tmp).ok();
    }

    // ===================== detection-precision regressions =====================

    /// A field access on a *non-Candy* receiver (`obj.morph`) must NOT be
    /// parsed as a Candy pseudo-function call: it is ordinary user code.
    #[test]
    fn field_access_on_ordinary_object_is_not_candy() {
        let src = with_auto_version(
            r#"
#import "candy": *
#let obj = (morph: 1)
#let helper = obj
#helper.morph()   // method-like call on a user object — NOT candy
#helper.reveal("x")
"#,
        );
        let tmp = std::env::temp_dir().join("candy_test_field_false.tyx");
        std::fs::write(&tmp, src).unwrap();
        let scene = parse_tyx(&tmp).unwrap();
        // No slides or items should be produced by the false-positive calls.
        assert_eq!(scene.slides.len(), 0, "slides: {:?}", scene.slides);
        assert!(!scene.items.contains_key(&Label("x".into())));
        std::fs::remove_file(&tmp).ok();
    }

    /// A user-defined helper that shadows a Candy name (`#let track = …`) must
    /// not be parsed as the `track` directive inside its scope.
    #[test]
    fn local_let_shadowing_hides_candy_directive() {
        let src = with_auto_version(
            r#"
#import "candy": *
#let track(n) = n
// Inside this `#{ … }` code block, `track` is the user's function, not candy's
// keyframe `track`. The call below must NOT produce a Track slide.
#{
  #track(5)
}
"#,
        );
        let tmp = std::env::temp_dir().join("candy_test_shadow.tyx");
        std::fs::write(&tmp, src).unwrap();
        let scene = parse_tyx(&tmp).unwrap();
        assert_eq!(scene.slides.len(), 0, "slides: {:?}", scene.slides);
        std::fs::remove_file(&tmp).ok();
    }

    /// A Candy directive shadowed *inside* a block is restored once the block
    /// exits, so the real Candy `track` works again at the top level.
    #[test]
    fn candy_directive_restored_after_shadow_scope() {
        let src = with_auto_version(
            r#"
#import "candy": *
#{
  #let track(n) = n
  #track(5)   // user's `track` inside the block — not candy
}
#mobject("a", circle(radius: 1cm))
#track("a", ((0, (1cm, 0cm, 1, 1, 0)),), duration: 10)   // real candy track (nested-tuple keys)
"#,
        );
        let tmp = std::env::temp_dir().join("candy_test_shadow_restore.tyx");
        std::fs::write(&tmp, src).unwrap();
        let scene = parse_tyx(&tmp).unwrap();
        assert_eq!(scene.slides.len(), 1, "slides: {:?}", scene.slides);
        assert!(matches!(scene.slides[0].actions[0], Action::Track { .. }));
        std::fs::remove_file(&tmp).ok();
    }

    /// Every candy import in test code must use the auto-fetched `@preview/candy`
    /// version — never a hard-coded one. This proves `with_auto_version` rewrites
    /// `#import "candy"` into the versioned published path.
    /// A `.tyx` that uses candy-style calls but never imports the candy package
    /// must be rejected with the dedicated E008 (not parsed as an empty scene,
    /// not panicked, not silently rendered).
    #[test]
    fn no_candy_import_is_e008() {
        let src = r#"
#mobject("a", circle(radius: 1cm, fill: blue))
#animate("a", to: (4cm, 0pt), duration: 30)
"#;
        let tmp = std::env::temp_dir().join("candy_test_no_import.tyx");
        std::fs::write(&tmp, src).unwrap();
        let err = parse_tyx(&tmp).unwrap_err();
        std::fs::remove_file(&tmp).ok();
        assert_eq!(err.code(), "E008", "expected E008, got {err:?}");
    }

    #[test]
    fn test_candy_imports_use_auto_fetched_version() {
        let src = with_auto_version(
            r#"
#import "candy": *
#mobject("a", circle(radius: 1cm))
"#,
        );
        let v = crate::typst_package_version().expect("typst/typst.toml version");
        assert!(
            src.contains(&format!("@preview/candy:{v}")),
            "test import must use the auto-fetched version `@preview/candy:{v}`: {src}"
        );
        assert!(
            !src.contains("#import \"candy\""),
            "test import must not retain the bare `candy` path: {src}"
        );
    }

    /// Standard Typst markup supports `//` line comments natively, so a `.tyx`
    /// that mixes prose, `//` comments and candy directives parses exactly like
    /// `typst compile` — no special code/markup mode switching.
    #[test]
    fn markup_supports_slash_comments_natively() {
        let src = with_auto_version(
            r#"
#import "candy": *
= Heading

Some prose with an equation $a + b = c$ and a URL https://example.com.
// a line comment — valid in standard markup mode
#mobject("dot", circle(radius: 1cm, fill: blue))
#animate("dot", to: (4cm, 0pt), duration: 30)
"#,
        );
        let tmp = std::env::temp_dir().join("candy_test_markup_comments.tyx");
        std::fs::write(&tmp, src).unwrap();
        let scene = parse_tyx(&tmp).unwrap();
        std::fs::remove_file(&tmp).ok();
        assert!(scene.items.contains_key(&Label("dot".into())));
    }
}
