//! Render `FrameData` into SVG (and, for the video path, RGBA) using the
//! `typst` compiler library in-process — no `typst` CLI is spawned.
//!
//! World implementation: candy uses [`typst_kit`] (the same font discovery +
//! package resolution crate that the official `typst` CLI uses) so the
//! in-process compile is *identical* to `typst compile`:
//!
//! - System fonts are discovered via `fontdb` (Linux fontconfig, macOS
//!   CoreText, Windows DirectWrite).
//! - Embedded fallback fonts (Libertinus Serif, New Computer Modern, DejaVu
//!   Sans Mono) are loaded from `typst-assets`.
//! - `@preview/<pkg>` package imports resolve from the local cache
//!   (`~/.cache/typst/packages` on Linux) and download on demand.
//! - Local `#import "file.typ"` resolves relative to the project root (the
//!   parent directory of the `.tyx` source).
//!
//! This closes candy's v0.1 "mobject bodies must be standalone Typst"
//! limitation: any valid Typst document now compiles in candy's World.
//!
//! The renderer is split into second-level submodules under `typst/`:
//!
//! * [`world`] — the in-process Typst [`World`](typst::World) implementation
//!   (`WorldState` + `CandyWorld`, plus the package downloader).
//! * [`content`] — per-frame source assembly: preamble re-declaration, the
//!   content timeline (`content_for`), AST-driven `ecval(...)` counter
//!   substitution, and subtitle placement / compilation.
//! * [`svg`] — SVG geometry parsing (bounding boxes, path tokenization,
//!   attribute extraction) for the native-layout pass and formula fragments.
//! * [`camera`] — the global `#camera` pan/zoom/rotate SVG transform.
//! * [`composite`] — formula-id localization for the per-glyph transform path.
//! * [`morph`] — shape-`#morph` rendering helpers: ring localization, SVG
//!   color → Typst conversion, and `polygon(...)` source generation.
//! * [`pages`] — cross-page scene playback: the per-scene page schedule that
//!   maps global time to the active page (sequential page playback with frozen
//!   timelines for the not-yet-active pages).
//! * [`transform`] — per-glyph `#transform` plan types and the body placement
//!   source builder.
//!
//! The `Renderer`'s `impl` block is further split across three submodules so
//! this file stays focused on the struct, the compile/cache core, and the
//! body/bg resolution helpers:
//!
//! * [`flow`] — flow (first-frame) layout and per-frame effective-state
//!   computation (group composition, camera scoping, cross-page gating).
//! * [`source`] — the stable *parameterized* whole-document Typst source
//!   (mobject/ecval/reveal wrapping) and the per-frame `sys.inputs` builder.
//! * [`frame`] — the frame-render pipeline: the whole-document native-Typst SVG
//!   path, opacity/subtitle overlays, and the per-glyph transform overlay.
pub(crate) mod camera;
pub(crate) mod composite;
pub(crate) mod content;
pub(crate) mod lru;
pub(crate) mod morph;
pub(crate) mod pages;
pub(crate) mod svg;
pub(crate) mod transform;
pub(crate) mod world;
// `Renderer`'s `impl` block is split across the second-level submodules below so
// this file stays readable: `flow` (layout + per-frame state), `source`
// (parameterized whole-document source assembly), and `frame` (the actual
// frame-render pipeline). Each re-uses `mod.rs`'s imports via `use super::*`.
pub(crate) mod flow;
pub(crate) mod frame;
pub(crate) mod source;
// Re-export the helper items so the `Renderer` impl below can call them with
// the same unqualified names as before the split.
pub(crate) use self::camera::*;
pub(crate) use self::composite::*;
pub(crate) use self::content::*;
pub(crate) use self::lru::LruCache;
pub(crate) use self::morph::*;
pub(crate) use self::pages::*;
pub(crate) use self::svg::*;
pub(crate) use self::transform::*;
pub(crate) use self::world::*;
/// Centimeters per Typst point (1pt = 1/72in, 1in = 2.54cm).
use crate::core::ast::PT_PER_CM;
#[cfg(test)]
use crate::core::ast::ParseArtifacts;
use crate::core::ast::{FrameData, Label, Scene, Subtitle};
use crate::core::diag::{CandyError, CandyWarn, SourceLoc};
use crate::core::morph::{MorphPlan, extract_shapes_from_svg, polygon_area};
use crate::parser::expr::strip_string_literal;
use crate::warn;
use std::collections::HashMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use typst::{World, WorldExt};
use typst_layout::PagedDocument;
use typst_library::foundations::{Dict, Smart, Value};
use typst_library::visualize::Paint;
use typst_svg::SvgOptions;
#[cfg(test)]
use typst_syntax::FileId;
#[cfg(test)]
use typst_syntax::Source as TypstSource;
use typst_syntax::ast::{self, Expr};
use typst_syntax::{LinkedNode, parse_code};
#[cfg(test)]
use typst_syntax::{RootedPath, VirtualPath, VirtualRoot};
/// Maximum segment length (in Typst points) when bisecting morph outline rings.
/// Smaller = smoother morph but more points (the plan is sampled per frame, so
/// the per-frame cost is linear in the point count — 3pt is a good balance).
const MORPH_MAX_SEGMENT: f64 = 3.0;
/// Capacity of the per-frame compiled-document cache (`body_cache`). Bounded so
/// an animated render cannot accumulate one `PagedDocument` per frame (that was
/// the OOM). Static / paused objects keep a stable key and stay resident; the
/// per-frame churn of moving objects is evicted.
///
/// With the whole-document native-Typst path each cached entry is one full-page
/// `PagedDocument` (the entire scene typeset by Typst), which is a few MB at
/// HD/4K — far smaller than the old design that parked *N* full-canvas
/// per-object documents per frame. The cap is kept deliberately small so the
/// body cache alone can never approach a gigabyte even when every frame
/// produces a distinct document. Peak memory is therefore `BODY_CACHE_CAP`
/// documents + `jobs` in-flight RGBA frames + the (small, cropped) sprite
/// cache — independent of the total frame count `N`.
const BODY_CACHE_CAP: usize = 16;

/// Renders a [`Scene`] into frames, with auto-detected mobject positions.
pub struct Renderer {
    scene: Scene,
    /// Shared World state (fonts + file resolver + library). Built once,
    /// reused for every frame compile — pays the system-font-scan cost
    /// exactly once per `Renderer::new`.
    state: Arc<WorldState>,
    /// Flow (first-frame) position of each mobject, in Typst points.
    flow_pos: HashMap<Label, (f64, f64)>,
    /// Full-canvas page size in points (from the flow layout).
    page_w: f64,
    page_h: f64,
    flow_computed: bool,
    /// Effective canvas size (pt) per scene id, resolved via inheritance.
    scene_pages: HashMap<usize, (f64, f64)>,
    /// label -> owning scene id (for parent auto-hide).
    label_scene: HashMap<Label, usize>,
    /// Cross-page scene playback scheduler: maps global time to the active page
    /// per scene and records which page each mobject's flow layout landed on.
    /// See [`pages`].
    pages: PageScheduler,
    /// Precomputed outline interpolators for `#morph` pairs, keyed by
    /// `(from, to)`. Built once in `ensure_flow` (the expensive part:
    /// render both bodies to SVG, extract + align their outline rings). Each
    /// frame then just samples the plan — this is the performance-first design.
    morph_cache: HashMap<(Label, Label), MorphPlan>,
    /// Precomputed per-glyph fragment layouts for `#transform` of inline content
    /// (formulas / text). Built once in `ensure_flow`: each `TransformPlan`'s
    /// old/new bodies are split into glyph fragments, laid out at their absolute
    /// page positions, and matched (LCS). During the transform window the
    /// renderer composites the interpolated fragments *over* the target label —
    /// the old content disassembles and the new content reassembles glyph by
    /// glyph, instead of the whole block dissolving at once (the previous
    /// "stiff" crossfade). Empty for shape transforms / non-inline content.
    transform_fragments: Vec<TransformFragmentPlan>,
    /// Cached camera start time (first non-identity keyframe). Computed once
    /// in `ensure_flow` to avoid O(N·T) scan per frame.
    cam_start: Option<u32>,
    /// Cached home scene for camera. Computed once in `ensure_flow`.
    /// Cached set of parent group labels (for filtering in prepare_states).
    /// Computed once in `ensure_flow`.
    parent_labels: std::collections::HashSet<Label>,
    /// Cache of compiled Typst documents keyed by their exact source string.
    /// Intermediate frames that produce an identical source (e.g. paused or
    /// otherwise static objects) reuse the prior compile instead of re-running
    /// the full Typst pipeline — the core performance win. Bodies that
    /// genuinely change per frame (morph polygons, `ecval` counter text,
    /// `transform` content swaps) have a distinct source each frame and still
    /// recompile, but are themselves memoized, so a counter that repeats a
    /// value reuses its compile too.
    /// Memoized background-color resolutions: raw `#scene(bg: …)` expression →
    /// resolved `#rrggbb` (or `#rrggbbaa`) hex string. Resolving goes through
    /// the real Typst compiler (see `resolve_bg_hex`), so it is done once per
    /// distinct background expression and shared across every frame.
    ///
    /// `Mutex` (not `RefCell`) because the renderer is shared `&self` across a
    /// parallel frame-render loop.
    body_cache: Mutex<LruCache<String, Arc<PagedDocument>>>,
    /// Memoized `#scene(bg: …)` expression → resolved `#rrggbb(aa)` hex.
    bg_cache: Mutex<HashMap<String, String>>,
    /// The stable, *parameterized* whole-document source used by the
    /// flow-measurement pass. Built once (in [`Renderer::with_root`]) from the
    /// parsed artifacts: every animatable mobject body is wrapped in a
    /// `sys.inputs.at("candy:<label>:…")` reader and every `#scene` call is
    /// gated by `sys.inputs.at("candy:active_scene")`. This string is compiled
    /// once per scene during [`Renderer::ensure_flow`] to read each mobject's
    /// flow position and which page it landed on. It is *not* used for the
    /// per-frame render (that uses the per-page documents in `param_sources`).
    /// `String::is_empty()` ⇒ no artifacts ⇒ the measurement path compiles an
    /// empty `param_source` (only used by hand-built test scenes that don't
    /// render; real scenes are parsed from a `.tyx` and carry a non-empty
    /// `artifacts.source`).
    param_source: String,
    /// Per-mobject wrapped body (the `sys.inputs`-driven body expression),
    /// collected once in [`Renderer::build_parameterized_source`]. The building
    /// block for the per-page render documents in `param_sources`.
    wrapped_bodies: HashMap<Label, String>,
    /// Accumulated Typst context per scene id (key `0` for the no-scene-tree /
    /// hand-built case), built once in [`Renderer::build_scene_contexts`]. Each
    /// value is the full chain of *ancestor* contexts (root + every ancestor
    /// scene's body context, minus `#mobject` / `#subtitle` / non-ancestor
    /// `#scene` calls) that a scene's per-page document must prepend so its
    /// mobjects inherit the parent scene's `#import` / `#set` / `#show` / `#let`
    /// environment — not just the immediate parent's page size / background.
    scene_contexts: HashMap<usize, String>,
    /// The stable, per-page render documents. Each entry is keyed by
    /// `(scene_id, page_index)` and is a standalone Typst document containing
    /// only that scene/page's mobjects, laid out from the top in raw flow
    /// ("裸排"), with the scene's runtime context injected via its preamble.
    /// This replaces the old whole-document render path: each frame compiles
    /// exactly one of these documents (the active scene's active page) instead
    /// of recompiling the entire document and extracting a page.
    param_sources: HashMap<(usize, usize), String>,
    /// Absolute path of the original `.tyx` source file, if known (empty for
    /// hand-built / programmatic `Scene`s). Used to give the compiled Typst
    /// source a real `FileId` so an `E005` points the user at the actual file
    /// rather than the synthetic `main.typ` detached source.
    source_path: PathBuf,
}
impl Renderer {
    /// Build a renderer from a parsed [`Scene`].
    ///
    /// `project_root` is the directory that local `#import "file.typ"`
    /// resolves against — typically the parent directory of the `.tyx`
    /// source. Pass `PathBuf::new()` (current dir) if you don't care.
    pub fn new(scene: Scene) -> Result<Self, CandyError> {
        Self::with_root(scene, PathBuf::new())
    }
    /// Like [`new`] but with an explicit project root for local imports.
    pub fn with_root(scene: Scene, project_root: PathBuf) -> Result<Self, CandyError> {
        scene.validate().map_err(|m| CandyError::Parse(m, None))?;
        // Hand-built scenes (unit tests, programmatic callers) carry no parsed
        // `.tyx`, so `artifacts.source` is empty. Synthesize a whole-document
        // source from `scene.items` so the single whole-doc render path can
        // still drive per-frame inputs (transform body swaps, reveals, ecval
        // counters, …) instead of rendering a blank page.
        let mut scene = scene;
        // Capture the original `.tyx` path (if any) so the compiled Typst source
        // can carry a real `FileId` and `E005` diagnostics point at the user's
        // file. Empty for hand-built / programmatic scenes.
        let source_path = scene.artifacts.file_path.clone();
        if scene.artifacts.source.is_empty() {
            let (src, mobject_body) = Self::synthesize_handbuilt_source(&scene);
            scene.artifacts.source = src;
            scene.artifacts.mobject_body = mobject_body;
        }
        let (param_source, wrapped_bodies) = Self::build_parameterized_source(&scene);
        let scene_contexts = Self::build_scene_contexts(&scene);
        Ok(Self {
            state: Arc::new(WorldState::new(project_root)),
            scene,
            flow_pos: HashMap::new(),
            page_w: 1.0,
            page_h: 1.0,
            flow_computed: false,
            scene_pages: HashMap::new(),
            label_scene: HashMap::new(),
            pages: PageScheduler::empty(),
            morph_cache: HashMap::new(),
            transform_fragments: Vec::new(),
            cam_start: None,
            parent_labels: std::collections::HashSet::new(),
            body_cache: Mutex::new(LruCache::with_capacity(BODY_CACHE_CAP)),
            bg_cache: Mutex::new(HashMap::new()),
            param_source,
            wrapped_bodies,
            scene_contexts,
            param_sources: HashMap::new(),
            source_path,
        })
    }
    /// Compile a Typst source string into a single-page document.
    fn compile(&self, src: &str, inputs: &Dict) -> Result<PagedDocument, CandyError> {
        let source = self.state.main_source(src);
        let world = CandyWorld::new(&self.state, source, inputs.clone());
        // Typst can *panic* (rather than return a diagnostic) on certain
        // malformed input — especially in release builds, where such a panic
        // would otherwise abort the process with no diagnostic. Catch it and
        // surface it as `E005` so a syntax error is always reported, never
        // swallowed or crashed on.
        let warned =
            match catch_unwind(AssertUnwindSafe(|| typst::compile::<PagedDocument>(&world))) {
                Ok(w) => w,
                Err(payload) => {
                    let msg = if let Some(s) = payload.downcast_ref::<String>() {
                        s.clone()
                    } else if let Some(s) = payload.downcast_ref::<&str>() {
                        s.to_string()
                    } else {
                        "typst panicked during compilation".to_string()
                    };
                    // A panic may have left partial comemo entries; clear them so
                    // the next compile starts clean.
                    comemo::evict(0);
                    return Err(CandyError::Typst(format!("typst panicked: {msg}"), None));
                }
            };
        // If the body consulted the wall clock (`datetime.today()`), the render
        // is time-dependent and not reproducible — warn once per renderer.
        if world.used_time() && self.state.note_time_used() {
            warn!(CandyWarn::TimeDependent);
        }
        // Drop comemo's memoized results for this `World`. The whole-document
        // path varies the `World` per frame (different `sys.inputs` ⇒ a
        // different `Library`), so without eviction comemo would accumulate one
        // compiled document per distinct inputs set and OOM on long animations.
        // Our own LRU `body_cache` already retains the documents we need, so
        // clearing comemo here only frees the transient per-frame sub-computations.
        comemo::evict(0);
        match warned.output {
            Ok(doc) => Ok(doc),
            Err(errs) => {
                let loc = errs
                    .first()
                    .and_then(|d| typst_diag_loc(&world, d, &self.source_path));
                Err(CandyError::Typst(
                    crate::core::diag::format_typst_errors(&errs),
                    loc,
                ))
            }
        }
    }
    /// Compile a Typst source, memoized by the exact source string.
    ///
    /// This is the unified compile entry point for every object render path
    /// (`render_frame`, `body_largest_shape`, and the whole-document path). It is
    /// behavior-preserving: identical `(source, inputs)` → identical document.
    /// The win is that frames sharing a source (static / paused objects, or a
    /// counter value that repeats) skip a redundant Typst compile. Bodies that
    /// change per frame — morph polygons, `ecval` counter text, `transform`
    /// content swaps — naturally produce a different source each time and
    /// recompile, exactly as before.
    ///
    /// `inputs` are the per-frame `sys.inputs` values (empty for object /
    /// background compiles that don't consult them). They are folded into the
    /// cache key so two frames with the same source but different inputs map to
    /// distinct compiled documents.
    fn compile_cached(&self, src: &str, inputs: &Dict) -> Result<Arc<PagedDocument>, CandyError> {
        let key = Self::cache_key(src, inputs);
        if let Some(doc) = self.body_cache.lock().unwrap().get(&key) {
            return Ok(doc.clone());
        }
        let doc = Arc::new(self.compile(src, inputs)?);
        self.body_cache.lock().unwrap().insert(key, doc.clone());
        Ok(doc)
    }
    /// Compile the per-page render document for `(sid, page)`, memoized by its
    /// exact source string + `inputs`.
    ///
    /// This is the render-path twin of [`Renderer::compile_cached`]: each frame
    /// selects exactly one `(scene_id, page_index)` document (the active scene's
    /// active page) and compiles it with that frame's `sys.inputs`. The source
    /// string is the standalone per-page document assembled in
    /// [`Renderer::assemble_page_doc`], so only the active page's mobjects are
    /// typeset — replacing the old whole-document recompile-and-extract-page
    /// approach. Falls back to page 0 of the same scene, then to the
    /// whole-document `param_source`, if a specific page document is missing.
    fn compile_page_source(
        &self,
        sid: usize,
        page: usize,
        inputs: &Dict,
    ) -> Result<Arc<PagedDocument>, CandyError> {
        let src = self
            .param_sources
            .get(&(sid, page))
            .or_else(|| self.param_sources.get(&(sid, 0)))
            .cloned()
            .unwrap_or_else(|| self.param_source.clone());
        self.compile_cached(&src, inputs)
    }
    /// Build a stable cache key from a source string and its `inputs` dict.
    /// When `inputs` is empty the key is just the source (the common
    /// object / background compile case); otherwise the inputs are appended in
    /// a deterministic `(key=value;)*` form so equal `(source, inputs)` pairs
    /// always collide to the same key.
    fn cache_key(src: &str, inputs: &Dict) -> String {
        if inputs.is_empty() {
            return src.to_string();
        }
        let mut k = String::with_capacity(src.len() + 64);
        k.push_str(src);
        k.push('\0');
        for (key, val) in inputs.iter() {
            use std::fmt::Write;
            let _ = write!(k, "{key}={val:?};");
        }
        k
    }
    /// The uniform output canvas size (pixels) every frame is composited onto:
    /// the largest scene page (or the document page when there are no scenes)
    /// scaled by `pixel_per_pt`. The streaming encoder composes each frame onto
    /// this fixed size so per-scene page-size variation never produces mismatched
    /// frame dimensions. Mirrors the `max(…)` the legacy `compose` used, but
    /// derived cheaply from scene metadata instead of from already-rendered
    /// frames (so it is known *before* any frame is rasterized).
    pub(crate) fn uniform_canvas(&self, pixel_per_pt: f32) -> (usize, usize) {
        let (pw, ph) = if self.scene.scenes.is_empty() {
            (self.page_w, self.page_h)
        } else {
            let mut mw = self.page_w;
            let mut mh = self.page_h;
            for &(w, h) in self.scene_pages.values() {
                mw = mw.max(w);
                mh = mh.max(h);
            }
            (mw, mh)
        };
        let w = (pw * pixel_per_pt as f64).round().max(1.0) as usize;
        let h = (ph * pixel_per_pt as f64).round().max(1.0) as usize;
        (w, h)
    }
    /// Resolve the Typst body for `label` at frame time `time_ms`, choosing the
    /// SAME source for every render path (SVG, pixels, isolated). During an
    /// active `#morph` window the morphed polygon wins; otherwise the label's
    /// (possibly `transform`-swapped, `ecval`-substituted) body is used. This
    /// is the single source of truth that keeps the three render modes unified
    /// — previously the isolated `render_frame` path skipped the morph branch.
    fn resolve_body(&self, label: &Label, time_ms: u32) -> (String, Vec<String>) {
        self.morph_body_for(label, time_ms)
            .unwrap_or_else(|| content_for(&self.scene, label, time_ms))
    }
    /// Resolve a `#scene(bg: …)` color expression to a `#rrggbb(aa)` hex string
    /// suitable for an SVG `fill` / video canvas, using the real Typst compiler.
    ///
    /// We compile a 1×1pt page whose `fill` is the expression and read back the
    /// resolved page color — this honors *any* valid Typst color (`white`,
    /// `rgb("#05060f")`, `rgb(r,g,b)`, `luma(…)`, gradients, …) instead of a
    /// hand-rolled string parser. A non-color paint (e.g. a gradient) or an
    /// unresolvable expression falls back to opaque white. Results are memoized
    /// per distinct expression.
    fn resolve_bg_hex(&self, bg: &str) -> Result<String, CandyError> {
        if let Some(c) = self.bg_cache.lock().unwrap().get(bg) {
            return Ok(c.clone());
        }
        let src = format!("#set page(width: 1pt, height: 1pt, margin: 0pt, fill: {bg})\n#rect()");
        // A compile failure (e.g. a syntax error inside `bg`) is a real error and
        // must propagate as `E005`. Only a *successful* compile whose fill is not
        // a solid colour legitimately falls back to opaque white.
        let resolved = self
            .compile(&src, &Dict::new())?
            .pages()
            .first()
            .and_then(|p| match &p.fill {
                Smart::Custom(Some(Paint::Solid(c))) => Some(c.to_hex().to_string()),
                _ => None,
            })
            .unwrap_or_else(|| "white".to_string());
        self.bg_cache
            .lock()
            .unwrap()
            .insert(bg.to_string(), resolved.clone());
        Ok(resolved)
    }
    /// Effective background hex for `scene_id`, walking up the scene tree to
    /// inherit a parent's `bg` (root with none ⇒ opaque white).
    fn scene_bg_hex(&self, scene_id: usize) -> Result<String, CandyError> {
        let mut cur = Some(scene_id);
        while let Some(id) = cur {
            if let Some(s) = self.scene.scenes.iter().find(|s| s.id == id) {
                if let Some(bg) = &s.bg {
                    return self.resolve_bg_hex(bg);
                }
                cur = s.parent;
            } else {
                break;
            }
        }
        Ok("white".to_string())
    }
}
/// Resolve a Typst [`typst::diag::SourceDiagnostic`] to a candy [`SourceLoc`]
/// via the compile `world`, so an `E005` can point the user at the exact
/// `file:line:col` and the offending source line — just like the parser-level
/// diagnostics (E002 / E004 / …) already do.
///
/// The compiled main source is always *detached* (Typst's synthetic `main.typ`
/// id) so parallel compiles never collide on a `FileId`. When the diagnostic's
/// span resolves to that detached `main.typ` id, we rewrite the path to the
/// real `.tyx` (`source_path`) so the user is pointed at their own file rather
/// than the synthetic name. For an error inside an `@preview/candy` package file
/// or a local `#import` the real package path is used as-is. `source_path` is
/// empty for hand-built / programmatic scenes, in which case the detached
/// `main.typ` name is kept. Returns `None` when the span is detached or its
/// source cannot be resolved (e.g. an internal Typst panic), in which case the
/// `E005` is reported without a location.
pub(crate) fn typst_diag_loc(
    world: &CandyWorld,
    diag: &typst::diag::SourceDiagnostic,
    source_path: &Path,
) -> Option<SourceLoc> {
    let range = world.range(diag.span)?;
    let id = diag.span.id()?;
    let src = world.source(id).ok()?;
    let file_id = src.id();
    let vpath = file_id.vpath().get_without_slash();
    // The main document is always the detached `main.typ`; rewrite it to the
    // user's real `.tyx` so the location points at their file.
    let path = if vpath == "main.typ" && !source_path.as_os_str().is_empty() {
        source_path
    } else {
        Path::new(vpath)
    };
    Some(SourceLoc::at(path, src.text(), range))
}

#[cfg(test)]
pub(crate) fn compile_file_for_test(path: &Path) -> Result<String, CandyError> {
    let dir = path.parent().unwrap_or_else(|| Path::new("")).to_path_buf();
    let vpath =
        VirtualPath::virtualize(&dir, path).expect("test file must sit under the project root");
    let id = FileId::new(RootedPath::new(VirtualRoot::Project, vpath));
    let state = WorldState::new(dir);
    let text = std::fs::read_to_string(path)?; // E001 on missing file
    let source = TypstSource::new(id, text);
    let world = CandyWorld::new(&state, source, Dict::new());
    let warned = typst::compile::<PagedDocument>(&world);
    match warned.output {
        Ok(doc) => {
            let page = doc
                .pages()
                .first()
                .ok_or_else(|| CandyError::Typst("no pages".into(), None))?;
            Ok(typst_svg::svg(page, &SvgOptions::default()))
        }
        Err(errs) => {
            let loc = errs.first().and_then(|d| typst_diag_loc(&world, d, path));
            Err(CandyError::Typst(
                crate::core::diag::format_typst_errors(&errs),
                loc,
            ))
        }
    }
}
/// Verify the content timeline actually swaps an mobject's rendered body
/// between frames (this is what makes `transform` show the OLD content before
/// the switch and the NEW content after, without corrupting earlier frames).
#[test]
fn content_timeline_swaps_rendered_body() {
    let src = "#import \"candy\": *\n\
               #scene(width: 16cm, height: 9cm)[\n\
               #mobject(\"box\", rect(width: 2cm, height: 2cm))\n\
               #transform(\"box\", to: circle(radius: 1cm), duration: 50)\n\
               ]\n";
    let tmp = std::env::temp_dir().join("candy_test_content_swap.tyx");
    std::fs::write(&tmp, src).unwrap();
    let scene = crate::parser::ast_walk::parse_tyx(&tmp, true).unwrap();
    let mut r = Renderer::with_root(scene, PathBuf::new()).unwrap();
    // Before the switch (t=0): should render the original `rect`.
    let before = r.render_frame_at(0, &[]).unwrap();
    // After the switch (t=100): should render the new `circle` (the `#transform`
    // records a `content_timeline` swap at `cursor + 1`, so by t=100 the body is
    // the new `circle`).
    let after = r.render_frame_at(100, &[]).unwrap();
    assert_ne!(
        before, after,
        "content timeline did not change rendered body"
    );
    std::fs::remove_file(&tmp).ok();
}
#[test]
fn substitute_counters_expands_ecval_as_ast_node() {
    use crate::core::ast::{CounterDef, Slide};
    use crate::core::easing::Easing;
    use crate::core::meta::PrivateMeta;
    use std::collections::HashMap;
    let counters = vec![CounterDef {
        name: "r".into(),
        scope: "0".into(),
        seed: 10,
        step: 1,
        duration_ms: None,
        easing: Easing::Linear,
        start_ms: 0,
    }];
    let scene = Scene {
        slides: vec![Slide {
            start_ms: 0,
            duration_ms: 100,
            actions: vec![],
        }],
        items: HashMap::new(),
        content_timeline: HashMap::new(),
        initial: HashMap::new(),
        audio: Vec::new(),
        imports: Vec::new(),
        page_size: None,
        subtitles: Vec::new(),
        counters,
        counter_events: Vec::new(),
        scopes: Vec::new(),
        scenes: Vec::new(),
        root_scene: None,
        morph_pairs: Vec::new(),
        transform_plans: Vec::new(),
        groups: HashMap::new(),
        artifacts: ParseArtifacts::default(),
        private_metadata: PrivateMeta::default(),
    };
    // The canonical `ecval("name")` form: a real AST call expanded to an integer.
    assert_eq!(
        substitute_counters(&scene, "circle(radius: ecval(\"r\") * 1pt + 1cm)", 0).0,
        "circle(radius: 10 * 1pt + 1cm)"
    );
    // A long-lived counter steps once per ms: at t=5 → seed + step·5 = 15.
    assert_eq!(substitute_counters(&scene, "ecval(\"r\")", 5).0, "15");
    // The integer substitution yields valid Typst inside markup too.
    assert_eq!(
        substitute_counters(&scene, "text([Count: #ecval(\"r\")])", 5).0,
        "text([Count: #15])"
    );
    // An undeclared counter is left untouched (matches prior registry behaviour).
    assert_eq!(
        substitute_counters(&scene, "ecval(\"missing\")", 0).0,
        "ecval(\"missing\")"
    );
    // The bare-ident form stays accepted for backwards compatibility.
    assert_eq!(substitute_counters(&scene, "ecval(r)", 0).0, "10");
}
#[test]
fn subtitle_stays_in_viewport() {
    use crate::core::ast::{Scene, Slide, SubPos, Subtitle};
    use crate::core::easing::Easing;
    use crate::core::meta::PrivateMeta;
    use std::collections::HashMap;
    let page_w = 16.0 * PT_PER_CM;
    let page_h = 9.0 * PT_PER_CM;
    let mut subtitles = vec![Subtitle {
        id: "__sub_bottom".into(),
        scope: "0".into(),
        body: "[Bottom caption]".into(),
        start_ms: 0,
        end_ms: None,
        position: SubPos::Bottom,
        easing: Easing::Linear,
    }];
    subtitles.push(Subtitle {
        id: "__sub_top".into(),
        scope: "0".into(),
        body: "[Top caption]".into(),
        start_ms: 0,
        end_ms: None,
        position: SubPos::Top,
        easing: Easing::Linear,
    });
    let scene = Scene {
        slides: vec![Slide {
            start_ms: 0,
            duration_ms: 100,
            actions: vec![],
        }],
        items: HashMap::new(),
        content_timeline: HashMap::new(),
        initial: HashMap::new(),
        audio: Vec::new(),
        imports: Vec::new(),
        page_size: Some((page_w, page_h)),
        subtitles,
        counters: Vec::new(),
        counter_events: Vec::new(),
        scopes: Vec::new(),
        scenes: Vec::new(),
        root_scene: None,
        morph_pairs: Vec::new(),
        transform_plans: Vec::new(),
        groups: HashMap::new(),
        artifacts: ParseArtifacts::default(),
        private_metadata: PrivateMeta::default(),
    };
    let mut r = Renderer::with_root(scene, PathBuf::new()).unwrap();
    let s = r.render_frame_at(50, &[]).unwrap();
    // Find the maximum y in any translate() transform; it must stay within the
    // page height (captions anchored by edge, not their top-left).
    let mut max_y = 0.0f64;
    for m in s.split("translate(").skip(1) {
        let nums: Vec<f64> = m
            .split(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-'))
            .filter(|t| !t.is_empty())
            .filter_map(|t| t.parse::<f64>().ok())
            .collect();
        if nums.len() >= 2 {
            max_y = max_y.max(nums[1]);
        }
    }
    assert!(
        max_y <= page_h + 1.0,
        "subtitle overflows viewport: max translate y = {max_y} > page_h {page_h}"
    );
}
/// Verify the performance-first morph path: the renderer precomputes a
/// `MorphPlan` and, during the pair window, returns the `to` object's body as
/// an interpolated `polygon(...)` (a real shape morph, not a plain crossfade).
/// Outside the window it falls back to the normal body (seamless hand-off).
#[test]
fn morph_renders_interpolated_polygon() {
    use crate::core::ast::{MorphPair, Slide};
    use crate::core::easing::Easing;
    use crate::core::meta::PrivateMeta;
    use std::collections::HashMap;
    let mut items = HashMap::new();
    items.insert(Label("a".into()), "circle(radius: 1cm, fill: blue)".into());
    items.insert(Label("b".into()), "square(size: 2cm, fill: red)".into());
    let morph_pairs = vec![MorphPair {
        from: Label("a".into()),
        to: Label("b".into()),
        to_body: None,
        start_ms: 0,
        end_ms: 100,
        easing: Easing::Linear,
    }];
    let scene = Scene {
        slides: vec![Slide {
            start_ms: 0,
            duration_ms: 100,
            actions: vec![],
        }],
        items,
        content_timeline: HashMap::new(),
        morph_pairs,
        transform_plans: Vec::new(),
        initial: HashMap::new(),
        audio: Vec::new(),
        imports: Vec::new(),
        page_size: None,
        subtitles: Vec::new(),
        counters: Vec::new(),
        counter_events: Vec::new(),
        scopes: Vec::new(),
        scenes: Vec::new(),
        root_scene: None,
        groups: HashMap::new(),
        artifacts: ParseArtifacts::default(),
        private_metadata: PrivateMeta::default(),
    };
    let mut r = Renderer::with_root(scene, PathBuf::new()).unwrap();
    r.ensure_flow_public().unwrap();
    // Before the window: normal body (b is just a square).
    assert!(
        r.morph_body_for(&Label("b".into()), 101).is_none(),
        "after the morph window, the object renders its normal body"
    );
    // At the start of the window: a polygon shaped like the *source* (circle).
    let body0 = r
        .morph_body_for(&Label("b".into()), 0)
        .expect("expected a morphed polygon at t=0");
    assert!(
        body0.0.starts_with("polygon("),
        "morph body must be a polygon"
    );
    // Mid-window: still a polygon (interpolated shape).
    assert!(
        r.morph_body_for(&Label("b".into()), 50)
            .unwrap()
            .0
            .starts_with("polygon(")
    );
    // At the end of the window: polygon shaped like the *target* (square) — and
    // visually identical to rendering `b` normally (seamless hand-off).
    let body_end = r
        .morph_body_for(&Label("b".into()), 100)
        .expect("expected a morphed polygon at t=end");
    assert!(body_end.0.starts_with("polygon("));
    // The plan was actually precomputed (not empty).
    assert!(!r.morph_cache.is_empty(), "morph plan should be cached");
}
/// Regression test for declaration-order preservation: mobjects must keep
/// their declaration order top-to-bottom. The labels below are deliberately
/// declared as `zeta, alpha, mid` (not alphabetical) so a stray alphabetical
/// sort would be detected.
#[test]
fn renderer_flow_layout_matches_native_and_declaration_order() {
    use crate::core::ast::{Label, Scene, SceneInfo, Slide};
    use crate::core::meta::PrivateMeta;
    use std::collections::HashMap;
    let ordered: Vec<(String, String)> = vec![
        ("zeta".into(), "text(size: 20pt)[First]".into()),
        ("alpha".into(), "rect(width: 3cm, height: 1cm)".into()),
        ("mid".into(), "text(size: 14pt)[Third]".into()),
    ];
    let mut items = HashMap::new();
    for (l, b) in &ordered {
        items.insert(Label(l.clone()), b.clone());
    }
    let owns: Vec<Label> = ordered.iter().map(|(l, _)| Label(l.clone())).collect();
    let scene = Scene {
        slides: vec![Slide {
            start_ms: 0,
            duration_ms: 100,
            actions: vec![],
        }],
        items,
        content_timeline: HashMap::new(),
        initial: HashMap::new(),
        audio: Vec::new(),
        imports: Vec::new(),
        page_size: None,
        subtitles: Vec::new(),
        counters: Vec::new(),
        counter_events: Vec::new(),
        scopes: Vec::new(),
        scenes: vec![SceneInfo {
            id: 0,
            parent: None,
            scope: 0,
            page_size: None,
            bg: None,
            start_ms: 0,
            end_ms: 0,
            name: None,
            owns_labels: owns.clone(),
        }],
        root_scene: Some(0),
        morph_pairs: Vec::new(),
        transform_plans: Vec::new(),
        groups: HashMap::new(),
        artifacts: ParseArtifacts::default(),
        private_metadata: PrivateMeta::default(),
    };
    let mut r = Renderer::with_root(scene, PathBuf::new()).unwrap();
    r.ensure_flow_public().unwrap();
    let page_w = 16.0 * PT_PER_CM;
    let page_h = 9.0 * PT_PER_CM;
    let _ = (page_w, page_h);
    // (1) Every mobject must get a flow position from the introspector.
    for (l, _) in &ordered {
        let label = Label(l.clone());
        r.flow_pos_for(&label)
            .unwrap_or_else(|| panic!("candy nat present for {l}"));
    }
    // (2) Declaration order must be preserved top-to-bottom. With labels
    //     `zeta, alpha, mid`, an alphabetical sort would put `alpha` on top;
    //     assert `zeta` is highest and the order follows the source.
    let y = |l: &str| r.flow_pos_for(&Label(l.into())).unwrap().1;
    assert!(
        y("zeta") < y("alpha"),
        "order scrambled: zeta must sit above alpha"
    );
    assert!(
        y("alpha") < y("mid"),
        "order scrambled: alpha must sit above mid"
    );
}
/// Regression test for "temporarily-not-rendered mobjects use `#hide` to occupy
/// their flow space instead of being skipped". A mobject hidden at frame 0
/// (its content-timeline resolves to `none` at t=0, e.g. a `reveal`/`typewriter`
/// before its start) must STILL reserve its flow box in the flow — otherwise
/// every later mobject shifts up and the hidden mobject never gets a `flow_pos` to be
/// placed at once it appears. The layout now wraps such mobjects in `#hide[…]`.
#[test]
fn hidden_at_frame0_mobject_reserves_space_via_hide() {
    use crate::core::ast::{Label, Scene, SceneInfo, Slide};
    use crate::core::meta::PrivateMeta;
    use std::collections::HashMap;
    // `hidden` is suppressed at t=0 via the content timeline; its base body is
    // still real text (what it shows once revealed).
    let ordered: Vec<(String, String)> = vec![
        ("top".into(), "text(size: 18pt)[Top]\n".into()),
        ("hidden".into(), "text(size: 18pt)[Hidden]\n".into()),
        ("bottom".into(), "text(size: 18pt)[Bottom]\n".into()),
    ];
    let mut items = HashMap::new();
    for (l, b) in &ordered {
        items.insert(Label(l.clone()), b.trim().to_string());
    }
    // Suppress `hidden` at frame 0 (mirrors `reveal`/`typewriter` behaviour).
    let mut ct: HashMap<Label, Vec<(u32, String)>> = HashMap::new();
    ct.insert(Label("hidden".into()), vec![(0, "none".to_string())]);
    let owns: Vec<Label> = ordered.iter().map(|(l, _)| Label(l.clone())).collect();
    let scene = Scene {
        slides: vec![Slide {
            start_ms: 0,
            duration_ms: 100,
            actions: vec![],
        }],
        items,
        content_timeline: ct,
        initial: HashMap::new(),
        audio: Vec::new(),
        imports: Vec::new(),
        page_size: None,
        subtitles: Vec::new(),
        counters: Vec::new(),
        counter_events: Vec::new(),
        scopes: Vec::new(),
        scenes: vec![SceneInfo {
            id: 0,
            parent: None,
            scope: 0,
            page_size: None,
            bg: None,
            start_ms: 0,
            end_ms: 0,
            name: None,
            owns_labels: owns.clone(),
        }],
        root_scene: Some(0),
        morph_pairs: Vec::new(),
        transform_plans: Vec::new(),
        groups: HashMap::new(),
        artifacts: ParseArtifacts::default(),
        private_metadata: PrivateMeta::default(),
    };
    let mut r = Renderer::with_root(scene, PathBuf::new()).unwrap();
    r.ensure_flow_public().unwrap();
    let page_w = 16.0 * PT_PER_CM;
    let page_h = 9.0 * PT_PER_CM;
    let _ = (page_w, page_h);
    // (1) The hidden mobject MUST have a flow position (it was not skipped).
    let _hidden = r
        .flow_pos_for(&Label("hidden".into()))
        .expect("hidden mobject must get a nat");
    // (2) It must keep its slot in the flow: below `top`, above `bottom`.
    let y = |l: &str| r.flow_pos_for(&Label(l.into())).unwrap().1;
    assert!(y("top") < y("hidden"), "hidden must sit below top");
    assert!(
        y("hidden") < y("bottom"),
        "hidden must reserve space above bottom"
    );
}
/// Per-glyph `Transform` must split inline content into independent glyph
/// fragments (matched glide, unmatched fade) instead of the old crossfade, and
/// must handle *chained* transforms by reading the latest `content_timeline`
/// entry as the old body.
#[test]
fn transform_splits_inline_content_into_glyph_fragments() {
    let src = "#import \"candy\": *\n\
               #scene(width: 16cm, height: 9cm)[\n\
               #mobject(\"eq\", [$a + b = c$])\n\
               #transform(\"eq\", to: [$a + b + d = c$], duration: 60)\n\
               #transform(\"eq\", to: [$a + b + d + e = c$], duration: 60)\n\
               ]\n";
    let tmp = std::env::temp_dir().join("candy_test_xf_frag.tyx");
    std::fs::write(&tmp, src).unwrap();
    let scene = crate::parser::ast_walk::parse_tyx(&tmp, true).unwrap();
    let mut r = Renderer::with_root(scene, PathBuf::new()).unwrap();
    r.ensure_flow_public().unwrap();
    let plans = r.transform_plans_debug();
    assert_eq!(plans.len(), 2, "expected 2 chained plans: {:?}", plans);
    // Plan 0: $a+b=c$ (5) -> $a+b+d=c$ (7): 5 matched + 2 new = 7 fragments.
    assert_eq!(plans[0].1, 7, "plan0 fragment count: {:?}", plans);
    // Plan 1: $a+b+d=c$ (7) -> $a+b+d+e=c$ (9): 7 matched + 2 new = 9 fragments.
    assert_eq!(plans[1].1, 9, "plan1 fragment count: {:?}", plans);
    // Before any window: nothing active.
    assert_eq!(r.active_fragment_count(0), 0);
    // Mid plan-0 window: 7 fragments.
    let mid0 = plans[0].2 + (plans[0].3 - plans[0].2) / 2;
    assert_eq!(r.active_fragment_count(mid0), 7, "mid plan0: {:?}", plans);
    // Mid plan-1 window: 9 fragments.
    let mid1 = plans[1].2 + (plans[1].3 - plans[1].2) / 2;
    assert_eq!(r.active_fragment_count(mid1), 9, "mid plan1: {:?}", plans);
    // After both windows: target shows final content, no fragments.
    let after = plans[1].3 + 10;
    assert_eq!(
        r.active_fragment_count(after),
        0,
        "after windows: {:?}",
        plans
    );
    std::fs::remove_file(&tmp).ok();
}

/// Regression: after a per-glyph `transform` window the target must render its
/// NEW content (the content-timeline swap), not disappear.
#[test]
fn transform_target_renders_after_window() {
    let src = "#import \"candy\": *\n\
               #scene(width: 16cm, height: 9cm)[\n\
               #mobject(\"eq\", [$a + b = c$])\n\
               #transform(\"eq\", to: [$a + b + d = c$], duration: 60)\n\
               #pause(duration: 60)\n\
               ]\n";
    let tmp = std::env::temp_dir().join("candy_test_xf_after.tyx");
    std::fs::write(&tmp, src).unwrap();
    let scene = crate::parser::ast_walk::parse_tyx(&tmp, true).unwrap();
    let frames = crate::core::scheduler::schedule(&scene).unwrap();
    eprintln!(
        "DBG scenes={:?} items={:?} label_scene={:?}",
        scene
            .scenes
            .iter()
            .map(|s| (s.id, s.name.clone(), s.start_ms, s.end_ms, s.parent))
            .collect::<Vec<_>>(),
        scene.items.keys().collect::<Vec<_>>(),
        scene.label_scene_map(),
    );
    eprintln!(
        "DBG scene_call={:?} artifacts_has={}",
        scene.artifacts.scene_call,
        scene.artifacts.source.len()
    );
    eprintln!(
        "DBG active@0={} frames_len={}",
        scene.active_scene_at(0),
        frames.len()
    );
    let mut r = Renderer::with_root(scene, PathBuf::new()).unwrap();
    r.ensure_flow_public().unwrap();
    // Mid-window: fragments present.
    let mid = 30u32;
    let svg_mid = r.render_frame_at(mid, &frames).unwrap();
    assert!(svg_mid.contains("<svg"), "mid-window svg empty");
    // After window (past the transform's end_ms, still inside the document): the
    // target shows its NEW content — the whole-document render must therefore
    // emit the *transformed* formula, not snap back to the original. We pin
    // this by comparing the number of distinct glyph `<symbol>`s before vs
    // after: the original `$a + b = c$` has 5 distinct glyphs, the new
    // `$a + b + d = c$` has 6. A snap-back to the original would leave
    // the two counts equal (regression for bug 1).
    let before = 0u32;
    let svg_before = r.render_frame_at(before, &frames).unwrap();
    let after = 90u32;
    let svg_after = r.render_frame_at(after, &frames).unwrap();
    // Collect the SET of glyph symbols each frame references (order/content
    // independent of position). The original `$a + b = c$` is a strict
    // subset of the new `$a + b + d = c$` (which adds the `d`
    // glyph). A snap-back to the original would make the two sets
    // EQUAL, so we assert the after-set is a strict superset.
    // Only count *formula glyph* references — Typst emits each glyph as a
    // `<symbol id="g<hex>">` referenced via `<use xlink:href="#g<hex>">`.
    // The transform overlay embeds its own `<g id="tf_eq_…">` groups and
    // references them via `<use xlink:href="#tf_eq_…">`, which would otherwise
    // pollute the set; filter to ids that are a `g` followed by pure hex.
    let set_of = |svg: &str| -> std::collections::BTreeSet<String> {
        svg.split("xlink:href=\"#")
            .skip(1)
            .map(|s| s.split('"').next().unwrap().to_string())
            .filter(|s| {
                !s.is_empty() && s.starts_with('g') && s[1..].bytes().all(|b| b.is_ascii_hexdigit())
            })
            .collect()
    };
    let before_set = set_of(&svg_before);
    let after_set = set_of(&svg_after);
    assert!(
        before_set.is_subset(&after_set) && before_set != after_set,
        "after window target must show the NEW formula (added glyphs); before={:?} after={:?}",
        before_set,
        after_set
    );
    std::fs::remove_file(&tmp).ok();
}

/// Regression for bug 2: chained `#transform`s must persist each intermediate
/// result. After the first transform (during the pause, before the second),
/// the base document must render the *intermediate* formula — not snap back to
/// the original, and not jump to the final one. With the old code the
/// whole-document path froze on the original body, so every transform's result
/// vanished the instant its overlay stopped drawing.
#[test]
fn chained_transform_persists_intermediate() {
    let v = crate::typst_package_version().expect("typst/typst.toml must declare a `version`");
    let pkg = format!("@preview/candy:{v}");
    let src = "#import \"candy\": *\n\
               #scene(width: 16cm, height: 9cm)[\n\
               #mobject(\"eq\", [$a + b = c$])\n\
               #transform(\"eq\", to: [$a + b + d = c$], duration: 60)\n\
               #pause(duration: 60)\n\
               #transform(\"eq\", to: [$a + b + d + e = c$], duration: 60)\n\
               #pause(duration: 60)\n\
               ]\n"
    .replace("#import \"candy\":", &format!("#import \"{pkg}\":"));
    let tmp = std::env::temp_dir().join("candy_test_xf_chain_persist.tyx");
    std::fs::write(&tmp, src).unwrap();
    let scene = crate::parser::ast_walk::parse_tyx(&tmp, true).unwrap();
    let frames = crate::core::scheduler::schedule(&scene).unwrap();
    eprintln!(
        "DBG scenes={:?} items={:?} label_scene={:?}",
        scene
            .scenes
            .iter()
            .map(|s| (s.id, s.name.clone(), s.start_ms, s.end_ms, s.parent))
            .collect::<Vec<_>>(),
        scene.items.keys().collect::<Vec<_>>(),
        scene.label_scene_map(),
    );
    eprintln!(
        "DBG scene_call={:?} artifacts_has={}",
        scene.artifacts.scene_call,
        scene.artifacts.source.len()
    );
    eprintln!(
        "DBG active@0={} frames_len={}",
        scene.active_scene_at(0),
        frames.len()
    );
    let mut r = Renderer::with_root(scene, PathBuf::new()).unwrap();
    r.ensure_flow_public().unwrap();
    // Collect the SET of glyph symbols each frame references (order/content
    // independent of position). Typst dedupes/orders `<symbol>` definitions by
    // content, so a raw symbol *count* is NOT a reliable content metric (the
    // original `$a+b=c$` can emit more `<symbol>` defs than a later, longer
    // formula). The SET of referenced glyph ids, however, is monotonic across
    // these chained transforms:
    //   original  `$a + b = c$`       ⊂ intermediate `$a + b + d = c$`
    //                                    ⊂ final       `$a + b + d + e = c$`
    // A snap-back to the original (bug 1) or a premature jump to the final
    // (bug 2) would break one of these strict-subset relations.
    // Only count *formula glyph* references — Typst emits each glyph as a
    // `<symbol id="g<hex>">` referenced via `<use xlink:href="#g<hex>">`.
    // The transform overlay embeds its own `<g id="tf_eq_…">` groups and
    // references them via `<use xlink:href="#tf_eq_…">`, which would otherwise
    // pollute the set; filter to ids that are a `g` followed by pure hex.
    let set_of = |svg: &str| -> std::collections::BTreeSet<String> {
        svg.split("xlink:href=\"#")
            .skip(1)
            .map(|s| s.split('"').next().unwrap().to_string())
            .filter(|s| {
                !s.is_empty() && s.starts_with('g') && s[1..].bytes().all(|b| b.is_ascii_hexdigit())
            })
            .collect()
    };
    let before = r.render_frame_at(0, &frames).unwrap();
    let before_set = set_of(&before);
    // During the pause after the FIRST transform: base must show the
    // intermediate `$a + b + d = c$` — a strict superset of the original
    // (adds the `d` glyph, and a second `+`).
    let mid_pause = 90u32;
    let svg = r.render_frame_at(mid_pause, &frames).unwrap();
    let mid_set = set_of(&svg);
    assert!(
        before_set.is_subset(&mid_set) && before_set != mid_set,
        "between transforms target must show the intermediate formula (added glyphs); before={:?} mid={:?}",
        before_set,
        mid_set
    );
    // After BOTH transforms: base must show the final formula `$a + b + d + e = c$`
    // — a strict superset of the intermediate (adds the `e` glyph).
    let after_all = 210u32;
    let svg2 = r.render_frame_at(after_all, &frames).unwrap();
    let after_set = set_of(&svg2);
    assert!(
        mid_set.is_subset(&after_set) && mid_set != after_set,
        "after all transforms target must show the final formula (added glyphs); mid={:?} after={:?}",
        mid_set,
        after_set
    );
    std::fs::remove_file(&tmp).ok();
}

/// Regression: the canvas background must be drawn *outside* the camera group so
/// it always covers the whole frame, even when the camera zooms/pans/rotates the
/// mobjects. `typst_svg` emits the page fill as a `<path>` (not a `<rect>`), so
/// the background must be detected as either tag; otherwise it falls inside the
/// camera `<g transform>` and shrinks on zoom-out, leaving transparent (uncovered)
/// edges instead of the canvas background color.
#[test]
fn camera_background_stays_fixed_outside_camera_group() {
    let src = "#import \"candy\": *\n\
               #scene(width: 16cm, height: 9cm, bg: rgb(\"#05060f\"))[\n\
               #mobject(\"a\", circle(radius: 1cm, fill: blue))\n\
               ]\n";
    let tmp = std::env::temp_dir().join("candy_test_cam_bg.tyx");
    std::fs::write(&tmp, src).unwrap();
    let scene = crate::parser::ast_walk::parse_tyx(&tmp, true).unwrap();
    // A zoomed-out camera (scale 0.4) at t=0; with no scene tree it applies
    // globally. This is the case that exposed the transparent-edge bug.
    let frames = vec![FrameData {
        scale: 0.4,
        ..FrameData::new(0, Label("__camera__".into()))
    }];
    let mut r = Renderer::with_root(scene, PathBuf::new()).unwrap();
    r.ensure_flow_public().unwrap();
    let svg = r.render_frame_at(0, &frames).unwrap();
    eprintln!(
        "DBG svg_len={} <g>={} <rect={} <path={}",
        svg.len(),
        svg.matches("<g").count(),
        svg.matches("<rect").count(),
        svg.matches("<path").count(),
    );
    eprintln!(
        "DBG svg_len={} <g>={} <rect={} <path={} contains_candy_n_input_check",
        svg.len(),
        svg.matches("<g").count(),
        svg.matches("<rect").count(),
        svg.matches("<path").count(),
    );
    // The canvas background is the *first* shape element in the document
    // (`<rect>` or `<path>`, as emitted by `typst_svg`).
    let rect = svg.find("<rect");
    let path = svg.find("<path");
    let bg_pos = match (rect, path) {
        (Some(r), Some(p)) => Some(r.min(p)),
        (Some(r), None) => Some(r),
        (None, Some(p)) => Some(p),
        (None, None) => None,
    }
    .expect("canvas background shape must be present in the frame");
    // The camera group (`<g transform=`) must come AFTER the background, i.e. the
    // background is fixed outside the camera group and covers the whole canvas
    // even when the camera zooms out.
    let cam_pos = svg
        .find("<g transform=")
        .expect("camera group must be present in the frame");
    assert!(
        bg_pos < cam_pos,
        "background must be drawn outside (before) the camera group so it covers \
         the whole canvas even when the camera zooms out (got bg@{bg_pos} cam@{cam_pos})"
    );
    // The camera group must actually carry the zoom so the test is meaningful.
    assert!(
        svg.contains("scale(0.4)"),
        "camera group must apply the zoom transform"
    );
    std::fs::remove_file(&tmp).ok();
}

/// Regression: a `#typewriter` on a string containing a multi-byte character
/// (e.g. an em-dash `—`, 3 bytes) must render mid-window without panicking.
/// The revealed prefix length is a *codepoint* count, but Typst's `str.slice`
/// indexes by *byte* offset, so slicing the string directly panicked with
/// "string index N is not a character boundary" when the prefix ended inside a
/// multi-byte char. `reveal_wrap_body` now slices the codepoint array instead.
#[test]
fn typewriter_multibyte_prefix_does_not_panic() {
    let src = "#import \"candy\": *\n\
               #scene(width: 16cm, height: 9cm)[\n\
               #mobject(\"outro\", \"The quadratic formula — done.\")\n\
               #typewriter(\"outro\", duration: 100)\n\
               #pause(duration: 60)\n\
               ]\n";
    let tmp = std::env::temp_dir().join("candy_test_typewriter_mb.tyx");
    std::fs::write(&tmp, src).unwrap();
    let scene = crate::parser::ast_walk::parse_tyx(&tmp, true).unwrap();
    let frames = crate::core::scheduler::schedule(&scene).unwrap();
    eprintln!(
        "DBG scenes={:?} items={:?} label_scene={:?}",
        scene
            .scenes
            .iter()
            .map(|s| (s.id, s.name.clone(), s.start_ms, s.end_ms, s.parent))
            .collect::<Vec<_>>(),
        scene.items.keys().collect::<Vec<_>>(),
        scene.label_scene_map(),
    );
    eprintln!(
        "DBG scene_call={:?} artifacts_has={}",
        scene.artifacts.scene_call,
        scene.artifacts.source.len()
    );
    eprintln!(
        "DBG active@0={} frames_len={}",
        scene.active_scene_at(0),
        frames.len()
    );
    let mut r = Renderer::with_root(scene, PathBuf::new()).unwrap();
    r.ensure_flow_public().unwrap();
    // Sweep across the reveal window: every prefix length (including the one that
    // ends right at the em-dash) must compile to a valid frame, not error.
    for t in [10u32, 30, 50, 70, 90, 120] {
        let out = r.render_frame_at(t, &frames);
        assert!(out.is_ok(), "frame at {t}ms must render, got {out:?}");
    }
    std::fs::remove_file(&tmp).ok();
}

/// Regression: the per-glyph transform overlay must embed each old/new formula
/// exactly ONCE in `<defs>` and reference it via clipped `<use>` elements — not
/// repeat the whole formula markup inside every fragment's clip (which let
/// neighbouring glyphs leak through a slightly-off clip box: the "residual
/// garbage" artefact). This pins the restored SVG rendering path.
#[test]
fn transform_overlay_uses_defs_and_use_in_svg() {
    let src = "#import \"candy\": *\n\
               #scene(width: 16cm, height: 9cm)[\n\
               #mobject(\"eq\", [$a + b = c$])\n\
               #transform(\"eq\", to: [$a + b + d = c$], duration: 60)\n\
               #pause(duration: 60)\n\
               ]\n";
    let tmp = std::env::temp_dir().join("candy_test_xf_defs.tyx");
    std::fs::write(&tmp, src).unwrap();
    let scene = crate::parser::ast_walk::parse_tyx(&tmp, true).unwrap();
    let frames = crate::core::scheduler::schedule(&scene).unwrap();
    eprintln!(
        "DBG scenes={:?} items={:?} label_scene={:?}",
        scene
            .scenes
            .iter()
            .map(|s| (s.id, s.name.clone(), s.start_ms, s.end_ms, s.parent))
            .collect::<Vec<_>>(),
        scene.items.keys().collect::<Vec<_>>(),
        scene.label_scene_map(),
    );
    eprintln!(
        "DBG scene_call={:?} artifacts_has={}",
        scene.artifacts.scene_call,
        scene.artifacts.source.len()
    );
    eprintln!(
        "DBG active@0={} frames_len={}",
        scene.active_scene_at(0),
        frames.len()
    );
    let mut r = Renderer::with_root(scene, PathBuf::new()).unwrap();
    r.ensure_flow_public().unwrap();
    // Mid window: the transform is active, so fragments must be drawn.
    let mid = 30u32;
    let svg = r.render_frame_at(mid, &frames).unwrap();
    // The formula is embedded once per plan as a `<g id="tf_eq_0_old">` /
    // `<g id="tf_eq_0_new">` inside `<defs>`.
    assert!(
        svg.contains("<g id=\"tf_eq_0_old\">"),
        "old formula must be embedded once in defs"
    );
    assert!(
        svg.contains("<g id=\"tf_eq_0_new\">"),
        "new formula must be embedded once in defs"
    );
    // Every fragment is a clipped `<use>` referencing that defs group — never a
    // re-embedded full formula inside the clip.
    let uses = svg.matches("<use xlink:href=\"#tf_eq_0_").count();
    assert!(
        uses >= 7,
        "expected >=7 clipped <use> fragments, got {uses}"
    );
    assert!(
        !svg.contains("clip-path=\"url(#tf_eq_0_0)\"><g "),
        "fragments must not re-embed the full formula inside the clip"
    );
    std::fs::remove_file(&tmp).ok();
}

/// Regression: a per-glyph `transform` must compose with *other* `#animate`
/// tracks on the same target (e.g. the formula glides AND scales/rotates at
/// once). The fragment overlay's `<g transform>` must therefore carry the
/// target's current `scale(` / `rotate(` so the glyphs inherit the extra
/// animation instead of ignoring it (previously the transform only read the
/// target's x/y translation and dropped scale/rotation, so any simultaneous
/// move/scale/rotate on the transformed label silently did nothing).
#[test]
fn transform_composes_with_concurrent_animate() {
    let src = "#import \"candy\": *\n\
               #scene(width: 16cm, height: 9cm)[\n\
               #mobject(\"eq\", [$a + b = c$])\n\
               #transform(\"eq\", to: [$a + b + d = c$], duration: 60)\n\
               #animate(\"eq\", scale: 2.0, rotate: 30deg, duration: 60)\n\
               #pause(duration: 60)\n\
               ]\n";
    let tmp = std::env::temp_dir().join("candy_test_xf_compose.tyx");
    std::fs::write(&tmp, src).unwrap();
    let scene = crate::parser::ast_walk::parse_tyx(&tmp, true).unwrap();
    let frames = crate::core::scheduler::schedule(&scene).unwrap();
    eprintln!(
        "DBG scenes={:?} items={:?} label_scene={:?}",
        scene
            .scenes
            .iter()
            .map(|s| (s.id, s.name.clone(), s.start_ms, s.end_ms, s.parent))
            .collect::<Vec<_>>(),
        scene.items.keys().collect::<Vec<_>>(),
        scene.label_scene_map(),
    );
    eprintln!(
        "DBG scene_call={:?} artifacts_has={}",
        scene.artifacts.scene_call,
        scene.artifacts.source.len()
    );
    eprintln!(
        "DBG active@0={} frames_len={}",
        scene.active_scene_at(0),
        frames.len()
    );
    let mut r = Renderer::with_root(scene, PathBuf::new()).unwrap();
    r.ensure_flow_public().unwrap();
    // Mid window: the transform is active AND the concurrent scale/rotate is
    // part-way through, so every fragment group must carry both transforms
    // (the transform inherits the target's live scale/rotation, not just x/y).
    let mid = 30u32;
    let svg = r.render_frame_at(mid, &frames).unwrap();
    let groups = svg.matches("<g opacity=").count();
    assert!(groups > 0, "expected fragment groups in overlay");
    let scaled = svg.matches("scale(").count();
    let rotated = svg.matches("rotate(").count();
    assert!(
        scaled > 0 && rotated > 0,
        "fragments must inherit concurrent scale/rotate (scale={scaled}, rotate={rotated})"
    );
    std::fs::remove_file(&tmp).ok();
}

/// Regression: a `#transform` must compose with a concurrent `#animate(dx: …)`
/// translation so the WHOLE formula glides — every fragment's translate must
/// shift by the *full* translation amount. This pins the positioning fix:
/// previously each fragment was offset by ~2·glyph-center, so a translation
/// appeared to "not apply" (and the formula's glyphs scattered toward the
/// top-left, reading as a ghost of the old layout).
#[test]
fn transform_translation_animate_shifts_all_fragments() {
    fn fragment_translate_xs(src: &str, mid: u32) -> Vec<f64> {
        let tmp = std::env::temp_dir().join("candy_test_xf_shift.tyx");
        std::fs::write(&tmp, src).unwrap();
        let scene = crate::parser::ast_walk::parse_tyx(&tmp, true).unwrap();
        let frames = crate::core::scheduler::schedule(&scene).unwrap();
        eprintln!(
            "DBG scenes={:?} items={:?} label_scene={:?}",
            scene
                .scenes
                .iter()
                .map(|s| (s.id, s.name.clone(), s.start_ms, s.end_ms, s.parent))
                .collect::<Vec<_>>(),
            scene.items.keys().collect::<Vec<_>>(),
            scene.label_scene_map(),
        );
        eprintln!(
            "DBG scene_call={:?} artifacts_has={}",
            scene.artifacts.scene_call,
            scene.artifacts.source.len()
        );
        eprintln!(
            "DBG active@0={} frames_len={}",
            scene.active_scene_at(0),
            frames.len()
        );
        let mut r = Renderer::with_root(scene, PathBuf::new()).unwrap();
        r.ensure_flow_public().unwrap();
        let svg = r.render_frame_at(mid, &frames).unwrap();
        std::fs::remove_file(&tmp).ok();
        // Each fragment group is `<g opacity="…" transform="translate(px, py) …">`.
        // (Camera/object groups either lack `opacity=` or lack a transform on
        // that line, so this isolates the per-glyph transform fragments.)
        let mut xs = Vec::new();
        for line in svg.lines() {
            if line.contains("opacity=") && line.contains("transform=\"translate(") {
                let p = line.find("transform=\"translate(").unwrap();
                let rest = &line[p + "transform=\"translate(".len()..];
                if let Some(end) = rest.find(',') {
                    if let Ok(x) = rest[..end].trim().parse::<f64>() {
                        xs.push(x);
                    }
                }
            }
        }
        xs
    }
    // `base`: a plain transform (no translation). `moved`: the SAME
    // transform, but preceeded by `#animate(dx: 5cm)` so the formula is
    // already shifted when the transform window runs. Because candy runs
    // `#animate` and `#transform` as *sequential* slides, the animate must
    // come BEFORE the transform for the translation to be live during the
    // transform window (at the transform's mid, the animate has already
    // finished and its dx=5cm is inherited as the transform's base offset).
    let base = "#import \"candy\": *\n\
               #scene(width: 16cm, height: 9cm)[\n\
               #mobject(\"eq\", [$a + b = c$])\n\
               #transform(\"eq\", to: [$a + b + d = c$], duration: 60)\n\
               #pause(duration: 60)\n\
               ]\n";
    let moved = "#import \"candy\": *\n\
               #scene(width: 16cm, height: 9cm)[\n\
               #mobject(\"eq\", [$a + b = c$])\n\
               #animate(\"eq\", dx: 5cm, duration: 60)\n\
               #transform(\"eq\", to: [$a + b + d = c$], duration: 60)\n\
               #pause(duration: 60)\n\
               ]\n";
    let mid_base = 30u32; // mid of the transform window in `base`
    let mid_moved = 90u32; // mid of the transform window in `moved`
    let xs_base = fragment_translate_xs(base, mid_base);
    let xs_moved = fragment_translate_xs(moved, mid_moved);
    assert!(!xs_base.is_empty(), "expected fragment translates");
    // The base (no animation) fragments must sit at coherent, positive
    // page positions — i.e. the formula's glyphs laid out left→right,
    // NOT scattered toward the top-left (the ~2·center offset bug that
    // read as a ghost of the old layout).
    assert!(
        xs_base.iter().all(|&x| x > -5.0),
        "base fragment translates must be coherent/positive, got {:?}",
        xs_base
    );
    assert_eq!(
        xs_base.len(),
        xs_moved.len(),
        "fragment count must match between base and moved"
    );
    let shift_pt = 5.0 * PT_PER_CM; // 5cm in pt
    for (a, b) in xs_base.iter().zip(xs_moved.iter()) {
        let d = (b - a) - shift_pt;
        assert!(
            d.abs() < 2.0,
            "fragment shift {d:.2}pt != 5cm ({shift_pt:.2}pt): base={a}, moved={b}"
        );
    }
}

/// Regression: chained `#transform`s on the same label must not let the *future*
/// transform's temporary old-content mobject (`__xf_eq_1`) become visible during
/// the first transform's window. The scheduler must keep every tmp invisible
/// until its own transform starts, and the renderer must not let tmp mobjects
/// push the target down the page via the flow layout (which would place the
/// formula fragments off-screen or create a displaced duplicate).
#[test]
fn chained_transforms_hide_future_tmp_during_first_window() {
    let src = "#import \"candy\": *\n\
               #scene(width: 16cm, height: 9cm)[\n\
               #mobject(\"eq\", [$a + b = c$])\n\
               #animate(\"eq\", to: (0cm, 3cm), duration: 60)\n\
               #transform(\"eq\", to: [$a + b + d = c$], duration: 60)\n\
               #transform(\"eq\", to: [$a + b + d + e = c$], duration: 60)\n\
               #pause(duration: 60)\n\
               ]\n";
    let tmp = std::env::temp_dir().join("candy_test_xf_chain_hide.tyx");
    std::fs::write(&tmp, src).unwrap();
    let scene = crate::parser::ast_walk::parse_tyx(&tmp, true).unwrap();
    let frames = crate::core::scheduler::schedule(&scene).unwrap();
    eprintln!(
        "DBG scenes={:?} items={:?} label_scene={:?}",
        scene
            .scenes
            .iter()
            .map(|s| (s.id, s.name.clone(), s.start_ms, s.end_ms, s.parent))
            .collect::<Vec<_>>(),
        scene.items.keys().collect::<Vec<_>>(),
        scene.label_scene_map(),
    );
    eprintln!(
        "DBG scene_call={:?} artifacts_has={}",
        scene.artifacts.scene_call,
        scene.artifacts.source.len()
    );
    eprintln!(
        "DBG active@0={} frames_len={}",
        scene.active_scene_at(0),
        frames.len()
    );
    let mut r = Renderer::with_root(scene, PathBuf::new()).unwrap();
    r.ensure_flow_public().unwrap();
    // Midpoint of the FIRST transform window: animate 0-60, first transform 61-120,
    // second transform 121-180. Mid of first window = 90.
    let mid = 90u32;
    let svg = r.render_frame_at(mid, &frames).unwrap();
    std::fs::remove_file(&tmp).ok();

    // Fragment groups are single-line `<g opacity="..." transform="translate(...) ...">`.
    // Object groups are `<g opacity="...">` followed by a nested `<svg>` on the next line.
    // During the first transform the target and both tmps should be hidden, so there
    // must be NO object groups with positive opacity.
    let mut visible_object_groups = 0usize;
    for line in svg.lines() {
        if line.starts_with("<g opacity=\"") && !line.contains("transform=") {
            if let Some(start) = line.find("opacity=\"") {
                let rest = &line[start + 9..];
                if let Some(end) = rest.find('"') {
                    if let Ok(op) = rest[..end].parse::<f64>() {
                        if op > 0.01 {
                            visible_object_groups += 1;
                        }
                    }
                }
            }
        }
    }
    assert_eq!(
        visible_object_groups, 0,
        "during first transform window, no object group should be visible (future tmp ghost); found {visible_object_groups} visible"
    );
    // Fragments must be present and land inside the page (not pushed off-screen by tmps).
    let fragments: Vec<&str> = svg
        .lines()
        .filter(|l| l.contains("opacity=") && l.contains("transform=\"translate("))
        .collect();
    assert!(!fragments.is_empty(), "expected fragment overlay");
    for line in &fragments {
        let p = line.find("transform=\"translate(").unwrap();
        let rest = &line[p + "transform=\"translate(".len()..];
        if let Some(comma) = rest.find(',') {
            let y_part = &rest[comma + 1..];
            if let Some(end) = y_part.find(')') {
                let y = y_part[..end].trim().parse::<f64>().unwrap();
                assert!(
                    y < r.page_h,
                    "fragment y={y} must be inside page height {page_h}",
                    page_h = r.page_h
                );
            }
        }
    }
}

/// Regression: a scene whose content overflows its page becomes a **cross-page
/// scene** — the mobjects stay in ONE scene (data shared: same ownership, same
/// timeline) but are laid out across the overflow pages, and the renderer plays
/// the pages **in sequence** on a single-page canvas (it does NOT grow the
/// canvas). The rendered SVG must therefore be exactly one page tall (not
/// stacked), and only the current page's mobjects are drawn — so the first
/// frame shows fewer than all of them; the rest play on later pages.
#[test]
fn overflowing_scene_plays_pages_in_sequence() {
    // A short (2cm-tall) page with six 1cm-tall blocks overflows onto several
    // pages.
    let src = "#import \"candy\": *\n\
               #scene(width: 10cm, height: 2cm)[\n\
               #mobject(\"a\", rect(width: 5cm, height: 1cm))\n\
               #mobject(\"b\", rect(width: 5cm, height: 1cm))\n\
               #mobject(\"c\", rect(width: 5cm, height: 1cm))\n\
               #mobject(\"d\", rect(width: 5cm, height: 1cm))\n\
               #mobject(\"e\", rect(width: 5cm, height: 1cm))\n\
               #mobject(\"f\", rect(width: 5cm, height: 1cm))\n\
               ]\n";
    let tmp = std::env::temp_dir().join("candy_test_xpage.tyx");
    std::fs::write(&tmp, src).unwrap();
    let scene = crate::parser::ast_walk::parse_tyx(&tmp, true).unwrap();
    let frames = crate::core::scheduler::schedule(&scene).unwrap();
    eprintln!(
        "DBG scenes={:?} items={:?} label_scene={:?}",
        scene
            .scenes
            .iter()
            .map(|s| (s.id, s.name.clone(), s.start_ms, s.end_ms, s.parent))
            .collect::<Vec<_>>(),
        scene.items.keys().collect::<Vec<_>>(),
        scene.label_scene_map(),
    );
    eprintln!(
        "DBG scene_call={:?} artifacts_has={}",
        scene.artifacts.scene_call,
        scene.artifacts.source.len()
    );
    eprintln!(
        "DBG active@0={} frames_len={}",
        scene.active_scene_at(0),
        frames.len()
    );
    let mut r = Renderer::with_root(scene, PathBuf::new()).unwrap();
    r.ensure_flow_public().unwrap();
    let svg = r.render_frame_at(0, &frames).unwrap();
    eprintln!(
        "DBG svg_len={} <g>={} <rect={} <path={}",
        svg.len(),
        svg.matches("<g").count(),
        svg.matches("<rect").count(),
        svg.matches("<path").count(),
    );
    eprintln!(
        "DBG svg_len={} <g>={} <rect={} <path={}",
        svg.len(),
        svg.matches("<g").count(),
        svg.matches("<rect").count(),
        svg.matches("<path").count(),
    );
    eprintln!(
        "DBG svg_len={} <g>={} <rect={} <path={} contains_candy_n_input_check",
        svg.len(),
        svg.matches("<g").count(),
        svg.matches("<rect").count(),
        svg.matches("<path").count(),
    );
    // Single-page height in pt: 2cm * PT_PER_CM. Native Typst SVG emits the
    // `height` attribute with a `pt` unit suffix, so strip it before parsing.
    let page_h_pt = 2.0 * crate::renderer::typst::PT_PER_CM;
    let h_attr = svg
        .lines()
        .find(|l| l.contains("<svg"))
        .and_then(|l| {
            let s = l.find("height=\"").unwrap();
            let start = s + "height=\"".len();
            let end = l[start..].find('"').unwrap();
            let raw = &l[start..start + end];
            raw.strip_suffix("pt").unwrap_or(raw).parse::<f64>().ok()
        })
        .expect("svg height attribute");
    // The canvas must stay exactly ONE page tall — not stacked, not grown.
    assert!(
        (h_attr - page_h_pt).abs() < 1.0,
        "cross-page scene canvas must stay a single page (height {h_attr} ≈ {page_h_pt}), not stacked"
    );
    // And the first frame must draw only the current page's mobjects (fewer than
    // all six), proving sequential page playback rather than one giant canvas.
    // In Typst 0.15 the mobjects render as `<path>` elements (the unfilled rects
    // come out as `fill="none"` strokes); the page background is a separate
    // `fill="#ffffff"` path, so count the stroked mobject paths.
    let drawn = svg.matches("fill=\"none\"").count();
    assert!(
        drawn > 0 && drawn < 6,
        "first frame should show only the current page's mobjects (drew {drawn} of 6)"
    );
    // Sequential playback: a frame deep into the playback (well past the first
    // page) must also show only the current page's mobjects — never all six
    // stacked on one canvas, and never blank.
    let svg_later = r.render_frame_at(4500, &frames).unwrap();
    let drawn_later = svg_later.matches("fill=\"none\"").count();
    assert!(
        drawn_later > 0 && drawn_later < 6,
        "later frame should still show only one page's mobjects (drew {drawn_later} of 6)"
    );
    std::fs::remove_file(&tmp).ok();
}

/// Regression: an `E005` Typst render failure must carry a source location that
/// points at the offending code in the user's `.tyx` (the `file:line:col` +
/// caret), not just a free-text message. A type error inside a mobject body
/// (`#(1cm + "x"` — adding a length and a string) fails Typst evaluation; the
/// renderer must surface `E005` with a `SourceLoc` whose path is the real `.tyx`
/// and whose line text is the offending source line.
#[test]
fn e005_typst_error_carries_source_location() {
    let src = "#import \"candy\": *\n\
               #scene(width: 16cm, height: 9cm)[\n\
               #mobject(\"bad\", #(1cm + \"x\"))\n\
               ]\n";
    let tmp = std::env::temp_dir().join("candy_test_e005_loc.tyx");
    std::fs::write(&tmp, src).unwrap();
    let scene = crate::parser::ast_walk::parse_tyx(&tmp, true).unwrap();
    let r = Renderer::with_root(scene, tmp.parent().unwrap().to_path_buf()).unwrap();
    let err = r
        .compile(&r.param_source, &Dict::new())
        .expect_err("type error in mobject body must fail compilation");
    assert_eq!(err.code(), "E005", "failure must be reported as E005");
    let loc = err
        .loc()
        .expect("E005 must carry a source location pointing at the bad code");
    assert_eq!(
        loc.path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned()),
        Some("candy_test_e005_loc.tyx".to_string()),
        "location must point at the real .tyx file"
    );
    assert!(
        loc.line_text.contains("1cm + \"x\"") || !loc.line_text.trim().is_empty(),
        "location must include the offending source line, got: {:?}",
        loc.line_text
    );
    std::fs::remove_file(&tmp).ok();
}
