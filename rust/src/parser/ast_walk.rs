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
use std::path::{Path, PathBuf};

use typst_syntax::ast::{self, Expr};
use typst_syntax::{LinkedNode, parse};

use crate::core::ast::{
    Action, AudioTrack, CounterDef, CounterEvent, FrameData, IncludeRegion, KeyframeCounterDef,
    Label, ParseArtifacts, Scene, SceneInfo, Slide, SourceMap, Subtitle, Timing,
};
use crate::core::diag::{CandyError, CandyWarn, SourceLoc};
use crate::core::meta::PrivateMeta;
use crate::warn;

use crate::parser::directives::process_call;
use crate::parser::expr::{CANDY, call_symbol, is_valid_typst_ident};

/// Typst points per centimeter — re-exported from core::ast for convenience.
use crate::core::ast::PT_PER_CM;

/// Maximum recursive `include` depth before we treat it as a cycle and error
/// (prevents an `a → b → a` loop from expanding forever).
const MAX_INCLUDE_DEPTH: usize = 64;

/// Recursively expand every `#include "path"` statement in the file at `file_path`,
/// inlining the referenced file's content at the call site, so candy's AST
/// parser sees *all* directives — including those declared inside included
/// `.tyx` / `.typ` files (and the files *they* include) — as one flat
/// document.
///
/// Without this, included files would be resolved by Typst's built-in `include`
/// only at *compile* time (inside the World), long after candy's parse has
/// run: directives inside an included file would never be parsed (no animation,
/// no `#mobject` capture, no scene gating). Expanding up front keeps
/// `ParseArtifacts::source` (the string the renderer text-splices against)
/// consistent with what candy actually parsed.
///
/// Expansion is AST-driven (not regex): only real `#include "literal"` calls
/// are touched, so an `include` inside a string or comment is left alone.
/// Each included file is expanded in the context of *its own* directory
/// (matching Typst's path resolution) and expanded fully *before* being
/// spliced, so a chain `a → b → c` collapses in one pass. A depth cap
/// guards against a cyclic include expanding forever.
///
/// Returns the expanded source **and** a [`SourceMap`]: every byte range that
/// came from an inlined include is recorded together with the `#include(…)`
/// call-site that pulled it in and the included file itself, so later diagnostics
/// can point at the *actual* error inside the included file and walk up the
/// include chain, instead of pointing at a meaningless offset in the concatenated
/// document or collapsing to the includer's `#include` line.
///
/// `chain` is the stack of *include-call* records from the root down to (but not
/// including) the file currently being expanded. Each record is
/// `(canonical_target, includer_path, includer_source, include_call_range)`:
/// `canonical_target` is the canonicalized path of the included file (used for the
/// single-chain cycle guard), and the rest describe the *includer's*
/// `#include "target"` call (its file, its own unexpanded source, and the byte
/// range of the call) so a diagnostic can be expanded into a layer-by-layer
/// "included from …" trace that walks the full include path back to the root.
fn expand_includes(
    source: &str,
    file_path: &Path,
    depth: usize,
    chain: &mut Vec<(PathBuf, PathBuf, String, std::ops::Range<usize>)>,
) -> Result<(String, Vec<IncludeRegion>), CandyError> {
    if depth > MAX_INCLUDE_DEPTH {
        return Err(CandyError::Parse(
            "include nesting too deep (check for a cyclic include)".into(),
            None,
        ));
    }
    let root = parse(source);
    let node = LinkedNode::new(&root);
    // `(call_start, call_end, expanded_content, included_path, nested_regions)`:
    // the whole `#include "…"` call span, the (already-recursively-expanded)
    // content that replaces it, the absolute path of the included file, plus the
    // regions of *its* included children (in the child's own coordinate space,
    // shifted into place when spliced).
    let mut edits: Vec<(usize, usize, String, PathBuf, Vec<IncludeRegion>)> = Vec::new();
    collect_include_edits(&node, file_path, depth, source, &mut edits, chain)?;
    if edits.is_empty() {
        return Ok((source.to_string(), Vec::new()));
    }
    // Apply right-to-left (descending start) so earlier offsets stay valid.
    edits.sort_by_key(|e| std::cmp::Reverse(e.0));
    let mut out = source.to_string();
    let mut regions: Vec<IncludeRegion> = Vec::new();
    for (start, end, rep, inc_path, nested) in edits {
        out.replace_range(start..end, &rep);
        // The inlined content of *this* include call occupies
        // `[start, start + rep.len())` in the output and equals `rep` exactly.
        // `inc_path`/`inc_raw` let the deepest trace frame point at the real
        // error *inside* the included file (not the includer's `#include` line).
        regions.push(IncludeRegion {
            start,
            end: start + rep.len(),
            ref_path: file_path.to_path_buf(),
            ref_raw: source.to_string(),
            ref_range: start..end,
            inc_path: inc_path.clone(),
            inc_raw: rep.clone(),
            chain: vec![(file_path.to_path_buf(), source.to_string(), start..end)],
        });
        // Nested regions live *inside* this inlined content, so shift them by
        // `start` to land in the output coordinate space. Prepend this include's
        // call-site to each nested region's chain so a diagnostic can walk the
        // full include path (root → … → immediate includer) layer by layer.
        // Keep each nested region's own `inc_path`/`inc_raw` (they already point
        // at the deeper included file where the real error lives).
        for n in nested {
            let mut chain = vec![(file_path.to_path_buf(), source.to_string(), start..end)];
            chain.extend(n.chain);
            regions.push(IncludeRegion {
                start: n.start + start,
                end: n.end + start,
                ref_path: n.ref_path,
                ref_raw: n.ref_raw,
                ref_range: n.ref_range,
                inc_path: n.inc_path,
                inc_raw: n.inc_raw,
                chain,
            });
        }
    }
    Ok((out, regions))
}

/// Walk `node` (parsed from `source`, the text of the file at `file_path`)
/// for `#include "path"` statements. For each, read the file relative to
/// `file_path`'s directory, recursively expand it, and record an edit that
/// replaces the whole call span with the expanded content (and hands back the
/// nested regions returned by the recursive expansion). We do not descend into
/// a call's children (the entire `include "…"` text is replaced anyway), which
/// also stops the recursive expansion from re-scanning the same call.
///
/// `source` is the text of `file_path`; it is used here to attach a precise
/// [`SourceLoc`] to a circular-include error at the offending `#include` call.
///
/// `chain` is the stack of *include-call* records from the root down to (but not
/// including) `file_path`. *Single-chain* cycle guard: a file may appear at
/// most once along the path from the root to this include. If the target is
/// already on `chain` (i.e. it is an ancestor of itself), the include would
/// recurse forever, so we reject it as a circular include. A duplicate on a
/// *different* branch (a diamond, e.g. `root → a → x` and `root → b → x`)
/// is **not** flagged, matching the rule that only the same include *path*
/// may not repeat a file.
fn collect_include_edits(
    node: &LinkedNode,
    file_path: &Path,
    depth: usize,
    source: &str,
    edits: &mut Vec<(usize, usize, String, PathBuf, Vec<IncludeRegion>)>,
    chain: &mut Vec<(PathBuf, PathBuf, String, std::ops::Range<usize>)>,
) -> Result<(), CandyError> {
    // `#include "path"` (and the code-mode form `include "path"`) is parsed
    // by Typst as a `ModuleInclude` AST node (NOT a `FuncCall`), so this is
    // the primary detection path.
    if let Some(mi) = node.get().cast::<ast::ModuleInclude>() {
        if let Some(rel) = module_include_path(mi) {
            // In markup, `#include "x"` — the leading `#` is a *separate*
            // token that sits OUTSIDE the `ModuleInclude` node's range, so
            // `node.range()` is just `include "x"` (no `#`). Extend the
            // replaced span to swallow that `#`: otherwise the replacement
            // (which itself begins with `#`, e.g. an inlined `#mobject`)
            // would leave the original `#` behind and produce `##mobject`
            // (or `##import`) — invalid Typst that fails to parse.
            let mut call_range = node.range();
            let s = call_range.start.saturating_sub(1);
            if source.as_bytes().get(s) == Some(&b'#') {
                call_range.start = s;
            }
            include_file(&rel, call_range, file_path, depth, source, edits, chain)?;
        }
    }
    for child in node.children() {
        collect_include_edits(&child, file_path, depth, source, edits, chain)?;
    }
    Ok(())
}

/// Extract the included path string from a `ModuleInclude` node.
/// Both `#include "file.tyx"` (markup mode, with the `#`) and the bare
/// `include "file.typ"` form (code mode, `#` omitted) yield the string
/// literal; any other form yields `None`.
fn module_include_path(mi: ast::ModuleInclude) -> Option<String> {
    match mi.source() {
        Expr::Str(s) => Some(s.get().to_string()),
        Expr::Parenthesized(p) => match p.expr() {
            Expr::Str(s) => Some(s.get().to_string()),
            _ => None,
        },
        _ => None,
    }
}

/// Read `rel` (resolved against `file_path`'s directory), apply the
/// single-chain circular-include guard, recursively expand it, and record an
/// edit that replaces `call_range` (the whole `#include "…"` span) with
/// the expanded content.
fn include_file(
    rel: &str,
    call_range: std::ops::Range<usize>,
    file_path: &Path,
    depth: usize,
    source: &str,
    edits: &mut Vec<(usize, usize, String, PathBuf, Vec<IncludeRegion>)>,
    chain: &mut Vec<(PathBuf, PathBuf, String, std::ops::Range<usize>)>,
) -> Result<(), CandyError> {
    // `file_path` is the *includer file*, so resolve `rel` against its
    // parent directory (matching Typst's include path resolution). Using
    // `file_path.join(rel)` would treat the file itself as a directory.
    let base = file_path.parent().unwrap_or_else(|| Path::new(""));
    let target = base.join(rel);
    let content = std::fs::read_to_string(&target)
        .map_err(|e| CandyError::Parse(format!("cannot include {rel:?}: {e}"), None))?;
    // Resolve to a canonical form so equivalent paths (`./x.tyx` vs `x.tyx`,
    // symlinks, `..`) compare equal in the cycle guard.
    let canon = std::fs::canonicalize(&target).unwrap_or_else(|_| target.clone());
    // Single-chain circular-include guard (see `collect_include_edits` docs):
    // a file may appear at most once along the path from the root to this
    // include. If the target is already on the chain it is an ancestor of
    // itself → reject as a circular include. Duplicates on *different*
    // branches (a diamond) are allowed.
    if chain.iter().any(|(c, _, _, _)| c == &canon) {
        let chain_str = chain
            .iter()
            .map(|(c, _, _, _)| c.display().to_string())
            .collect::<Vec<_>>()
            .join(" -> ");
        // The deepest frame points at the *current* includer's `#include`
        // call (the one that closes the cycle). Every outer includer's
        // `#include` call (recorded in `chain`, skipping the root entry which
        // has no includer) becomes an "included from …" trace frame so the
        // user sees the whole `a → b → c → a` path rather than a single line.
        let mut loc = SourceLoc::at(file_path, source, call_range.clone());
        loc.include_trace = chain
            .iter()
            .skip(1)
            .map(|(_, inc, src, rg)| SourceLoc::at(inc, src, rg.clone()))
            .collect();
        return Err(CandyError::Parse(
            format!(
                "circular include detected: {} is already included on this include path ({} -> {}). \
                 A file may only be included once along a single include chain; \
                 duplicates on different branches are allowed.",
                canon.display(),
                chain_str,
                canon.display()
            ),
            Some(loc),
        ));
    }
    // Expand the included file in the context of *its* own directory so its
    // nested includes resolve correctly (matching Typst's resolution).
    chain.push((
        canon,
        file_path.to_path_buf(),
        source.to_string(),
        call_range.clone(),
    ));
    let (expanded, nested) = expand_includes(&content, &target, depth + 1, chain)?;
    chain.pop();
    edits.push((call_range.start, call_range.end, expanded, target, nested));
    Ok(())
}

// Parse `.tyx` file into a `Scene` AST.
///
/// Precondition: `path` exists and is valid UTF-8 (else E001).
/// Postcondition: returns `Ok(Scene)` with validated slides (else E002).
/// `private_metadata` is set to the fixed defaults.
pub fn parse_tyx(path: &Path, ignore_version: bool) -> Result<Scene, CandyError> {
    let source_file = std::fs::read_to_string(path)?; // E001 on missing file
    // Recursively inline every `#include "…"` statement *before* the AST walk, so
    // candy's parser sees directives declared inside included files as part of
    // one flat document (otherwise Typst would only resolve them at compile
    // time, long after the parse has run, and their directives would be lost).
    // The `SourceMap` records, for each inlined region, the `#include`
    // call-site that pulled it in — used to trace diagnostics back to that line.
    // `chain` tracks the include path from the root down to `path` so that
    // `expand_includes` can reject a file that is included twice on the *same*
    // include chain (a cyclic include) while still allowing the same file to
    // appear on different branches.
    let mut chain = vec![(
        std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()),
        path.to_path_buf(),
        source_file.clone(),
        0..source_file.len(),
    )];
    let (raw, include_regions) = expand_includes(&source_file, path, 0, &mut chain)?;
    let source_map = SourceMap {
        regions: include_regions,
    };
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

    // Record the source file path so diagnostics can point at the real file.
    let mut ctx = ParseCtx {
        file_path: path.to_path_buf(),
        source: raw.clone(),
        source_map: source_map.clone(),
        ..Default::default()
    };
    // The whole document is the implicit root scope (id 0).
    ctx.scope_stack.push(0);
    ctx.scope_starts.insert(0, 0);
    ctx.next_scope_id = 1;
    // The whole document is also the implicit whole-document *scene* (id 0).
    // Every mobject / action not declared inside an explicit `#scene(...)`
    // belongs to it. This is the "no scene defined → whole document is one
    // scene" rule. If the document later defines explicit `#scene(...)` calls,
    // this implicit scene is *not* used for rendering; the parser validates
    // that the document is then either (multiple parallel scenes, no root
    // content) or (root content → whole doc is one scene).
    ctx.scenes.push(SceneInfo {
        id: 0,
        name: Some("root".to_string()),
        scope: 0,
        page_size: if ctx.candy_show_rule_seen {
            // The global `candy` show rule owns the canvas; its config sizes
            // the implicit scene.
            Some((ctx.config.width_pt, ctx.config.height_pt))
        } else {
            ctx.page_size_cm
                .map(|(w, h)| (w * PT_PER_CM, h * PT_PER_CM))
        },
        start_ms: 0,
        end_ms: 0,
        owns_labels: Vec::new(),
    });
    ctx.current_scene = 0;
    ctx.next_scene_id = 1;
    walk(&node, &raw, &mut ctx);
    // Surface any fatal parse error raised lazily by a directive (e.g. a `kc`
    // operation on a name that is not visible in the current lexical scope).
    if let Some(e) = ctx.pending_error.take() {
        return Err(e);
    }
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
    // Attribute every declared label to its owning scene in *declaration*
    // order (`label_order` is recorded the first time each label is registered).
    // This keeps the natural top-to-bottom flow layout and the paint z-order
    // faithful to source order; iterating `label_scene` (a `HashMap`) directly
    // would scrambleparallel mobjects on every run.
    for label in &ctx.label_order {
        let sid = ctx.label_scene.get(label).copied().unwrap_or(0);
        if let Some(s) = ctx.scenes.iter_mut().find(|s| s.id == sid) {
            s.owns_labels.push(label.clone());
        }
    }

    // CandyDumpedYou: a `.tyx` must import candy via a Typst package import
    // of the form `@<namespace>/candy:<version>`. File-style imports
    // (`#import "candy"`) are rejected. The imported version must satisfy at
    // least one semver requirement from the CLI's baked-in compatibility list
    // (`[package.metadata.tyx].compatible_versions` in the Rust Cargo.toml;
    // matched via the `semver` crate), unless `--ignore-version` is passed —
    // which also accepts file-style imports for development/testing.
    if !ignore_version && ctx.file_style_candy_import {
        return Err(CandyError::CandyDumpedYou(
            "file-style candy import detected; candy must be imported as a \
             Typst package (`@<namespace>/candy:<version>`), not via a local \
             file path (e.g. `#import \"candy\"`); pass --ignore-version to \
             bypass this check"
                .into(),
            ctx.file_style_import_loc.clone(),
        ));
    }
    if !ctx.candy_imported && !ctx.file_style_candy_import {
        return Err(CandyError::CandyDumpedYou(
            "the .tyx does not import the candy package; candy can only render \
             documents that import `@<namespace>/candy:<version>`"
                .into(),
            None,
        ));
    }
    // The `candy` show rule owns the global canvas (width / height / ppi / fps).
    // Without it the document has no viewport configuration at all, so candy
    // would fall back to a guessed page and render garbage — reject it up front
    // with the same E008 used for a missing import.
    if !ctx.candy_show_rule_seen {
        return Err(CandyError::CandyDumpedYou(
            "the .tyx never applies the candy show rule; add `#show: candy` \
             (optionally `#show: candy.with(width: .., height: .., ppi: .., \
             fps: ..)`) right after the candy import — it configures the global \
             canvas, resolution and frame rate for the whole animation"
                .into(),
            ctx.candy_import_loc.clone(),
        ));
    }
    if !ignore_version {
        if let Some(ref imported_v) = ctx.candy_import_version {
            if !crate::version_is_compatible(imported_v) {
                return Err(CandyError::CandyDumpedYou(
                    format!(
                        "candy version mismatch: .tyx imports candy:{imported_v} \
                         but the installed candy CLI {cli} only accepts versions \
                         matching `{reqs}`; pass --ignore-version to skip this \
                         check",
                        cli = crate::CANDY_VERSION,
                        reqs = crate::compatible_versions_display(),
                    ),
                    ctx.candy_import_loc.clone(),
                ));
            }
        }
    }

    // Every scene must have at least one slide so the renderer can emit frames.
    // Candy DSL directives (`mobject`, `animate`, `pause`, `play`, `subtitle`,
    // `ecnew`, `scene`, …) all advance the parse cursor and produce slides.
    // If no candy directives were used, `ctx.slides` stays empty — inject a
    // single `pause(duration: 500)` so the whole-document recompiler still
    // produces one frame and renders the static Typst content. Content that
    // overflows the viewport is warned (W018) and clipped at rasterization;
    // there is no per-page timeline splitting.
    if ctx.slides.is_empty() {
        ctx.slides.push(Slide {
            start_ms: 0,
            duration_ms: 500,
            actions: Vec::new(),
            loc: None,
        });
    }

    // The implicit whole-document `SceneInfo` (id 0) was pushed *before* the
    // AST walk, so its `page_size` could not yet reflect a `#show: candy`
    // config discovered during the walk. Backfill it now so the implicit scene
    // (when it is the only scene) shares the single global canvas.
    if ctx.candy_show_rule_seen {
        if let Some(root) = ctx.scenes.iter_mut().find(|s| s.id == 0) {
            root.page_size = Some((ctx.config.width_pt, ctx.config.height_pt));
        }
    }

    // Document-structure rule (flat scenes, no nesting, no "root scene" split):
    // a document is valid if it is EITHER
    //   (a) a single implicit scene — no explicit `#scene` call at all; or
    //   (b) one or more explicit `#scene(...)` calls at the root, and the root
    //       has NO content of its own (every mobject / directive is inside a
    //       scene).
    // If explicit scenes exist but the implicit scene (id 0) owns any labels,
    // the document mixes root content with scenes, which is a hard parse error.
    let has_explicit_scenes = ctx.scenes.iter().any(|s| s.id != 0);
    if has_explicit_scenes {
        let root_owns = ctx
            .scenes
            .iter()
            .find(|s| s.id == 0)
            .map(|s| s.owns_labels.len())
            .unwrap_or(0);
        if root_owns > 0 {
            return Err(CandyError::Scene(
                "a document that defines explicit `#scene(...)` calls must not \
                 also contain content at the document root; either put all \
                 content inside scenes (parallel scenes, no root content) or \
                 keep root content and let the whole document be a single scene \
                 (no explicit `#scene` call)"
                    .into(),
                None,
            ));
        }
        // Drop the implicit scene (id 0): with explicit scenes present, only the
        // explicit scenes are rendered, each on its own page.
        ctx.scenes.retain(|s| s.id != 0);
    }

    // Assign UUID-like names to anonymous scenes so they can be referenced
    // by scene-switch(target: "scene_<uuid>") even when the author didn't provide
    // an explicit name.
    Scene::assign_anonymous_names(&mut ctx.scenes);

    // Re-order the slide timeline to follow `#scene-switch(target)` jumps and
    // recompute each scene's `[start_ms, end_ms]` interval so the renderer's
    // `active_scene_at(time_ms)` gating shows the target scene's content.
    // No-op for documents without scene switching.
    finalize_scene_switching(&mut ctx)?;

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
        page_size: if ctx.candy_show_rule_seen {
            // The global `candy` show rule owns the canvas; its config sizes
            // the root scene (scene-level width/height are deprecated).
            Some((ctx.config.width_pt, ctx.config.height_pt))
        } else {
            ctx.page_size_cm
                .map(|(w, h)| (w * PT_PER_CM, h * PT_PER_CM))
        },
        subtitles: ctx.subtitles,
        counters: ctx.counters,
        counter_events: ctx.counter_events,
        kcdefs: ctx.kcdefs,
        kc_events: ctx.kc_events,
        scopes: ctx.scopes,
        scenes: ctx.scenes,
        groups: ctx.groups.clone(),
        artifacts: ParseArtifacts {
            source: raw,
            file_path: path.to_path_buf(),
            source_map,
            mobject_body: ctx.mobject_body_ranges.clone(),
            scene_call: ctx.scene_call_ranges.clone(),
            subtitle_call: ctx.subtitle_call_ranges.clone(),
            label_locs: ctx.label_locs.clone(),
            name_ref_locs: ctx.name_ref_locs.clone(),
            // Global canvas / export config. Seeded by `DEFAULT` and overwritten
            // by the first `#show: candy` show rule via `detect_candy_show_rule`.
            config: ctx.config,
        },
        private_metadata: private,
    };
    scene.validate()?;
    Ok(scene)
}

/// Accumulated parse state.
#[derive(Default)]
pub(crate) struct ParseCtx {
    /// Absolute path of the `.tyx` being parsed (used to build `SourceLoc`s
    /// for diagnostics so errors/warnings point at the real file).
    pub(crate) file_path: std::path::PathBuf,
    /// local name -> original Candy symbol (resolved through imports).
    pub(crate) symbol_map: HashMap<String, String>,
    /// Candy module alias names (`candy`, `c`, ...) bound by a bare
    /// `#import "candy"` / `#import "candy" as X`. Enables `candy.mobject(...)`
    /// field-access detection while keeping ordinary method calls out.
    pub(crate) candy_aliases: HashSet<String>,
    /// Whether the candy package itself was imported via the canonical
    /// `@preview/candy:<version>` package form. File-style imports (`#import "candy"`)
    /// are NOT recognized — they trigger CandyDumpedYou.
    pub(crate) candy_imported: bool,
    /// The version string from the detected `@preview/candy:<version>` import,
    /// if any. Used to verify it satisfies the installed candy CLI's semver
    /// compatibility list (`[package.metadata.tyx].compatible_versions`).
    pub(crate) candy_import_version: Option<String>,
    /// Whether a file-style candy import (`#import "candy"` / `#import ".../candy"`)
    /// was seen — these trigger CandyDumpedYou.
    pub(crate) file_style_candy_import: bool,
    /// The raw `.tyx` source text, retained so diagnostics (e.g. E008 import /
    /// version errors) can build a [`SourceLoc`] pointing at the offending
    /// `#import` line instead of just emitting a message.
    pub(crate) source: String,
    /// Source map for the expanded `source`: for each inlined `#include(…)`
    /// region, the `#include` call-site that pulled it in. Used by
    /// [`ParseCtx::loc`] to trace an error inside an included file back to the
    /// `#include` line in the includer (the "referenced position").
    pub(crate) source_map: SourceMap,
    /// Source location of the detected `@<ns>/candy:<version>` package import,
    /// used to point E008 (version mismatch) at the real import line.
    pub(crate) candy_import_loc: Option<SourceLoc>,
    /// Source location of a file-style candy import (`#import "candy"`), used to
    /// point E008 at the offending import.
    pub(crate) file_style_import_loc: Option<SourceLoc>,
    /// label -> raw body source text.
    pub(crate) items: HashMap<Label, String>,
    /// label -> frame-0 visual state.
    pub(crate) initial: HashMap<Label, FrameData>,
    pub(crate) slides: Vec<Slide>,
    pub(crate) audio: Vec<AudioTrack>,
    /// Sequential ("after") boundary: the end time of the most recently closed
    /// directive entry. The next `after` animation begins here. Also the
    /// reference point for `subtitle` / `ecnew` / counter-event markers (which
    /// are not slide-emitting and never advance it).
    pub(crate) cursor: u32,
    /// Start time of the most recently closed directive entry. Used as the
    /// start point for the next `with` animation (PPT "Start: With Previous").
    pub(crate) timeline_start: u32,
    /// Running end time within the currently-open directive entry. Used to
    /// place continuation slides of a multi-slide directive (e.g. `blink`,
    /// `morph`) sequentially after the entry's first slide.
    pub(crate) entry_end: u32,
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
    /// Per-scope registry of declared **mobject labels** (scope id → labels).
    /// Used to detect a label redefined in the *same* lexical scope (which
    /// warns + shadows) while a redefinition inside a *nested* scope is
    /// legitimate Typst shadowing and is left alone.
    pub(crate) mobject_names: HashMap<String, HashSet<String>>,
    /// Per-scope registry of declared **ecnew names** (scope id → names),
    /// mirroring `mobject_names` for the easing-counter namespace.
    pub(crate) ecnew_names: HashMap<String, HashSet<String>>,
    /// Keyframe counters (the "keyframe-counter module", `kc*`).
    pub(crate) kcdefs: Vec<KeyframeCounterDef>,
    pub(crate) kc_events: Vec<CounterEvent>,
    /// Per-scope registry of declared **kcnew names** (scope id → names),
    /// mirroring `mobject_names` / `ecnew_names` for the keyframe-counter
    /// namespace. Used for duplicate-name detection and scope-visibility checks.
    pub(crate) kcnew_names: HashMap<String, HashSet<String>>,
    /// First fatal parse error raised lazily by a directive (e.g. a `kc`
    /// operation on a name that is not visible in the current lexical scope).
    /// Surfaced from `parse_tyx` after the AST walk completes.
    pub(crate) pending_error: Option<CandyError>,
    /// Source location of every label's declaration (`#mobject` / `#ecnew`),
    /// keyed by label. Fed into `Scene::artifacts.label_locs` so later
    /// diagnostics (e.g. `E004` LabelNotFound) can point at the declaration.
    pub(crate) label_locs: HashMap<Label, SourceLoc>,
    /// Source location of every *name reference* (a `#mobject`/`#ecnew` target
    /// label or easing-counter name written inside a directive such as
    /// `#animate(target: "x")`, `#ecpause("c")`, …), keyed by the resolved name
    /// string. Fed into `Scene::artifacts.name_ref_locs` so name-anomaly errors
    /// (`E004` LabelNotFound / `E006` UnknownKey) can point at the *usage* site
    /// rather than only at declarations (which don't exist for an unknown name).
    pub(crate) name_ref_locs: HashMap<String, SourceLoc>,
    /// Source location of each `#scene-switch(target: "X")` call, keyed by the
    /// target name. Used to point `E006 UnknownKey` at the *usage* site when a
    /// scene switch references a scene that was never declared.
    pub(crate) scene_switch_locs: HashMap<String, SourceLoc>,
    /// Source location of the directive currently being processed. `process_call`
    /// sets it from the call node's range; `emit_slide` copies it onto each
    /// produced `Slide` so structural `E002`/`Parse` errors (e.g. a bad
    /// `duration_ms`) can point at the offending directive.
    pub(crate) current_directive_loc: Option<SourceLoc>,
    /// Lexical scope intervals (finalized on scope exit / at end of parse).
    pub(crate) scopes: Vec<crate::core::ast::ScopeInfo>,
    /// Nested scene tree (see `SceneInfo`). `current_scene` is the scene that
    /// owns mobjects declared right now; `scene_stack` tracks open scenes.
    pub(crate) scenes: Vec<SceneInfo>,
    /// Parent→child grouping links (`child → parent`), recorded by `#group`. A
    /// group is a special kind of mobject: an mobject can own child mobjects, and
    /// animating the parent transforms all children (parent→child inheritance).
    /// The renderer composes group transforms using this map; the parent label is
    /// itself a normal mobject (registered in `items` / `initial`) but is never
    /// drawn directly.
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
    /// arrangement / z-order ofparallel mobjects comes out scrambled).
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
    /// Global canvas / resolution / frame-rate config declared via the
    /// `#show: candy` (or `#show: candy.with(width:.., height:.., ppi:.., fps:..)`)
    /// show rule. Populated by [`detect_candy_show_rule`] during the AST walk;
    /// falls back to [`GlobalConfig::DEFAULT`] until the first `candy` show rule
    /// is seen. Replaces the per-scene `width` / `height` / `bg` mechanism.
    pub(crate) config: crate::core::ast::GlobalConfig,
    /// Whether a `#show: candy` show rule has already been seen. The candy config
    /// is global and singular; a second `candy` show rule triggers a conflict
    /// warning rather than overwriting the first.
    pub(crate) candy_show_rule_seen: bool,
}

impl ParseCtx {
    /// Resolve a byte-range `range` (in the *expanded* source) to a
    /// [`SourceLoc`] for diagnostics.
    ///
    /// If the range falls inside a region that was inlined by `#include(…)`,
    /// this returns a [`SourceLoc`] pointing at the *referencing position* —
    /// the `#include "…"` call-site in the includer file (its original
    /// source text and the call's byte range), exactly as the user requested
    /// ("被include引入的外部文件源码追踪时要跟踪到被引用的位置"). Otherwise the
    /// range is mapped to the original `.tyx` file directly.
    pub(crate) fn loc(&self, range: std::ops::Range<usize>) -> SourceLoc {
        if let Some(inc) = self.source_map.region_for(range.start) {
            // Point at the *actual* error inside the deepest included file
            // (`inc_path`/`inc_raw`) and attach the full includer chain as an
            // "included from …" trace, so a parse error inside a nested include
            // shows the real offending line plus every outer includer's
            // `#include` call-site.
            let k = range.start - inc.start;
            let span =
                (range.end - range.start).clamp(1, inc.inc_raw.len().saturating_sub(k).max(1));
            let child_range = k..(k + span).min(inc.inc_raw.len());
            return SourceLoc::at_included_error(
                &inc.inc_path,
                &inc.inc_raw,
                child_range,
                &inc.chain,
            );
        }
        SourceLoc::at(&self.file_path, &self.source, range)
    }

    /// Begin a new directive entry and return its absolute start time on the
    /// timeline, resolving `timing` + `delay`:
    ///
    /// - `Some(Timing::With)` → start at the previous entry's start (`delay` added).
    /// - `None` / `Some(Timing::After)` → start at the sequential boundary `cursor`
    ///   (`delay` added).
    ///
    /// Records the entry's start as `timeline_start` (so the next `with` entry
    /// lines up) and seeds `entry_end` for any continuation slides.
    pub(crate) fn entry_start(&mut self, timing: Option<Timing>, delay: u32) -> u32 {
        let start = match timing {
            Some(Timing::With) => self.timeline_start.saturating_add(delay),
            _ => self.cursor.saturating_add(delay),
        };
        self.timeline_start = start;
        self.entry_end = start;
        start
    }

    /// Advance the running end of the current entry by `duration`. Call once
    /// after emitting each slide (the first slide uses the value returned by
    /// [`Self::entry_start`]; continuation slides read `entry_end` directly).
    pub(crate) fn entry_advance(&mut self, duration: u32) {
        self.entry_end += duration;
    }

    /// Close the current entry, advancing the sequential boundary `cursor` to
    /// the furthest end seen so far (a `with` entry may finish before the
    /// previous `after` entry, but the timeline must extend to the last
    /// finishing animation).
    pub(crate) fn entry_close(&mut self) {
        self.cursor = self.cursor.max(self.entry_end);
    }

    /// Resolve an `#audio` track's start time without advancing the timeline
    /// (audio plays concurrently and must not block subsequent animations).
    pub(crate) fn audio_start(&self, timing: Option<Timing>, delay: u32) -> u32 {
        match timing {
            Some(Timing::With) => self.timeline_start.saturating_add(delay),
            _ => self.cursor.saturating_add(delay),
        }
    }
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

    // Scene scoping: a `scene` call opens a *flat* scene around its body.
    // Scenes may only appear at the document root — a `#scene` nested inside
    // another `#scene` is a hard parse error. `#scene` accepts **only** the
    // `name` argument; every other argument (including the historically-removed
    // `width` / `height` / `bg`, which are treated as if they never existed) is
    // an unknown / undefined argument — a regular parse error, not a silent
    // ignore. All scene-specific errors use the dedicated `CandyError::Scene`
    // (E011) code so they never collide with E008.
    if let Some(call) = node.get().cast::<ast::FuncCall>() {
        if call_symbol(&call, ctx).as_deref() == Some("scene") {
            // Nesting is forbidden. We are already inside a scene if the stack
            // is non-empty (the implicit whole-document scene id 0 is *not*
            // pushed onto the stack, so this only triggers for explicit nested
            // scenes).
            if !ctx.scene_stack.is_empty() {
                ctx.pending_error = Some(CandyError::Scene(
                    "nested #scene is not allowed; a scene may only be defined at \
                     the document root. Use `#switch(target: \"name\")` to move \
                     between scenes instead of nesting them."
                        .to_string(),
                    Some(ctx.loc(node.range())),
                ));
                return;
            }
            let id = ctx.next_scene_id;
            ctx.next_scene_id += 1;
            let scope = ctx.next_scope_id;
            ctx.next_scope_id += 1;
            // Validate `#scene` arguments against its signature. Only `name` is
            // supported. Argument-format mistakes (an unknown argument or a
            // wrong-typed one) are a regular **parse** error (`E002`), uniform
            // with every other API-format error in candy — including the removed
            // `width` / `height` / `bg`, which are treated as if they never
            // existed. Scene *structural* mistakes (nesting, root-content mix)
            // use the dedicated `CandyError::Scene` (E011).
            let mut scene_name: Option<String> = None;
            for a in call.args().items() {
                if let ast::Arg::Named(n) = a {
                    let name = n.name().as_str();
                    match name {
                        "name" => {
                            if let Expr::Str(s) = n.expr() {
                                let name = s.get().to_string();
                                if !is_valid_typst_ident(&name) {
                                    ctx.pending_error = Some(CandyError::InvalidKey {
                                        what: "scene name".into(),
                                        value: name.clone(),
                                        not_ident: true,
                                        loc: Some(ctx.loc(node.range())),
                                    });
                                    return;
                                }
                                scene_name = Some(name);
                            } else {
                                ctx.pending_error = Some(CandyError::Parse(
                                    "the `name` argument of #scene must be a string \
                                     literal, got a non-string value"
                                        .to_string(),
                                    Some(ctx.loc(node.range())),
                                ));
                                return;
                            }
                        }
                        other => {
                            ctx.pending_error = Some(CandyError::Parse(
                                format!(
                                    "`{other}` is not a valid argument for #scene; valid \
                                     arguments are: name"
                                ),
                                Some(ctx.loc(node.range())),
                            ));
                            return;
                        }
                    }
                }
            }
            // When the global `candy` show rule is present it owns the canvas,
            // so every scene shares that single uniform page; otherwise the
            // size is measured from `#set page(...)` (see `page_size_cm`).
            let page_size = if ctx.candy_show_rule_seen {
                Some((ctx.config.width_pt, ctx.config.height_pt))
            } else {
                ctx.page_size_cm
                    .map(|(w, h)| (w * PT_PER_CM, h * PT_PER_CM))
            };
            // Capture the *entire* `#scene(...)` call span: the whole-document
            // recompiler gates each scene with `sys.inputs.at("candy:active_scene")`
            // so only the active scene emits a page (keeping every Typst
            // invocation to a single page). Gating the whole call (rather than
            // just its body) is required because `#scene(…)` expands to
            // `page(…)`, which would still emit an (empty) page if only its body
            // were blanked.
            let cr = node.range();
            ctx.scene_call_ranges.insert(id, (cr.start, cr.end));
            let start = ctx.cursor;
            // Warn on a scene name redefined in the same lexical scope. A scene
            // with no explicit `name` is anonymous and cannot collide.
            if let Some(name) = &scene_name {
                if ctx
                    .scenes
                    .iter()
                    .any(|s| s.name.as_deref() == Some(name.as_str()))
                {
                    warn!(CandyWarn::DuplicateName(
                        "scene".into(),
                        name.clone(),
                        ctx.loc(node.range()),
                    ));
                }
            }
            ctx.scenes.push(SceneInfo {
                id,
                name: scene_name,
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
            ctx.current_scene = 0;
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
    // Detect `#show: candy` (global canvas / resolution / frame-rate config).
    if node.get().cast::<ast::ShowRule>().is_some() {
        detect_candy_show_rule(node, ctx);
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
        process_import(imp, node.range(), ctx);
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
fn process_import(imp: ast::ModuleImport, range: std::ops::Range<usize>, ctx: &mut ParseCtx) {
    // Detect candy package imports. Any Typst package import of the form
    // `@<namespace>/candy:<version>` is accepted (e.g. `@preview/candy:0.1.0`,
    // `@local/candy:0.1.0`). File-style imports (`#import "candy"` or
    // `#import ".../candy"`) are recorded separately and trigger
    // CandyDumpedYou unless `--ignore-version` is passed.
    if let Expr::Str(s) = imp.source() {
        let src = s.get();
        // Package import: `@<ns>/candy:<version>` — any namespace is accepted.
        if src.starts_with('@') {
            if let Some((path, version)) = src.split_once(':') {
                if path.ends_with("/candy") {
                    ctx.candy_imported = true;
                    ctx.candy_import_version = Some(version.to_string());
                    ctx.candy_import_loc = Some(ctx.loc(range.clone()));
                }
            }
        } else if src == "candy" || src.ends_with("/candy") {
            ctx.file_style_candy_import = true;
            ctx.file_style_import_loc = Some(ctx.loc(range));
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
            // Bare module import (`#import "@<ns>/candy:..." as c`):
            // the module object itself is bound to a name, enabling
            // `candy.mobject(...)` field-access calls. Record the bound alias so
            // `call_symbol` only treats *that* receiver's Candy fields as Candy.
            if let Expr::Str(s) = imp.source() {
                let src = s.get();
                if src.starts_with('@') && src.contains("/candy:") {
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

/// Detect and consume a `#show: candy` / `#show: candy.with(width:.., height:..,
/// ppi:.., fps:..)` global-config show rule.
///
/// The candy canvas config is *global and singular*. The first such show rule
/// seeds [`ParseCtx::config`]; any further `candy` show rule is silently
/// ignored (the first config wins). Detection recognizes both the canonical
/// `#import "candy": *`
/// form (so `candy` resolves through `symbol_map`) and the `#import "candy" as
/// X` alias form (so `X` resolves through `candy_aliases`), plus the
/// `candy.with(..)` field-access invocation.
///
/// Unlike [`extract_page_size`] this does **not** descend into nested nodes:
/// a `candy` show rule is always a top-level statement, so only the node itself
/// is inspected.
fn detect_candy_show_rule(node: &LinkedNode, ctx: &mut ParseCtx) {
    let Some(show) = node.get().cast::<ast::ShowRule>() else {
        return;
    };
    // A selector-bearing show rule (`#show math.equation: ...`) is not a candy
    // config rule; only the no-selector `show: candy` form configures the canvas.
    if show.selector().is_some() {
        return;
    }
    let transform = show.transform();
    if !is_candy_show_callee(&transform, ctx) {
        return;
    }

    // Extract named args. `#show: candy` (bare, no `.with`) uses defaults;
    // `#show: candy.with(width:.., ...)` carries the overrides on the FuncCall.
    let mut width_pt: Option<f64> = None;
    let mut height_pt: Option<f64> = None;
    let mut ppi: Option<f64> = None;
    let mut fps: Option<f64> = None;

    // For `candy.with(..)` the args live on the FuncCall; for bare `candy` there
    // are none (all default).
    if let Expr::FuncCall(fc) = &transform {
        // width / height are lengths; ppi / fps are unitless numbers.
        for arg in fc.args().items() {
            let ast::Arg::Named(named) = arg else {
                continue;
            };
            match named.name().as_str() {
                "width" => {
                    if let Some(cm) = collect_named_lengths_here(named.expr()) {
                        width_pt = Some(cm * crate::core::ast::PT_PER_CM);
                    }
                }
                "height" => {
                    if let Some(cm) = collect_named_lengths_here(named.expr()) {
                        height_pt = Some(cm * crate::core::ast::PT_PER_CM);
                    }
                }
                "ppi" => ppi = crate::parser::expr::expr_to_f64(&named.expr()),
                "fps" => fps = crate::parser::expr::expr_to_f64(&named.expr()),
                _ => {}
            }
        }
    }

    if !ctx.candy_show_rule_seen {
        ctx.candy_show_rule_seen = true;
        let mut cfg = crate::core::ast::GlobalConfig::DEFAULT;
        if let Some(w) = width_pt {
            cfg.width_pt = w;
        }
        if let Some(h) = height_pt {
            cfg.height_pt = h;
        }
        if let Some(p) = ppi {
            cfg.ppi = p as u32;
        }
        if let Some(f) = fps {
            cfg.fps = f as u32;
        }
        ctx.config = cfg;
    } else {
        // A second (or later) `candy` show rule is silently ignored — the
        // global canvas is owned by the first show rule only.
    }
}

/// True iff `callee` is the candy module used as a show-rule target: either the
/// bare `candy` identifier (resolved via `symbol_map` or `candy_aliases`) or a
/// `candy.with(..)` field-access invocation (`candy` is an alias, `with` is the
/// field).
fn is_candy_show_callee(callee: &Expr, ctx: &ParseCtx) -> bool {
    match callee {
        Expr::Ident(id) => {
            let name = id.as_str();
            // `#import "candy" as X` binds X into `candy_aliases`.
            if ctx.candy_aliases.contains(name) {
                return true;
            }
            // `#import "candy": *` maps the local name to the "candy" symbol.
            let norm = name.replace('_', "-");
            ctx.symbol_map
                .get(&norm)
                .or_else(|| ctx.symbol_map.get(name))
                .map(|s| s == "candy")
                .unwrap_or(false)
        }
        Expr::FuncCall(fc) => {
            // `candy.with(..)` — the callee is a `.with` field access whose
            // target is the candy function itself. That target may be a module
            // alias (`#import "candy" as X` → `X.with`) *or* the `candy`
            // symbol pulled in by a glob import (`#import "candy": *`), so
            // resolve it with the same rules as a bare identifier.
            let Expr::FieldAccess(fa) = fc.callee() else {
                return false;
            };
            fa.field().as_str() == "with" && is_candy_show_callee(&fa.target(), ctx)
        }
        _ => false,
    }
}

/// Re-order the slide timeline to follow `#scene-switch(target)` jumps and
/// recompute each scene's `[start_ms, end_ms]` interval to match the remapped
/// playback.
///
/// The renderer decides which scene is visible at a given frame via
/// `Scene::active_scene_at(time_ms)` (every `#scene(...)` body is gated by
/// `sys.inputs.at("candy:active_scene")`), so for a scene switch to actually
/// *show* the target scene's content the scene intervals must reflect the
/// switched order. This walk does exactly that: it follows the linear slide
/// list, and whenever it hits a `SceneSwitch` to a **later** scene it jumps the
/// cursor to that scene's `start_ms` and skips the slides belonging to the
/// scenes in between (so they are not replayed at the wrong time). Each scene's
/// interval is then recomputed from the segments it actually played.
///
/// Documents without any `SceneSwitch` action are left completely untouched —
/// their linear timeline and sequential (mutually-exclusive) scene intervals are
/// already correct, so this is a no-op for them. Backward switches (target
/// `start_ms <=` current cursor) are intentionally not jumped: replaying an
/// earlier scene would require duplicating its frames and is out of scope for
/// the v1 switch model.
fn finalize_scene_switching(ctx: &mut ParseCtx) -> Result<(), CandyError> {
    let has_switch = ctx.slides.iter().any(|s| {
        s.actions
            .iter()
            .any(|a| matches!(a, Action::SceneSwitch { .. }))
    });
    if !has_switch {
        return Ok(());
    }

    let n = ctx.slides.len();
    // Original cumulative start times (pre-remap), used to map a slide back to
    // the scene it originally belonged to.
    let mut slide_start = vec![0u32; n];
    {
        let mut p = 0u32;
        for (i, s) in ctx.slides.iter().enumerate() {
            slide_start[i] = p;
            p += s.duration_ms;
        }
    }

    // `active_at(t)` mirrors `Scene::active_scene_at` but over `ctx.scenes`
    // (the intervals as they stand when this runs). Scenes are flat, so the
    // active scene is simply the one whose interval contains `t`.
    let active_at = |t: u32| -> usize {
        let mut best: Option<usize> = None;
        for s in &ctx.scenes {
            if t >= s.start_ms && t <= s.end_ms {
                best = Some(s.id);
            }
        }
        // Fall back to the implicit whole-document scene (id 0) if it still
        // exists, otherwise the first declared scene.
        best.unwrap_or(0)
    };

    let resolve = |target: &str| -> Option<usize> {
        ctx.scenes
            .iter()
            .find(|s| s.name.as_deref() == Some(target))
            .map(|s| s.id)
            .or_else(|| {
                target
                    .parse::<usize>()
                    .ok()
                    .filter(|id| ctx.scenes.iter().any(|s| s.id == *id))
            })
    };

    let mut order: Vec<usize> = Vec::with_capacity(n);
    let mut segments: std::collections::HashMap<usize, Vec<(u32, u32)>> =
        std::collections::HashMap::new();

    let mut i = 0usize;
    let mut ptr = 0u32;
    while i < n {
        order.push(i);
        let dur = ctx.slides[i].duration_ms;
        let end = ptr + dur;
        // Remap this slide's absolute `start_ms` to the switched (remapped)
        // timeline. `slide_start[i]` is the *sequential* cursor (computed above
        // by cumulative duration, ignoring `with`/`delay`), so `ptr -
        // slide_start[i]` is the scene-switch jump delta (0 until the first
        // forward jump). Adding it to the parser's timing-aware `start_ms`
        // preserves each slide's `with`/`delay` offset relative to its new
        // position, while collapsing the gap the switch would otherwise leave.
        ctx.slides[i].start_ms += ptr - slide_start[i];
        let sid = active_at(slide_start[i] + dur / 2);
        segments.entry(sid).or_default().push((ptr, end));

        // Detect a scene switch on this slide.
        let switch = ctx.slides[i].actions.iter().find_map(|a| match a {
            Action::SceneSwitch { target, .. } => Some(target.clone()),
            _ => None,
        });
        let mut jumped = false;
        if let Some(t) = switch {
            let tid = resolve(&t);
            if tid.is_none() {
                // Unknown scene target: report E006 at the `#scene-switch` call
                // site rather than silently skipping the jump.
                let loc = ctx.scene_switch_locs.get(&t).cloned();
                return Err(CandyError::UnknownKey("scene".into(), t, loc));
            }
            if let Some(tid) = tid {
                let tstart = ctx
                    .scenes
                    .iter()
                    .find(|s| s.id == tid)
                    .map(|s| s.start_ms)
                    .unwrap_or(ptr);
                if tstart > ptr {
                    ptr = tstart;
                    // Skip slides whose *original* scene is not the target, so we
                    // don't replay the skipped scene's content at the wrong time.
                    while i + 1 < n
                        && active_at(slide_start[i + 1] + ctx.slides[i + 1].duration_ms / 2) != tid
                    {
                        i += 1;
                    }
                    jumped = true;
                }
            }
        }
        if !jumped {
            ptr = end;
        }
        i += 1;
    }

    // Reorder slides to the switched playback order.
    let mut new_slides = Vec::with_capacity(n);
    for &idx in &order {
        new_slides.push(ctx.slides[idx].clone());
    }
    ctx.slides = new_slides;

    // Recompute each scene's interval from the segments it actually played.
    for s in ctx.scenes.iter_mut() {
        if let Some(segs) = segments.get(&s.id) {
            let start = segs.iter().map(|(a, _)| *a).min().unwrap_or(s.start_ms);
            let end = segs.iter().map(|(_, b)| *b).max().unwrap_or(s.end_ms);
            s.start_ms = start;
            s.end_ms = end;
        } else {
            // A scene never reached by the switched flow is made inactive so the
            // renderer never shows its content.
            s.start_ms = u32::MAX;
            s.end_ms = u32::MAX;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::ast::Action;

    /// Rewrite a test's `#import "candy"` into `#import "@preview/candy:<v>"`,
    /// auto-fetching the published package version from `typst/typst.toml`,
    /// and inject the mandatory `#show: candy` global-config show rule.
    ///
    /// Project convention: only *test* code needs the Typst package version
    /// auto-fetched (production code must not). Wrapping every test source with
    /// this helper guarantees no test hard-codes a candy version.
    ///
    /// Every `.tyx` must apply the `candy` show rule (it owns the global canvas
    /// / ppi / fps); a missing one is E008. Tests write plain sources without
    /// it, so the helper appends `#show: candy` right after the candy import —
    /// mirroring what real `.tyx` documents do (see `examples/*.tyx`).
    fn with_auto_version(raw: &str) -> String {
        let v = crate::typst_package_version().expect("typst/typst.toml must declare a `version`");
        let pkg = format!("@preview/candy:{v}");
        // `:`-form imports (`#import "candy": *` and renamed imports) keep
        // their explicit bindings, so just rewrite the package path.
        let s = raw.replace("#import \"candy\":", &format!("#import \"{pkg}\":"));
        // Bare module import (`#import "candy"`) must preserve the `candy`
        // binding name so `#candy.mobject(...)` still resolves after the path
        // rewrite — bind it explicitly as `candy`.
        let s = s.replace(
            "#import \"candy\"\n",
            &format!("#import \"{pkg}\" as candy\n"),
        );
        inject_candy_show_rule(&s)
    }

    /// Insert `#show: candy` directly after the first candy `#import` line of
    /// `src`, unless the source already applies the show rule itself.
    ///
    /// The rule must come *after* the import (so the `candy` binding exists)
    /// but *before* any content, matching the layout of every `examples/*.tyx`.
    ///
    /// Some tests use a selective import (`#import "…candy…": animate as anim`)
    /// that never binds the `candy` show function itself, so an extra
    /// `#import …: candy` is prepended to bring it into scope.
    fn inject_candy_show_rule(src: &str) -> String {
        if src.contains("#show: candy") {
            return src.to_string();
        }
        let Some(import_line) = src
            .lines()
            .find(|l| l.trim_start().starts_with("#import") && l.contains("candy"))
        else {
            // No candy import at all: this source deliberately tests the
            // missing-import E008 path, so leave it untouched.
            return src.to_string();
        };
        // Is the `candy` show function in scope? A glob import (`: *`) or a
        // module import bound `as candy` provides it; a selective import of
        // other symbols does not, so import `candy` explicitly alongside it.
        let binds_candy = import_line.contains(" as candy")
            || import_line.trim_end().ends_with('*')
            || import_line.contains(": candy")
            || import_line.contains(", candy");
        let injected = if binds_candy {
            format!("{import_line}\n#show: candy")
        } else {
            let path = import_line
                .split('"')
                .nth(1)
                .expect("candy import line must contain a quoted package path");
            format!("{import_line}\n#import \"{path}\": candy\n#show: candy")
        };
        src.replacen(import_line, &injected, 1)
    }

    const DOT: &str = r#"
#import "candy": *
#mobject("dot", circle(radius: 1cm, fill: blue))
#mobject("dot2", rect(width: 1cm, height: 1cm))
#animate("dot", to: (4cm, 0pt), duration: 30, easing: "linear")
#animate("dot2", scale: 150%, duration: 20)
#pause(duration: 15)
#audio("voice.opus", blocking: false, loop: false, volume: 0.9, slice: none)
"#;

    #[test]
    fn parses_dot_ast() {
        let tmp = std::env::temp_dir().join("candy_test_dot.tyx");
        std::fs::write(&tmp, with_auto_version(DOT)).unwrap();
        let scene = parse_tyx(&tmp, true).unwrap();
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

    /// `timing: "with"` begins at the previous animation's start (parallel);
    /// `timing: "after"` begins at the latest end so far; `delay:` shifts both.
    #[test]
    fn timing_with_and_delay_resolve_start_ms() {
        let src = with_auto_version(
            r#"
#import "candy": *
#mobject("a", circle(radius: 1cm))
#mobject("b", rect(width: 1cm, height: 1cm))
#mobject("c", text(size: 12pt)[hi])
#animate("a", to: (4cm, 0pt), duration: 1000)
#animate("b", to: (0cm, 4cm), duration: 1000, timing: "with")
#animate("c", to: (1cm, 1cm), duration: 500, timing: "after", delay: 250)
"#,
        );
        let tmp = std::env::temp_dir().join("candy_test_timing_with.tyx");
        std::fs::write(&tmp, src).unwrap();
        let scene = parse_tyx(&tmp, true).unwrap();
        assert_eq!(scene.slides.len(), 3);
        // a: default `after` → sequential boundary 0.
        assert_eq!(scene.slides[0].start_ms, 0);
        assert_eq!(scene.slides[0].duration_ms, 1000);
        // b: `with` a, no delay → starts at a's start (0), overlapping it.
        assert_eq!(scene.slides[1].start_ms, 0);
        // c: `after` + delay 250 → starts at the latest end (a ends 1000) + 250.
        assert_eq!(scene.slides[2].start_ms, 1250);
        std::fs::remove_file(&tmp).ok();
    }

    #[test]
    fn mobject_declaration_order_is_preserved() {
        //parallel mobjects must keep their source declaration order. The labels are
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
        let scene = parse_tyx(&tmp, true).unwrap();
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
        let scene = parse_tyx(&tmp, true).unwrap();
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
#anim("box", scale: 150%, duration: 20)
"#,
        );
        let tmp = std::env::temp_dir().join("candy_test_box.tyx");
        std::fs::write(&tmp, src).unwrap();
        let scene = parse_tyx(&tmp, true).unwrap();
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
        let scene = parse_tyx(&tmp, true).unwrap();
        // one synthetic block label
        let blocks: usize = scene
            .items
            .keys()
            .filter(|l| l.0.starts_with("__block_"))
            .count();
        assert_eq!(blocks, 1);
        // A `play` block emits two slides: the play window itself, then a 1ms
        // `Hide` slide so sequential `play` steps don't visually overlap.
        assert_eq!(scene.slides.len(), 2);
        assert_eq!(scene.slides[0].duration_ms, 25);
        assert_eq!(scene.slides[1].duration_ms, 1);
        assert!(matches!(
            scene.slides[1].actions.as_slice(),
            [Action::Hide { .. }]
        ));
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
#animate("box", rotate: 45deg, opacity: 50%, easing: "smooth", duration: 20)
#pause(duration: 15)
#audio("voice.opus", blocking: false, loop: false, volume: 0.9)
#play(circle(radius: 1cm), duration: 10)
"#;
        std::fs::write(&tmp, calls).unwrap();
        let out = crate::renderer::compile_file_for_test(&tmp, &Default::default());
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
#animate("sq", rotate: 90deg, opacity: 30%, duration: 25, easing: "cubic-in-out")
"#,
        );
        let tmp = std::env::temp_dir().join("candy_test_rotate.tyx");
        std::fs::write(&tmp, src).unwrap();
        let scene = parse_tyx(&tmp, true).unwrap();
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
#indicate("dot", factor: 120%, duration: 18)
#flash("dot", factor: 180%, duration: 12)
#wiggle("dot", degrees: 12deg, duration: 16)
#disappear("dot")
#appear("dot")
#set_color("dot", color: red, duration: 1)
"#,
        );
        let tmp = std::env::temp_dir().join("candy_test_manim.tyx");
        std::fs::write(&tmp, src).unwrap();
        let scene = parse_tyx(&tmp, true).unwrap();
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
        let scene = parse_tyx(&tmp, true).unwrap();
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

    /// Regression for `#fade-transform`: `to` must start hidden (opacity 0) and
    /// fade IN to 1 across the crossfade window while `from` fades OUT from 1 to
    /// 0. A naive implementation that only emits `FadeOut(from)` + `FadeIn(to)`
    /// leaves `to` at its default opacity 1 for the whole timeline (a plain
    /// mobject is visible by default), so `to` shows at full color during
    /// `from`'s fade-out — the reported "to 一直是全量着色" bug.
    #[test]
    fn fade_transform_crossfades_opacity() {
        let src = with_auto_version(
            r#"
#import "candy": *
#mobject("old", circle(radius: 1cm, fill: blue))
#mobject("new", rect(width: 2cm, height: 2cm, fill: red))
#fade-transform("old", "new", duration: 300, easing: "smooth")
"#,
        );
        let tmp = std::env::temp_dir().join("candy_test_fade_transform.tyx");
        std::fs::write(&tmp, src).unwrap();
        let scene = parse_tyx(&tmp, true).unwrap();
        let raw = crate::core::scheduler::schedule(&scene).unwrap();
        // The renderer interpolates the sparse scheduler keyframes into one
        // dense frame per sample time before building per-frame states (see
        // `prepare_states`); a fade-in only exists on the *interpolated* curve,
        // not on the raw keyframes (which keep a frame-0 seed at opacity 1).
        let frames = crate::core::interpolator::interpolate(raw);

        let to: Vec<&FrameData> = frames.iter().filter(|f| f.target.0 == "new").collect();
        let from: Vec<&FrameData> = frames.iter().filter(|f| f.target.0 == "old").collect();
        assert!(!to.is_empty(), "expected keyframes for `new` (to)");
        assert!(!from.is_empty(), "expected keyframes for `old` (from)");

        // `to` starts hidden and ends fully visible.
        assert!(
            (to[0].opacity - 0.0).abs() < 1e-6,
            "`to` must start hidden, got opacity {}",
            to[0].opacity
        );
        assert!(
            (to.last().unwrap().opacity - 1.0).abs() < 1e-6,
            "`to` must end fully visible, got opacity {}",
            to.last().unwrap().opacity
        );
        // A genuine crossfade passes through an intermediate opacity (not an
        // instant switch), proving `to` actually fades IN.
        assert!(
            to.iter().any(|f| (0.4..0.6).contains(&f.opacity)),
            "`to` should crossfade through ~0.5; opacities: {:?}",
            to.iter().map(|f| f.opacity).collect::<Vec<_>>()
        );

        // `from` starts fully visible and ends hidden.
        assert!(
            (from[0].opacity - 1.0).abs() < 1e-6,
            "`from` must start fully visible, got opacity {}",
            from[0].opacity
        );
        assert!(
            (from.last().unwrap().opacity - 0.0).abs() < 1e-6,
            "`from` must end hidden, got opacity {}",
            from.last().unwrap().opacity
        );
        assert!(
            from.iter().any(|f| (0.4..0.6).contains(&f.opacity)),
            "`from` should crossfade through ~0.5; opacities: {:?}",
            from.iter().map(|f| f.opacity).collect::<Vec<_>>()
        );
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
        let scene = parse_tyx(&tmp, true).unwrap();
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
#save-state("dot", slot: "home")
#restore("dot", slot: "home", duration: 10, easing: "smooth")
#indicate("dot", factor: 120%, duration: 12)
#flash("dot", factor: 200%, duration: 10)
#wiggle("dot", degrees: 10deg, duration: 14)
#disappear("dot")
#appear("dot")
#set-color("dot", color: red, duration: 1)
"#;
        std::fs::write(&tmp, calls).unwrap();
        let out = crate::renderer::compile_file_for_test(&tmp, &Default::default());
        let _ = std::fs::remove_file(&tmp);
        assert!(
            out.is_ok(),
            "std Typst failed to compile manim API: {out:?}"
        );
    }

    /// Verify that a `#scene` nested inside another `#scene` is a hard parse
    /// error (scenes are now flat — nesting is forbidden; use `#switch` to move
    /// between scenes instead).
    #[test]
    fn nested_scene_is_a_parse_error() {
        let src = with_auto_version(
            r#"
#import "candy": *
#scene(name: "outer")[
  #mobject("a", circle(radius: 1cm))
  #scene(name: "inner")[
    #mobject("b", rect(width: 1cm))
  ]
]
"#,
        );
        let tmp = std::env::temp_dir().join("candy_test_nested_scene.tyx");
        std::fs::write(&tmp, src).unwrap();
        let res = parse_tyx(&tmp, true);
        assert!(
            res.is_err(),
            "nested #scene must be rejected, got: {:?}",
            res.ok()
        );
        std::fs::remove_file(&tmp).ok();
    }

    /// A `#scene` that carries the removed `width` / `height` / `bg` arguments
    /// (or any unknown argument) is a hard parse error, not a silent ignore.
    #[test]
    fn scene_with_removed_args_is_a_parse_error() {
        for (label, args) in [
            ("width", "width: 16cm"),
            ("height", "height: 9cm"),
            ("bg", "bg: white"),
            ("unknown", "frobnicate: 42"),
        ] {
            let src = with_auto_version(&format!(
                r#"
#import "candy": *
#scene({args})[
  #mobject("a", circle(radius: 1cm))
]
"#
            ));
            let tmp = std::env::temp_dir().join(format!("candy_test_scene_{label}.tyx"));
            std::fs::write(&tmp, src).unwrap();
            let res = parse_tyx(&tmp, true);
            assert!(
                res.is_err(),
                "#scene({args}) must be rejected, got: {:?}",
                res.ok()
            );
            std::fs::remove_file(&tmp).ok();
        }
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
#set page(width: 16cm, height: 9cm, margin: 0pt)
#scene()[
  #mobject("a", circle(radius: 1cm))
  #pause(duration: 50)
]
#set page(width: 16cm, height: 9cm, margin: 0pt)
#scene()[
  #mobject("b", rect(width: 1cm))
  #pause(duration: 50)
]
"#,
        );
        let tmp = std::env::temp_dir().join("candy_test_sibling_scene.tyx");
        std::fs::write(&tmp, src).unwrap();
        let scene = parse_tyx(&tmp, true).unwrap();

        // Scenes are flat and the implicit whole-document scene is dropped as
        // soon as explicit `#scene(...)` calls exist, so exactly the 2 siblings
        // remain.
        assert_eq!(
            scene.scenes.len(),
            2,
            "2 flat siblings, no implicit root: {:?}",
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
        let scene = parse_tyx(&tmp, true).unwrap();
        // No items should be produced by the false-positive calls.
        // The parser auto-inserts a single pause slide for any scene that
        // has content (even if it's just the candy import), so we expect
        // exactly one empty slide.
        assert_eq!(scene.slides.len(), 1, "expected auto-inserted pause slide");
        assert_eq!(scene.slides[0].actions.len(), 0);
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
        let scene = parse_tyx(&tmp, true).unwrap();
        // No slides should be produced by the shadowed call.
        // The parser auto-inserts a single pause slide for any scene.
        assert_eq!(scene.slides.len(), 1, "expected auto-inserted pause slide");
        assert_eq!(scene.slides[0].actions.len(), 0);
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
        let scene = parse_tyx(&tmp, true).unwrap();
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
        let err = parse_tyx(&tmp, true).unwrap_err();
        std::fs::remove_file(&tmp).ok();
        assert_eq!(err.code(), "E008", "expected E008, got {err:?}");
    }

    /// The `candy` show rule owns the global canvas (width / height / ppi /
    /// fps). A `.tyx` that imports candy but never applies it has no viewport
    /// configuration, so it must be rejected with E008 rather than silently
    /// rendered against a guessed page.
    #[test]
    fn missing_candy_show_rule_is_e008() {
        // Deliberately bypass `with_auto_version`'s show-rule injection: build
        // the versioned import by hand so the source has the import but no
        // `#show: candy`.
        let v = crate::typst_package_version().expect("typst/typst.toml version");
        let src = format!(
            "\n#import \"@preview/candy:{v}\": *\n\
             #mobject(\"a\", circle(radius: 1cm))\n\
             #animate(\"a\", to: (4cm, 0pt), duration: 30)\n"
        );
        let tmp = std::env::temp_dir().join("candy_test_no_show_rule.tyx");
        std::fs::write(&tmp, src).unwrap();
        let err = parse_tyx(&tmp, true).unwrap_err();
        std::fs::remove_file(&tmp).ok();
        assert_eq!(err.code(), "E008", "expected E008, got {err:?}");
        assert!(
            err.to_string().contains("show rule"),
            "E008 message must name the missing show rule: {err}"
        );
    }

    /// `#show: candy.with(..)` overrides the default canvas, and the resulting
    /// config is what every scene's `page_size` reports.
    #[test]
    fn candy_show_rule_with_overrides_global_canvas() {
        let v = crate::typst_package_version().expect("typst/typst.toml version");
        let src = format!(
            "\n#import \"@preview/candy:{v}\": *\n\
             #show: candy.with(width: 20cm, height: 10cm, ppi: 96, fps: 60)\n\
             #mobject(\"a\", circle(radius: 1cm))\n\
             #animate(\"a\", to: (4cm, 0pt), duration: 30)\n"
        );
        let tmp = std::env::temp_dir().join("candy_test_show_rule_with.tyx");
        std::fs::write(&tmp, src).unwrap();
        let scene = parse_tyx(&tmp, true).unwrap();
        std::fs::remove_file(&tmp).ok();
        let cfg = scene.artifacts.config;
        assert_eq!(cfg.ppi, 96);
        assert_eq!(cfg.fps, 60);
        let expect = (20.0 * PT_PER_CM, 10.0 * PT_PER_CM);
        assert_eq!((cfg.width_pt, cfg.height_pt), expect);
        assert_eq!(scene.page_size, Some(expect));
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
        let scene = parse_tyx(&tmp, true).unwrap();
        std::fs::remove_file(&tmp).ok();
        assert!(scene.items.contains_key(&Label("dot".into())));
    }

    /// A mobject label redefined in the *same* scope must warn (W014) and let
    /// the **later** definition shadow the earlier. We assert the shadowing
    /// outcome (the surviving body is the later one); the warning itself is
    /// emitted via `warn!` to stderr.
    #[test]
    fn duplicate_mobject_same_scope_shadows_later() {
        let src = with_auto_version(
            r#"
#import "candy": *
#mobject("a", circle(radius: 1cm, fill: red))
#mobject("a", rect(width: 2cm, height: 2cm, fill: blue))
"#,
        );
        let tmp = std::env::temp_dir().join("candy_test_dup_mobject.tyx");
        std::fs::write(&tmp, src).unwrap();
        let scene = parse_tyx(&tmp, true).unwrap();
        std::fs::remove_file(&tmp).ok();
        // Later definition wins.
        assert_eq!(
            scene.items[&Label("a".into())],
            "rect(width: 2cm, height: 2cm, fill: blue)"
        );
    }

    /// Two mobjects with the same label in *different* (nested) scopes are
    /// legitimate Typst shadowing and must NOT be flagged as duplicates — the
    /// parser should still succeed without error.
    #[test]
    fn nested_scope_mobject_redefinition_is_not_duplicate() {
        let src = with_auto_version(
            r#"
#import "candy": *
#mobject("a", circle(radius: 1cm, fill: red))
#{
  #mobject("a", rect(width: 2cm, height: 2cm, fill: blue))
}
"#,
        );
        let tmp = std::env::temp_dir().join("candy_test_nested_mobject.tyx");
        std::fs::write(&tmp, src).unwrap();
        let scene = parse_tyx(&tmp, true).unwrap();
        std::fs::remove_file(&tmp).ok();
        // Both declarations parse; the inner (later) body is the surviving one
        // in the global `items` map. The point is no error / no duplicate warn.
        assert_eq!(
            scene.items[&Label("a".into())],
            "rect(width: 2cm, height: 2cm, fill: blue)"
        );
    }

    /// An ecnew redefined in the *same* scope must warn (W014) and let the
    /// **later** definition shadow the earlier. We assert the shadowing
    /// outcome: exactly one `CounterDef` with that name survives, carrying the
    /// later seed.
    #[test]
    fn duplicate_ecnew_same_scope_shadows_later() {
        let src = with_auto_version(
            r#"
#import "candy": *
#ecnew("k", seed: 1, step: 1)
#ecnew("k", seed: 99, step: 2)
"#,
        );
        let tmp = std::env::temp_dir().join("candy_test_dup_ecnew.tyx");
        std::fs::write(&tmp, src).unwrap();
        let scene = parse_tyx(&tmp, true).unwrap();
        std::fs::remove_file(&tmp).ok();
        let same: Vec<&crate::core::ast::CounterDef> =
            scene.counters.iter().filter(|c| c.name == "k").collect();
        // The duplicate is collapsed: only the later definition survives.
        assert_eq!(same.len(), 1, "duplicate ecnew should shadow to one def");
        assert_eq!(same[0].seed, 99, "later ecnew seed should win");
        assert_eq!(same[0].step, 2);
    }

    /// Two ecnew counters with the same name in *different* (nested) scopes are
    /// legitimate Typst shadowing: both `CounterDef`s survive (resolved at
    /// runtime by scope depth) and must NOT warn.
    #[test]
    fn nested_scope_ecnew_redefinition_is_not_duplicate() {
        let src = with_auto_version(
            r#"
#import "candy": *
#ecnew("k", seed: 1)
#{
  #ecnew("k", seed: 2)
}
"#,
        );
        let tmp = std::env::temp_dir().join("candy_test_nested_ecnew.tyx");
        std::fs::write(&tmp, src).unwrap();
        let scene = parse_tyx(&tmp, true).unwrap();
        std::fs::remove_file(&tmp).ok();
        let same: Vec<&crate::core::ast::CounterDef> =
            scene.counters.iter().filter(|c| c.name == "k").collect();
        assert_eq!(same.len(), 2, "nested ecnew redefinitions both survive");
    }

    /// A document with mobject items but no candy animation directives should
    /// auto-insert a single `pause(duration: 500)` slide so the static content
    /// is still rendered.
    #[test]
    fn mobject_without_animation_auto_inserts_pause() {
        let src = with_auto_version(
            r#"
#import "candy": *
#mobject("a", circle(radius: 1cm))
#mobject("b", rect(width: 2cm))
"#,
        );
        let tmp = std::env::temp_dir().join("candy_test_static_mobjects.tyx");
        std::fs::write(&tmp, src).unwrap();
        let scene = parse_tyx(&tmp, true).unwrap();
        std::fs::remove_file(&tmp).ok();
        // Should have exactly one auto-inserted pause slide.
        assert_eq!(scene.slides.len(), 1, "expected auto-inserted pause slide");
        assert_eq!(scene.slides[0].duration_ms, 500);
        assert_eq!(scene.slides[0].actions.len(), 0);
        // Items should be present.
        assert_eq!(scene.items.len(), 2);
    }

    /// A document with only static Typst markup and no candy directives should
    /// also auto-insert pause (the parser guarantees at least one slide per scene).
    #[test]
    fn pure_typst_markup_auto_inserts_pause() {
        let src = with_auto_version(
            r#"
#import "candy": *
= Hello World
This is plain Typst content with an equation $E = mc^2$.
"#,
        );
        let tmp = std::env::temp_dir().join("candy_test_pure_typst.tyx");
        std::fs::write(&tmp, src).unwrap();
        let scene = parse_tyx(&tmp, true).unwrap();
        std::fs::remove_file(&tmp).ok();
        // Even pure Typst content gets a slide so it can be rendered.
        assert_eq!(scene.slides.len(), 1, "expected auto-inserted pause slide");
        assert_eq!(scene.slides[0].duration_ms, 500);
    }

    /// A document with subtitles but no animation should also auto-insert pause.
    #[test]
    fn subtitles_without_animation_auto_inserts_pause() {
        let src = with_auto_version(
            r#"
#import "candy": *
#subtitle("caption", body: [Hello world], start: 0)
"#,
        );
        let tmp = std::env::temp_dir().join("candy_test_static_subtitle.tyx");
        std::fs::write(&tmp, src).unwrap();
        let scene = parse_tyx(&tmp, true).unwrap();
        std::fs::remove_file(&tmp).ok();
        assert_eq!(scene.slides.len(), 1, "expected auto-inserted pause slide");
        assert_eq!(scene.slides[0].duration_ms, 500);
        assert!(!scene.subtitles.is_empty());
    }

    /// A document with counters but no animation should auto-insert pause.
    #[test]
    fn counters_without_animation_auto_inserts_pause() {
        let src = with_auto_version(
            r#"
#import "candy": *
#ecnew("k", seed: 0, step: 1)
"#,
        );
        let tmp = std::env::temp_dir().join("candy_test_static_counter.tyx");
        std::fs::write(&tmp, src).unwrap();
        let scene = parse_tyx(&tmp, true).unwrap();
        std::fs::remove_file(&tmp).ok();
        assert_eq!(scene.slides.len(), 1, "expected auto-inserted pause slide");
        assert_eq!(scene.slides[0].duration_ms, 500);
        assert!(!scene.counters.is_empty());
    }

    /// A document with an explicit scene that owns labels but has no slides
    /// should auto-insert a pause.
    #[test]
    fn explicit_scene_with_labels_no_slides_auto_inserts_pause() {
        let src = with_auto_version(
            r#"
#import "candy": *
#set page(width: 16cm, height: 9cm, margin: 0pt)
#scene()[
  #mobject("a", circle(radius: 1cm))
]
"#,
        );
        let tmp = std::env::temp_dir().join("candy_test_scene_no_slides.tyx");
        std::fs::write(&tmp, src).unwrap();
        let scene = parse_tyx(&tmp, true).unwrap();
        std::fs::remove_file(&tmp).ok();
        assert_eq!(scene.slides.len(), 1, "expected auto-inserted pause slide");
        assert_eq!(
            scene.scenes.len(),
            1,
            "one explicit scene, no implicit root"
        );
        // The explicit scene should own label "a".
        let s = scene.scenes.iter().find(|s| s.id == 1).unwrap();
        assert!(!s.owns_labels.is_empty());
    }

    /// A file included twice on the *same* include path (a → b → a) is a
    /// cycle and must be rejected with a `circular include` error that carries
    /// a `SourceLoc` at the offending `#include` call-site.
    #[test]
    fn circular_include_on_same_chain_is_rejected() {
        let dir = std::env::temp_dir();
        let a = dir.join("candy_test_cycle_a.tyx");
        let b = dir.join("candy_test_cycle_b.tyx");
        std::fs::write(
            &a,
            with_auto_version(
                "#include \"candy_test_cycle_b.tyx\"\n#mobject(\"a\", circle(radius: 1cm))\n",
            ),
        )
        .unwrap();
        std::fs::write(
            &b,
            "#include \"candy_test_cycle_a.tyx\"\n#mobject(\"b\", circle(radius: 1cm))\n",
        )
        .unwrap();
        let err = parse_tyx(&a, true).unwrap_err();
        std::fs::remove_file(&a).ok();
        std::fs::remove_file(&b).ok();
        match err {
            CandyError::Parse(msg, Some(loc)) => {
                assert!(
                    msg.contains("circular include"),
                    "expected circular-include error, got: {msg}"
                );
                assert_eq!(
                    loc.path, b,
                    "error should point at the file whose `#include` closes the cycle (b → a)"
                );
                assert!(
                    loc.line_text.contains("include"),
                    "error should point at the `#include` line, got: {:?}",
                    loc.line_text
                );
            }
            other => panic!("expected CandyError::Parse with a location, got {other:?}"),
        }
    }

    /// The same file included on *different* branches (a diamond:
    /// root → a → x and root → b → x) is allowed — only a single include
    /// *path* may not repeat a file.
    #[test]
    fn diamond_include_on_different_branches_is_allowed() {
        let dir = std::env::temp_dir();
        let root = dir.join("candy_test_diamond_root.tyx");
        let a = dir.join("candy_test_diamond_a.tyx");
        let b = dir.join("candy_test_diamond_b.tyx");
        let x = dir.join("candy_test_diamond_x.tyx");
        std::fs::write(
            &root,
            with_auto_version(
                "#import \"candy\": *\n#include \"candy_test_diamond_a.tyx\"\n#include \"candy_test_diamond_b.tyx\"\n#mobject(\"r\", circle(radius: 1cm))\n",
            ),
        )
        .unwrap();
        std::fs::write(
            &a,
            "#include \"candy_test_diamond_x.tyx\"\n#mobject(\"a\", circle(radius: 1cm))\n",
        )
        .unwrap();
        std::fs::write(
            &b,
            "#include \"candy_test_diamond_x.tyx\"\n#mobject(\"b\", circle(radius: 1cm))\n",
        )
        .unwrap();
        std::fs::write(&x, "#mobject(\"x\", circle(radius: 1cm))\n").unwrap();
        let scene = parse_tyx(&root, true).expect("diamond includes must be allowed");
        std::fs::remove_file(&root).ok();
        std::fs::remove_file(&a).ok();
        std::fs::remove_file(&b).ok();
        std::fs::remove_file(&x).ok();
        // All four mobjects (root, a, b, and the shared x) must be present.
        assert!(scene.items.contains_key(&Label("r".into())));
        assert!(scene.items.contains_key(&Label("a".into())));
        assert!(scene.items.contains_key(&Label("b".into())));
        assert!(scene.items.contains_key(&Label("x".into())));
    }

    /// Keys must be valid Typst identifiers. A `#mobject` whose label contains a
    /// space (not an identifier) is rejected with `E007 InvalidKey`.
    #[test]
    fn mobject_with_spaced_label_is_e007() {
        let src = with_auto_version(
            "#import \"candy\": *\n#mobject(\"my object\", circle(radius: 1cm))\n",
        );
        let tmp = std::env::temp_dir().join("candy_test_bad_mobject_space.tyx");
        std::fs::write(&tmp, src).unwrap();
        let err = parse_tyx(&tmp, true).unwrap_err();
        std::fs::remove_file(&tmp).ok();
        assert_eq!(err.code(), "E007", "expected E007, got {err:?}");
    }

    /// A leading digit is not a valid identifier start, so a numeric-prefixed
    /// `#mobject` label is also `E007 InvalidKey`.
    #[test]
    fn mobject_with_leading_digit_label_is_e007() {
        let src =
            with_auto_version("#import \"candy\": *\n#mobject(\"1st\", circle(radius: 1cm))\n");
        let tmp = std::env::temp_dir().join("candy_test_bad_mobject_digit.tyx");
        std::fs::write(&tmp, src).unwrap();
        let err = parse_tyx(&tmp, true).unwrap_err();
        std::fs::remove_file(&tmp).ok();
        assert_eq!(err.code(), "E007", "expected E007, got {err:?}");
    }

    /// A `#scene(name: ...)` whose name is not a valid identifier is `E007`.
    #[test]
    fn scene_with_invalid_name_is_e007() {
        let src = with_auto_version(
            "#import \"candy\": *\n#scene(name: \"bad name\", body: { #mobject(\"a\", circle()) })\n",
        );
        let tmp = std::env::temp_dir().join("candy_test_bad_scene_name.tyx");
        std::fs::write(&tmp, src).unwrap();
        let err = parse_tyx(&tmp, true).unwrap_err();
        std::fs::remove_file(&tmp).ok();
        assert_eq!(err.code(), "E007", "expected E007, got {err:?}");
    }

    /// Valid identifier keys (including Unicode letters, which Typst's
    /// `is_ident` accepts) must parse without an `E007`.
    #[test]
    fn valid_unicode_mobject_name_is_accepted() {
        let src = with_auto_version(
            "#import \"candy\": *\n#mobject(\"café_名前-x\", circle(radius: 1cm))\n",
        );
        let tmp = std::env::temp_dir().join("candy_test_good_mobject_unicode.tyx");
        std::fs::write(&tmp, src).unwrap();
        let scene = parse_tyx(&tmp, true).expect("Unicode identifier keys must be accepted");
        std::fs::remove_file(&tmp).ok();
        assert!(scene.items.contains_key(&Label("café_名前-x".into())));
    }
}
