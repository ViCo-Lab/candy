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
//! * [`matrix`] — 2-D affine matrix math for the camera warp.
//! * [`camera`] — the global `#camera` pan/zoom/rotate transform + warp.
//! * [`composite`] — alpha compositing ("over"), offset paste, formula crop,
//!   and formula-id localization for the per-glyph transform path.
//! * [`morph`] — shape-`#morph` rendering helpers: ring localization, SVG
//!   color → Typst conversion, and `polygon(...)` source generation.
//! * [`pages`] — cross-page scene playback: the per-scene page schedule that
//!   maps global time to the active page (sequential page playback with frozen
//!   timelines for the not-yet-active pages).
//! * [`transform`] — per-glyph `#transform` plan types and the body placement
//!   source builder.
pub(crate) mod camera;
pub(crate) mod composite;
pub(crate) mod content;
pub(crate) mod matrix;
pub(crate) mod morph;
pub(crate) mod pages;
pub(crate) mod svg;
pub(crate) mod transform;
pub(crate) mod world;
// Re-export the helper items so the `Renderer` impl below can call them with
// the same unqualified names as before the split.
pub(crate) use self::camera::*;
pub(crate) use self::composite::*;
pub(crate) use self::content::*;
pub(crate) use self::morph::*;
pub(crate) use self::pages::*;
pub(crate) use self::svg::*;
pub(crate) use self::transform::*;
pub(crate) use self::world::*;
use crate::core::ast::{FrameData, Label, Scene, Subtitle};
use crate::core::diag::{CandyError, CandyWarn};
use crate::warn;
use crate::core::morph::{MorphPlan, extract_shapes_from_svg, polygon_area};
use std::collections::HashMap;
use std::hash::Hash;
#[cfg(test)]
use std::path::Path;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use typst_layout::PagedDocument;
use typst_library::foundations::Smart;
use typst_library::visualize::Paint;
use typst_render::{RenderOptions, render};
use typst_svg::SvgOptions;
#[cfg(test)]
use typst_syntax::FileId;
use typst_syntax::Source as TypstSource;
#[cfg(test)]
use typst_syntax::{RootedPath, VirtualPath, VirtualRoot};
use typst_utils::Scalar;
/// Centimeters per Typst point (1pt = 1/72in, 1in = 2.54cm).
pub(crate) const PT_PER_CM: f64 = 28.346_456_692_913_385;
/// Maximum segment length (in Typst points) when bisecting morph outline rings.
/// Smaller = smoother morph but more points (the plan is sampled per frame, so
/// the per-frame cost is linear in the point count — 3pt is a good balance).
const MORPH_MAX_SEGMENT: f64 = 3.0;
#[derive(Hash, PartialEq, Eq, Clone)]
struct SpriteKey {
    label: String,
    body_hash: u64,
    scale_q: u32,
    rot_q: u32,
    x_q: u32,
    y_q: u32,
    ppi_q: u32,
}
/// Renders a [`Scene`] into frames, with auto-detected mobject positions.
pub struct Renderer {
    scene: Scene,
    /// Shared World state (fonts + file resolver + library). Built once,
    /// reused for every frame compile — pays the system-font-scan cost
    /// exactly once per `Renderer::new`.
    state: Arc<WorldState>,
    /// Natural (first-frame) position of each mobject, in Typst points.
    nat: HashMap<Label, (f64, f64)>,
    /// Full-canvas page size in points (from the natural document).
    page_w: f64,
    page_h: f64,
    natural_computed: bool,
    /// Effective canvas size (pt) per scene id, resolved via inheritance.
    scene_pages: HashMap<usize, (f64, f64)>,
    /// label -> owning scene id (for parent auto-hide).
    label_scene: HashMap<Label, usize>,
    /// Cross-page scene playback scheduler: maps global time to the active page
    /// per scene and records which page each mobject's natural layout landed on.
    /// See [`pages`].
    pages: PageScheduler,
    /// Precomputed outline interpolators for `#morph` pairs, keyed by
    /// `(from, to)`. Built once in `ensure_natural` (the expensive part:
    /// render both bodies to SVG, extract + align their outline rings). Each
    /// frame then just samples the plan — this is the performance-first design.
    morph_cache: HashMap<(Label, Label), MorphPlan>,
    /// Precomputed per-glyph fragment layouts for `#transform` of inline content
    /// (formulas / text). Built once in `ensure_natural`: each `TransformPlan`'s
    /// old/new bodies are split into glyph fragments, laid out at their absolute
    /// page positions, and matched (LCS). During the transform window the
    /// renderer composites the interpolated fragments *over* the target label —
    /// the old content disassembles and the new content reassembles glyph by
    /// glyph, instead of the whole block dissolving at once (the previous
    /// "stiff" crossfade). Empty for shape transforms / non-inline content.
    transform_fragments: Vec<TransformFragmentPlan>,
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
    body_cache: Mutex<HashMap<String, Arc<PagedDocument>>>,
    /// Per-object rasterized-sprite cache. Keyed by the effective render state
    /// (label + body source + quantized scale/rotation/position + ppi). This is
    /// the *second* performance layer on top of `body_cache`: even after the
    /// Typst source is memoized, rasterizing it to RGBA (`render`) is expensive,
    /// so identical states reuse the previously rasterized frame. The page
    /// size / canvas is constant, so the cached `RenderedFrame` composites
    /// directly. `Mutex` for the same `&self`-shared reason as `body_cache`.
    sprite_cache: Mutex<HashMap<SpriteKey, Arc<crate::renderer::RenderedFrame>>>,
    /// Memoized `#scene(bg: …)` expression → resolved `#rrggbb(aa)` hex.
    bg_cache: Mutex<HashMap<String, String>>,
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
        scene.validate().map_err(CandyError::Parse)?;
        Ok(Self {
            state: Arc::new(WorldState::new(project_root)),
            scene,
            nat: HashMap::new(),
            page_w: 1.0,
            page_h: 1.0,
            natural_computed: false,
            scene_pages: HashMap::new(),
            label_scene: HashMap::new(),
            pages: PageScheduler::empty(),
            morph_cache: HashMap::new(),
            transform_fragments: Vec::new(),
            body_cache: Mutex::new(HashMap::new()),
            sprite_cache: Mutex::new(HashMap::new()),
            bg_cache: Mutex::new(HashMap::new()),
        })
    }
    /// Compile a Typst source string into a single-page document.
    fn compile(&self, src: &str) -> Result<PagedDocument, CandyError> {
        let source = TypstSource::detached(src.to_string());
        let world = CandyWorld::new(&self.state, source);
        let warned = typst::compile::<PagedDocument>(&world);
        // If the body consulted the wall clock (`datetime.today()`), the render
        // is time-dependent and not reproducible — warn once per renderer.
        if world.used_time() && self.state.note_time_used() {
            warn!(CandyWarn::TimeDependent);
        }
        warned
            .output
            .map_err(|errs| CandyError::Typst(format!("{:?}", errs)))
    }
    /// Compile a Typst source, memoized by the exact source string.
    ///
    /// This is the unified compile entry point for every object render path
    /// (`render_object_svg`, `render_object_pixels`, `render_frame`). It is
    /// behavior-preserving: identical source → identical document. The win is
    /// that frames sharing a source (static / paused objects, or a counter
    /// value that repeats) skip a redundant Typst compile. Bodies that change
    /// per frame — morph polygons, `ecval` counter text, `transform` content
    /// swaps — naturally produce a different source each time and recompile,
    /// exactly as before.
    fn compile_cached(&self, src: &str) -> Result<Arc<PagedDocument>, CandyError> {
        if let Some(doc) = self.body_cache.lock().unwrap().get(src) {
            return Ok(doc.clone());
        }
        let doc = Arc::new(self.compile(src)?);
        self.body_cache
            .lock()
            .unwrap()
            .insert(src.to_string(), doc.clone());
        Ok(doc)
    }
    /// Resolve the Typst body for `label` at frame time `time_ms`, choosing the
    /// SAME source for every render path (SVG, pixels, isolated). During an
    /// active `#morph` window the morphed polygon wins; otherwise the label's
    /// (possibly `transform`-swapped, `ecval`-substituted) body is used. This
    /// is the single source of truth that keeps the three render modes unified
    /// — previously the isolated `render_frame` path skipped the morph branch.
    fn resolve_body(&self, label: &Label, time_ms: u32) -> String {
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
    fn resolve_bg_hex(&self, bg: &str) -> String {
        if let Some(c) = self.bg_cache.lock().unwrap().get(bg) {
            return c.clone();
        }
        let resolved = {
            let src =
                format!("#set page(width: 1pt, height: 1pt, margin: 0pt, fill: {bg})\n#rect()");
            self.compile(&src)
                .ok()
                .and_then(|doc| {
                    doc.pages().first().and_then(|p| match &p.fill {
                        Smart::Custom(Some(Paint::Solid(c))) => Some(c.to_hex().to_string()),
                        _ => None,
                    })
                })
                .unwrap_or_else(|| "white".to_string())
        };
        self.bg_cache
            .lock()
            .unwrap()
            .insert(bg.to_string(), resolved.clone());
        resolved
    }
    /// Effective background hex for `scene_id`, walking up the scene tree to
    /// inherit a parent's `bg` (root with none ⇒ opaque white).
    fn scene_bg_hex(&self, scene_id: usize) -> String {
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
        "white".to_string()
    }
    /// Parse a `#rrggbb(aa)` hex (or a bare `#rgb`) into `(r, g, b, a)`, with
    /// full opacity as the default alpha. Used to seed the video canvas.
    fn hex_to_rgba(bg: &str) -> [u8; 4] {
        let h = bg.trim_start_matches('#');
        let bytes = match h.len() {
            3 => {
                let r = h.as_bytes()[0];
                let g = h.as_bytes()[1];
                let b = h.as_bytes()[2];
                vec![
                    Self::hex_digit(r),
                    Self::hex_digit(g),
                    Self::hex_digit(b),
                    255u8,
                ]
            }
            6 => {
                let r = u8::from_str_radix(&h[0..2], 16).unwrap_or(255);
                let g = u8::from_str_radix(&h[2..4], 16).unwrap_or(255);
                let b = u8::from_str_radix(&h[4..6], 16).unwrap_or(255);
                vec![r, g, b, 255]
            }
            8 => {
                let r = u8::from_str_radix(&h[0..2], 16).unwrap_or(255);
                let g = u8::from_str_radix(&h[2..4], 16).unwrap_or(255);
                let b = u8::from_str_radix(&h[4..6], 16).unwrap_or(255);
                let a = u8::from_str_radix(&h[6..8], 16).unwrap_or(255);
                vec![r, g, b, a]
            }
            _ => vec![255, 255, 255, 255],
        };
        [bytes[0], bytes[1], bytes[2], bytes[3]]
    }
    /// Map a single hex digit (`0-9`, `a-f`, `A-F`) to its 0–15 value, doubling
    /// it to a 0–255 byte for `#rgb` shorthand expansion. Unknown digits → 15.
    fn hex_digit(b: u8) -> u8 {
        match b {
            b'0'..=b'9' => b - b'0',
            b'a'..=b'f' => b - b'a' + 10,
            b'A'..=b'F' => b - b'A' + 10,
            _ => 15,
        }
        .saturating_mul(17)
    }
    /// Compose a parent transform onto a child transform (group support).
    /// Both are deltas from their respective natural positions; the result is
    /// the child's effective transform with the parent's pan/zoom/rotate applied
    /// to the child's local offset.
    fn compose_transforms(parent: &FrameData, child: &FrameData) -> FrameData {
        let r = parent.rotation.to_radians();
        let (s, c) = (r.sin(), r.cos());
        let lx = child.x * c - child.y * s;
        let ly = child.x * s + child.y * c;
        FrameData {
            time_ms: child.time_ms,
            target: child.target.clone(),
            x: parent.x + lx * parent.scale,
            y: parent.y + ly * parent.scale,
            scale: parent.scale * child.scale,
            opacity: (parent.opacity * child.opacity).clamp(0.0, 1.0),
            rotation: parent.rotation + child.rotation,
            easing: child.easing.clone(),
        }
    }
    /// Resolve the effective per-frame transform of `label`, walking up the
    /// group parent chain (cycle-guarded) and composing each ancestor.
    fn effective_state(&self, label: &Label, states: &HashMap<Label, FrameData>) -> FrameData {
        // Build the ancestor chain root → … → immediate parent → label.
        let mut chain = vec![label.clone()];
        let mut seen = std::collections::HashSet::new();
        let mut cur = label.clone();
        while let Some(p) = self.scene.groups.get(&cur) {
            if !seen.insert(p.clone()) {
                break; // cycle guard
            }
            chain.push(p.clone());
            cur = p.clone();
        }
        chain.reverse(); // root first
        let mut combined = states
            .get(&chain[0])
            .cloned()
            .unwrap_or_else(|| self.initial_for(chain[0].clone(), 0));
        for anc in chain.iter().skip(1) {
            let child = states
                .get(anc)
                .cloned()
                .unwrap_or_else(|| self.initial_for(anc.clone(), 0));
            combined = Self::compose_transforms(&combined, &child);
        }
        combined
    }
    /// Build the per-frame object states: seed from `all_frames` + `scene.items`,
    /// then apply group (parent→child) transform composition. Returns the map of
    /// label → effective transform (excluding the synthetic camera and any
    /// synthetic group-parent containers, which are never drawn), plus the
    /// camera state if present.
    fn prepare_states(
        &self,
        all_frames: &[FrameData],
        time_ms: u32,
    ) -> (HashMap<Label, FrameData>, Option<FrameData>) {
        let mut states: HashMap<Label, FrameData> = HashMap::new();
        for f in all_frames {
            if f.time_ms <= time_ms {
                states
                    .entry(f.target.clone())
                    .and_modify(|e| {
                        if f.time_ms >= e.time_ms {
                            *e = f.clone();
                        }
                    })
                    .or_insert_with(|| f.clone());
            }
        }
        for label in self.scene.items.keys() {
            states
                .entry(label.clone())
                .or_insert_with(|| self.initial_for(label.clone(), time_ms));
        }
        // Camera is a synthetic mobject; extract and remove it from the draw set.
        let mut camera = states.get(&Label(CAMERA_LABEL.into())).cloned();
        states.remove(&Label(CAMERA_LABEL.into()));
        // A `#camera` directive is *scene-scoped*: it only transforms the scene
        // in which it is declared. Once that scene ends, the camera returns to
        // identity so a pan/zoom/rotate from an earlier scene does not leak into
        // later scenes (which would shift/scale/rotate content that should sit
        // at its plain-Typst position). This mirrors the per-scene reset used
        // for every other animated property. With no scene tree the camera
        // applies globally (legacy behaviour).
        //
        // The camera's "home scene" is the scene active at the camera's *first*
        // keyframe (the directive's start). We apply the camera only while the
        // current frame's active scene is that same home scene. (The scheduler
        // also snapshots every item — including the synthetic camera — at the
        // document's final frame, which would otherwise pin the camera transform
        // to the very end; keying off the *first* keyframe avoids that trap.)
        if !self.scene.scenes.is_empty() {
            if camera.is_some() {
                // Home scene = the scene active when the camera *animation*
                // actually begins. The interpolator expands the sparse camera
                // keyframes into one dense frame per sample time, and the
                // scheduler also seeds `__camera__` with an identity keyframe at
                // `time_ms = 0`, so we can't just take the minimum keyframe time.
                // Instead we locate the first frame where the camera deviates
                // from identity — that is the start of the `#camera` directive —
                // and use its home scene. The camera then applies only while the
                // current frame's active scene is that same home scene; once the
                // scene ends it returns to identity (no leak into later scenes).
                let is_nonidentity = |f: &FrameData| {
                    f.x.abs() > 1e-6
                        || f.y.abs() > 1e-6
                        || (f.scale - 1.0).abs() > 1e-6
                        || f.rotation.abs() > 1e-6
                };
                let cam_start = all_frames
                    .iter()
                    .filter(|f| f.target.0 == CAMERA_LABEL && is_nonidentity(f))
                    .map(|f| f.time_ms)
                    .min()
                    .unwrap_or(0);
                let home = self.scene.active_scene_at(cam_start);
                let active = self.scene.active_scene_at(time_ms);
                if active != home {
                    camera = None;
                }
            }
        }
        // Synthetic group parents (empty body) are containers, not drawn.
        let parent_labels: std::collections::HashSet<&Label> = self.scene.groups.values().collect();
        let mut out: HashMap<Label, FrameData> = HashMap::new();
        for label in states.keys() {
            if parent_labels.contains(label)
                && self
                    .scene
                    .items
                    .get(label)
                    .map_or(false, |b| b.trim() == "none")
            {
                continue;
            }
            out.insert(label.clone(), self.effective_state(label, &states));
        }
        (out, camera)
    }
    /// Compute (once) the natural layout of every mobject.
    ///
    /// Each scene's objects are laid out by plain Typst document flow — every
    /// mobject is emitted as an ordinary block-level Typst object on its own
    /// line, so Typst stacks them top-to-bottom (left-aligned) with its standard
    /// spacing, exactly as the same bodies would land if written directly in a
    /// Typst source. This is what makes `#play` beats and `mobject` text appear
    /// at their "standard mode" positions instead of all piling up at the page
    /// origin (0, 0), and it stays consistent with how hand-written Typst would
    /// typeset the same content (no synthetic `#stack` gap or centring).
    ///
    /// We cannot rely on Typst's `data-typst-label` SVG attribute — the
    /// `typst_svg` exporter used here does not emit it — so instead we measure
    /// each object's bounding box by wrapping it in a uniquely-coloured block
    /// and reading the footprint back from the single rendered layout SVG.
    fn ensure_natural(&mut self) -> Result<(), CandyError> {
        if self.natural_computed {
            return Ok(());
        }
        // Use the page size from the .tyx source if set (`#set page(width:..,
        // height:..)`), otherwise default to 16:9 (16cm × 9cm).
        let (page_w_cm, page_h_cm) = self
            .scene
            .page_size
            .map(|(w, h)| (w / PT_PER_CM, h / PT_PER_CM))
            .unwrap_or((16.0, 9.0));
        self.page_w = page_w_cm * PT_PER_CM;
        self.page_h = page_h_cm * PT_PER_CM;
        let preamble = imports_preamble(&self.scene);
        // Group labels by the scene that owns them, preserving declaration order
        // within each scene. Legacy single-scene documents (no `scenes`) lay out
        // every item on one page.
        let mut by_scene: Vec<(usize, (f64, f64), Vec<Label>)> = Vec::new();
        if self.scene.scenes.is_empty() {
            let mut labels: Vec<Label> = self.scene.items.keys().cloned().collect();
            labels.sort_by(|a, b| a.0.cmp(&b.0));
            by_scene.push((0, (self.page_w, self.page_h), labels));
        } else {
            for s in &self.scene.scenes {
                let pg = self.scene.effective_page_pt(s.id);
                // `owns_labels` is in declaration order, which matches the
                // top-to-bottom document flow we want to reproduce.
                let ls: Vec<Label> = s.owns_labels.clone();
                by_scene.push((s.id, pg, ls));
            }
        }
        let mut nat: HashMap<Label, (f64, f64)> = HashMap::new();
        // Number of pages each scene's natural layout spilled onto. A scene that
        // overflows its single page becomes a *cross-page scene*: its mobjects
        // stay in ONE scene (data shared) but are laid out across several pages,
        // and the renderer plays the pages in sequence (see [`pages`]).
        let mut scene_page_counts: HashMap<usize, usize> = HashMap::new();
        // label -> the page (0-based) its natural layout landed on. Fed to the
        // page scheduler so it can partition each scene's timeline by page.
        let mut page_of: HashMap<Label, usize> = HashMap::new();
        for (sid, (pw, ph), labels) in &by_scene {
            // Native layout pass: each mobject is wrapped in a content-sized,
            // block-level coloured `block` and emitted in plain document flow.
            // Typst's own block flow stacks them top-to-bottom (left-aligned) with
            // its standard spacing — exactly where the same bodies would land if
            // written directly in a Typst source. This is faithful to "standard
            // Typst" layout, unlike `#stack(dir: ttb, spacing: …)`, which imposes a
            // synthetic fixed gap and centring that do not match plain Typst.
            if labels.is_empty() {
                continue;
            }
            let mut palette: Vec<(Label, String)> = Vec::new();
            let mut blocks = String::new();
            for (i, label) in labels.iter().enumerate() {
                let natural = self.scene.items.get(label).map(|s| s.trim()).unwrap_or("");
                // Frame-0 body (content-timeline resolved at t=0). A mobject that
                // is revealed / transformed / hidden so nothing shows at t=0 yet
                // WILL render later has `frame0` empty/`none` while `natural`
                // still carries the real content. Such mobjects must keep their
                // natural box in the flow — otherwise every later mobject shifts
                // up and the hidden mobject never gets a `nat` to be placed at —
                // so we emit them wrapped in `#hide[…]`, exactly the native-Typst
                // idiom for "keep the space, hide the ink", instead of dropping
                // them from the flow. Pure containers whose base body is empty /
                // `none` *and* which never render any content own no box and are
                // still skipped.
                let frame0 = content_for(&self.scene, label, 0);
                let frame0_t = frame0.trim();
                if (natural.is_empty() || natural == "none")
                    && (frame0_t.is_empty() || frame0_t == "none")
                {
                    continue;
                }
                // Distinct, safe 24-bit colour (skips black/white corners).
                let color = format!("#{:06x}", 0x010101u32.wrapping_add(i as u32) & 0xFFFFFF);
                palette.push((label.clone(), color.clone()));
                let natural_sub = substitute_counters(&self.scene, natural, 0);
                let body = if frame0_t.is_empty() || frame0_t == "none" {
                    // Temporarily not rendered: keep the box, hide the ink.
                    // Note: this is interpolated inside `#{{ … }}` (Typst *code*
                    // mode), so `hide` is called as a bare function — no `#`.
                    format!("hide[{natural_sub}]")
                } else {
                    // `content_for` already substituted counters at t=0, so the
                    // frame-0 body can be emitted verbatim.
                    frame0_t.to_string()
                };
                // A block-level, content-sized wrapper makes Typst place this
                // object on its own line in the normal flow (so it stacks
                // vertically with the others), while the unique fill colour lets
                // us recover its footprint from the single rendered SVG.
                blocks.push_str(&format!(
                    "\n#block(width: auto, fill: rgb(\"{color}\"))[#{{ ({body}) }}]"
                ));
            }
            if blocks.is_empty() {
                continue;
            }
            let src = format!(
                "{preamble}\n#set page(width: {pw}pt, height: {ph}pt, margin: 0pt, fill: none)\n\
                 {blocks}\n"
            );
            let doc = match self.compile(&src) {
                Ok(d) => d,
                Err(_) => continue,
            };
            // A scene is laid out in plain Typst document flow, so content that
            // overflows its single page spills onto *subsequent* pages (page 1,
            // page 2, …), each `ph` tall. We treat this as a **cross-page scene**:
            // the mobjects stay in ONE scene (data shared — same ownership, same
            // timeline), but they are laid out across the overflow pages and the
            // renderer plays the pages **in sequence** on a single-page canvas
            // (it does NOT grow the canvas). Each mobject keeps its position
            // *within* the page it landed on (page-local `ly`), and is only drawn
            // while that page is the active one — so the other pages' timelines
            // stay frozen until this page finishes and the renderer auto-advances.
            let pages = doc.pages();
            let num_pages = pages.len().max(1);
            scene_page_counts.insert(*sid, num_pages);
            for (k, page) in pages.iter().enumerate() {
                let svg = typst_svg::svg(page, &SvgOptions::default());
                for (label, color) in &palette {
                    // An object appears on exactly one page, so skip colours we
                    // have already placed on an earlier page.
                    if nat.contains_key(label) {
                        continue;
                    }
                    let Some(layout_bbox) = bbox_of_svg_with_fill(&svg, color) else {
                        continue;
                    };
                    // The natural position is exactly where plain Typst lays the
                    // object's content box: the coloured `block` we wrap it in
                    // shrinks to the body, so its fill footprint's top-left *is*
                    // the body's native content-box top-left (`lx, ly`). Each
                    // page's SVG resets its origin to that page's top-left, so we
                    // record the position *within* the page (page-local `ly`) and
                    // remember which page `k` the mobject landed on.
                    let (lx, ly, _, _) = layout_bbox;
                    nat.insert(label.clone(), (lx, ly));
                    page_of.insert(label.clone(), k);
                }
            }
        }
        self.nat = nat;
        // Build per-scene canvas sizes + label→scene ownership for auto-hide.
        // When `scenes` is empty (legacy single-scene document) we fall back to
        // the whole document as one scene (id 0) — behavior identical to v0.1.
        self.label_scene = self.scene.label_scene_map();
        let mut sp: HashMap<usize, (f64, f64)> = HashMap::new();
        if self.scene.scenes.is_empty() {
            // Legacy single-scene document: the whole document is one scene
            // (id 0). The canvas is a *single* page — overflowing content does
            // NOT grow the canvas; it becomes additional pages that play in
            // sequence (see `page_schedules`).
            sp.insert(0, (self.page_w, self.page_h));
        } else {
            for s in &self.scene.scenes {
                // Each scene's canvas is exactly one page (its `width`/`height`).
                // Overflow pages play in sequence on this single-page canvas; they
                // do NOT stack vertically (see [`pages`]).
                let (pw, ph) = self.scene.effective_page_pt(s.id);
                sp.insert(s.id, (pw, ph));
            }
        }
        self.scene_pages = sp;
        // Build the cross-page scene playback scheduler. This partitions each
        // scene's timeline into page-segments (see [`pages`]): each page has its
        // own independent timeline, and the renderer auto-advances from one page
        // to the next once the current page's content has finished playing.
        self.pages = PageScheduler::build(&self.scene, page_of, &scene_page_counts);
        // Precompute morph plans (the expensive part) exactly once. For each
        // `#morph(from, to)` pair we render both bodies to SVG, extract their
        // outline rings, normalize each ring to its own local origin, and build
        // a `MorphPlan`. Each frame then only *samples* the plan — this is the
        // performance-first design (no per-frame SVG ring extraction).
        for pair in &self.scene.morph_pairs {
            let fb = self.scene.items.get(&pair.from);
            // For `transform`, `to_body` overrides `items[to]` (which still holds
            // the *original* body until the content-timeline swap) so the morph
            // interpolates toward the new content.
            let tb = pair
                .to_body
                .as_ref()
                .or_else(|| self.scene.items.get(&pair.to));
            let (Some(fb), Some(tb)) = (fb, tb) else {
                continue;
            };
            let Some((fr, _, _)) = self.body_largest_shape(fb) else {
                continue;
            };
            let Some((tr, fill, stroke)) = self.body_largest_shape(tb) else {
                continue;
            };
            let fl = localize_ring(fr);
            let tl = localize_ring(tr);
            if fl.is_empty() || tl.is_empty() {
                continue;
            }
            let plan = MorphPlan::new(fl, tl, fill, stroke, MORPH_MAX_SEGMENT);
            self.morph_cache
                .insert((pair.from.clone(), pair.to.clone()), plan);
        }
        // Precompute per-glyph fragment layouts for inline `#transform` calls.
        // Each plan's old/new bodies are split into glyph fragments, laid out at
        // their absolute page positions, and matched via LCS. Only plans that
        // actually produced fragments are kept (empty ones fall back to the
        // crossfade, which is left intact).
        let plans = self.scene.transform_plans.clone();
        for plan in &plans {
            // Render the whole old/new formulas and extract each glyph /
            // decoration as a positioned fragment (Typst's own layout). On
            // failure we keep the legacy crossfade intact.
            if let Some(tf) = self.build_transform_fragments(plan) {
                self.transform_fragments.push(tf);
            }
        }
        self.natural_computed = true;
        Ok(())
    }
    /// The frame-0 visual state for a label (opacity 0 for `play` blocks).
    fn initial_for(&self, label: Label, time_ms: u32) -> FrameData {
        match self.scene.initial.get(&label) {
            Some(f) => FrameData {
                time_ms,
                target: label,
                x: f.x,
                y: f.y,
                scale: f.scale,
                opacity: f.opacity,
                rotation: f.rotation,
                easing: f.easing.clone(),
            },
            None => FrameData::new(time_ms, label),
        }
    }
    /// Render a single mobject at its placed position onto a transparent
    /// full-canvas RGBA frame (page-sized).
    fn render_object_pixels(
        &self,
        label: &Label,
        st: &FrameData,
        time_ms: u32,
        page_w: f64,
        page_h: f64,
        pixel_per_pt: f32,
    ) -> Result<crate::renderer::RenderedFrame, CandyError> {
        let nat = self.nat.get(label).cloned().unwrap_or((0.0, 0.0));
        let nat_cm = (nat.0 / PT_PER_CM, nat.1 / PT_PER_CM);
        let abs_x_cm = nat_cm.0 + st.x;
        let abs_y_cm = nat_cm.1 + st.y;
        let scale_pct = st.scale * 100.0;
        let body = self.resolve_body(label, time_ms);
        let preamble = imports_preamble(&self.scene);
        // Sprite cache: identical (label + body + quantized transform + ppi)
        // states reuse the previously rasterized RGBA, skipping Typst's
        // `render`. Paused / static objects are the common case and hit this
        // every frame; genuinely moving objects miss it (correctly re-raster).
        let body_hash = {
            let mut h = std::collections::hash_map::DefaultHasher::new();
            body.hash(&mut h);
            std::hash::Hasher::finish(&h)
        };
        let key = SpriteKey {
            label: label.0.clone(),
            body_hash,
            scale_q: (scale_pct * 10.0).round().max(0.0) as u32,
            rot_q: (st.rotation * 10.0).round() as u32,
            x_q: ((abs_x_cm + 1e6) * 100.0).round().max(0.0) as u32,
            y_q: ((abs_y_cm + 1e6) * 100.0).round().max(0.0) as u32,
            ppi_q: (pixel_per_pt * 100.0).round() as u32,
        };
        if let Some(cached) = self.sprite_cache.lock().unwrap().get(&key) {
            return Ok((**cached).clone());
        }
        let placed = place_source(
            page_w,
            page_h,
            abs_x_cm,
            abs_y_cm,
            scale_pct,
            st.rotation,
            &body,
            &preamble,
        );
        let doc = self.compile_cached(&placed)?;
        let page = doc
            .pages()
            .first()
            .ok_or_else(|| CandyError::Typst("document produced no pages".into()))?;
        let opts = RenderOptions {
            pixel_per_pt: Scalar::new(pixel_per_pt as f64),
            render_bleed: false,
        };
        let pix = render(page, &opts);
        let frame = crate::renderer::RenderedFrame {
            width: pix.width() as usize,
            height: pix.height() as usize,
            rgba: pix.data().to_vec(),
        };
        self.sprite_cache
            .lock()
            .unwrap()
            .insert(key, Arc::new(frame.clone()));
        Ok(frame)
    }
    /// Stable paint index for each label, following source *declaration* order:
    /// scenes in order, each scene's `owns_labels` in declaration order. Used so
    /// the composite z-order is deterministic and faithful to native Typst
    /// (later-declared mobjects paint on top) instead of an arbitrary `HashMap`
    /// iteration or an alphabetical sort that scrambles并列 mobjects.
    fn draw_order_index(&self) -> HashMap<Label, usize> {
        let mut idx = HashMap::new();
        let mut i = 0usize;
        for s in &self.scene.scenes {
            for l in &s.owns_labels {
                idx.entry(l.clone()).or_insert(i);
                i += 1;
            }
        }
        idx
    }
    /// Composite all mobjects (per-object opacity) onto an opaque-white canvas.
    pub fn render_frame_pixels(
        &mut self,
        time_ms: u32,
        all_frames: &[FrameData],
        pixel_per_pt: f32,
    ) -> Result<crate::renderer::RenderedFrame, CandyError> {
        self.ensure_natural()?;
        self.render_frame_pixels_par(time_ms, all_frames, pixel_per_pt)
    }
    /// Parallel-safe variant of [`render_frame_pixels`](Self::render_frame_pixels).
    ///
    /// Takes `&self` (not `&mut self`) so it can be called from a rayon
    /// parallel iterator. **Precondition:** `ensure_natural()` must have been
    /// called once before any parallel call (it initializes `nat`/`page_w`/
    /// `page_h`). The [`Renderer::ensure_natural_public`] method exposes this.
    pub fn render_frame_pixels_par(
        &self,
        time_ms: u32,
        all_frames: &[FrameData],
        pixel_per_pt: f32,
    ) -> Result<crate::renderer::RenderedFrame, CandyError> {
        // Resolve per-object effective transforms (group composition applied)
        // and extract the optional global camera state.
        let (states, camera) = self.prepare_states(all_frames, time_ms);
        // Resolve the active scene (innermost scene at this frame time) and its
        // canvas. Entering a child scene hides its parent — we render only the
        // active scene's mobjects. With no scene tree, the whole document is one
        // scene and everything renders (legacy behavior).
        let active = if self.scene.scenes.is_empty() {
            0
        } else {
            self.scene.active_scene_at(time_ms)
        };
        // Cross-page scene: the page currently playing. Only mobjects on this
        // page are drawn; the other pages' timelines stay frozen until this page
        // finishes and the renderer auto-advances to the next page.
        // Cross-page scene: the page currently playing. Only mobjects on this
        // page are drawn; the other pages' timelines stay frozen until this page
        // finishes and the renderer auto-advances to the next page.
        let active_page = self.pages.active_page_of(active, time_ms);
        let (pw, ph) = if self.scene.scenes.is_empty() {
            (self.page_w, self.page_h)
        } else {
            self.scene_pages
                .get(&active)
                .copied()
                .unwrap_or((self.page_w, self.page_h))
        };
        let mut labels: Vec<&Label> = states.keys().collect();
        let order = self.draw_order_index();
        labels.sort_by(|a, b| order.get(*a).cmp(&order.get(*b)).then(a.0.cmp(&b.0)));
        let mut objs: Vec<(f64, crate::renderer::RenderedFrame)> = Vec::new();
        for label in &labels {
            // Scene auto-hide: a mobject is visible ONLY when its owner scene IS
            // the active scene (`label_scene[label] == active`). This is what
            // makes scenes behave like independent slides — entering a child
            // scene hides its parent, and when the root scene is active only the
            // root's own mobjects are drawn (a child scene's content does NOT
            // leak onto the root canvas). Mobjects not attributed to any scene
            // (legacy / global) are kept visible.
            if !self.scene.scenes.is_empty() {
                let owner = self.label_scene.get(*label).copied().unwrap_or(active);
                if owner != active {
                    continue;
                }
            }
            // Cross-page gate: skip mobjects that belong to a different page.
            // Their timeline is frozen (not drawn) until the renderer advances
            // to their page.
            if let Some(p) = self.pages.page_of(label) {
                if p != active_page {
                    continue;
                }
            }
            // Per-glyph transform: the `target`/`old` mobjects are replaced by
            // the interpolated glyph fragments, so skip them during the window.
            if self.transform_hidden(*label, time_ms) {
                continue;
            }
            let st = states.get(*label).unwrap();
            let frame = self.render_object_pixels(*label, st, time_ms, pw, ph, pixel_per_pt)?;
            objs.push((st.opacity, frame));
        }
        // Subtitle overlays are collected separately: they must be composited
        // AFTER the global camera warp so they stay pinned at a fixed page
        // position/size regardless of the current view (pan/zoom/rotate).
        let mut subs: Vec<crate::renderer::RenderedFrame> = Vec::new();
        for sub in &self.scene.subtitles {
            if self
                .scene
                .visible_subtitle_ids_at(time_ms)
                .contains(&sub.id)
            {
                let frame = self.render_subtitle_pixels(sub, time_ms, pixel_per_pt)?;
                subs.push(frame);
            }
        }
        // Canvas size follows the active scene's page (not an arbitrary frame).
        let w = (pw * pixel_per_pt as f64).round().max(1.0) as usize;
        let h = (ph * pixel_per_pt as f64).round().max(1.0) as usize;
        // Seed the canvas with the active scene's background color (inheriting
        // from a parent scene, defaulting to opaque white), so the configured
        // `bg` actually shows in the encoded video — not a hardcoded white.
        let bg_rgba = if self.scene.scenes.is_empty() {
            [255u8, 255, 255, 255]
        } else {
            Self::hex_to_rgba(&self.scene_bg_hex(active))
        };
        let mut canvas = vec![0u8; w * h * 4];
        for chunk in canvas.chunks_mut(4) {
            chunk.copy_from_slice(&bg_rgba);
        }
        for (opacity, f) in &objs {
            composite_over(&mut canvas, f, *opacity, w, h);
        }
        // Per-glyph transform overlays (Manim-style), composited directly into
        // the canvas so they are warped by the camera together with the other
        // mobjects — and so we never allocate a full-page buffer per fragment
        // (which made rendering pathologically slow). Fragments are placed with
        // correct cm→pt→px scaling, so they stay glued to the target mobject.
        self.transform_fragment_frames(&states, time_ms, pixel_per_pt, pw, ph, &mut canvas, w, h)?;
        // Apply the global camera (pan + zoom + rotate) by warping the
        // composited object canvas through the inverse camera transform.
        // Subtitles are deliberately NOT warped here — they are overlaid
        // afterwards so they remain at a fixed page position and fixed size
        // no matter what the camera does.
        if let Some(cam) = &camera {
            warp_canvas_with_camera(&mut canvas, w, h, cam, pw, ph, pixel_per_pt, bg_rgba);
        }
        // Overlay subtitles on top of the warped canvas, at their fixed
        // page-anchored positions.
        for f in &subs {
            composite_over(&mut canvas, f, 1.0, w, h);
        }
        Ok(crate::renderer::RenderedFrame {
            width: w,
            height: h,
            rgba: canvas,
        })
    }
    /// Public wrapper around `ensure_natural` so callers (e.g. the parallel
    /// rasterization loop in `build_input_with_gpu`) can pre-compute the
    /// natural layout before spawning parallel frame renders.
    pub fn ensure_natural_public(&mut self) -> Result<(), CandyError> {
        self.ensure_natural()
    }
    /// Test-only accessor for the computed natural (first-frame) top-left of a
    /// mobject, in Typst points (page origin). Used by the native-consistency /
    /// declaration-order regression tests.
    #[cfg(test)]
    pub(crate) fn nat_for(&self, label: &Label) -> Option<(f64, f64)> {
        self.nat.get(label).copied()
    }
    /// Test-only: summary of the precomputed per-glyph transform plans
    /// `(target, fragment_count, start_ms, end_ms)`.
    #[cfg(test)]
    pub(crate) fn transform_plans_debug(&self) -> Vec<(String, usize, u32, u32)> {
        self.transform_fragments
            .iter()
            .map(|p| (p.target.0.clone(), p.anims.len(), p.start_ms, p.end_ms))
            .collect()
    }
    /// Test-only: total number of glyph fragments active at `time_ms`.
    #[cfg(test)]
    pub(crate) fn active_fragment_count(&self, time_ms: u32) -> usize {
        self.transform_fragments
            .iter()
            .filter(|p| time_ms >= p.start_ms && time_ms < p.end_ms)
            .map(|p| p.anims.len())
            .sum()
    }
    /// GPU-accelerated variant of [`render_frame_pixels`](Self::render_frame_pixels).
    ///
    /// Available only when the `gpu` cargo feature is enabled. Renders the
    /// frame to SVG (same as `render_frame_at`, with per-object opacity
    /// already applied via `<g opacity>` wrappers), then rasterizes the SVG on
    /// the GPU via vello + wgpu. The result is identical to the CPU path
    /// (modulo GPU rasterization differences like anti-aliasing quality), so
    /// the downstream video encoder consumes it unchanged.
    ///
    /// Pass a reusable [`crate::renderer::raster::gpu::GpuRenderer`] — constructing a
    /// wgpu device is expensive, so it should be created once and reused
    /// across every frame in the animation.
    #[cfg(feature = "gpu")]
    pub fn render_frame_pixels_gpu(
        &mut self,
        time_ms: u32,
        all_frames: &[FrameData],
        pixel_per_pt: f32,
        gpu: &mut crate::renderer::raster::gpu::GpuRenderer,
    ) -> Result<crate::renderer::RenderedFrame, CandyError> {
        // 1. Produce the composite SVG for this frame (with opacity baked in).
        let svg_bytes = self.render_frame_at(time_ms, all_frames)?;
        let svg_str = std::str::from_utf8(&svg_bytes)
            .map_err(|e| CandyError::Typst(format!("svg utf8: {e}")))?;
        // 2. Compute target pixel dimensions from the active scene's page size + ppi.
        let (pw, ph) = if self.scene.scenes.is_empty() {
            (self.page_w, self.page_h)
        } else {
            let active = self.scene.active_scene_at(time_ms);
            self.scene_pages
                .get(&active)
                .copied()
                .unwrap_or((self.page_w, self.page_h))
        };
        let width = (pw * pixel_per_pt as f64).round().max(1.0) as u32;
        let height = (ph * pixel_per_pt as f64).round().max(1.0) as u32;
        // 3. Rasterize on the GPU.
        gpu.render_svg(svg_str, width, height)
    }
    /// Render the full scene at a frame index to an SVG string (draft / fallback).
    ///
    /// Unlike the older implementation, this applies per-object `opacity` by
    /// rendering each mobject as its own SVG and composing them via nested
    /// `<svg opacity="...">` elements. This closes the gap with the video path
    /// (which always applied opacity via `composite_over`) — the SVG draft and
    /// the encoded video now agree visually.
    pub fn render_frame_at(
        &mut self,
        time_ms: u32,
        all_frames: &[FrameData],
    ) -> Result<Vec<u8>, CandyError> {
        self.ensure_natural()?;
        // Resolve per-object effective transforms (group composition applied)
        // and extract the optional global camera state.
        let (states, camera) = self.prepare_states(all_frames, time_ms);
        let mut labels: Vec<&Label> = states.keys().collect();
        let order = self.draw_order_index();
        labels.sort_by(|a, b| order.get(*a).cmp(&order.get(*b)).then(a.0.cmp(&b.0)));
        // Deterministic z-order (same as the video path), following source
        // declaration order so并列 mobjects paint in the order they were written.
        // Resolve the active scene + its canvas. Only the active scene's
        // mobjects are rendered; a parent scene is auto-hidden while a child
        // scene is active.
        let active = if self.scene.scenes.is_empty() {
            0
        } else {
            self.scene.active_scene_at(time_ms)
        };
        // Cross-page scene: the page currently playing. Only mobjects on this
        // page are drawn; the other pages' timelines stay frozen until this page
        // finishes and the renderer auto-advances to the next page.
        let active_page = self.pages.active_page_of(active, time_ms);
        let (pw, ph) = if self.scene.scenes.is_empty() {
            (self.page_w, self.page_h)
        } else {
            self.scene_pages
                .get(&active)
                .copied()
                .unwrap_or((self.page_w, self.page_h))
        };
        // Background, page-sized canvas. The fill honors the active scene's
        // configured `bg` (e.g. rgb("#05060f")), inheriting from a parent
        // scene and defaulting to opaque white when none is set.
        let bg_hex = if self.scene.scenes.is_empty() {
            "white".to_string()
        } else {
            self.scene_bg_hex(active)
        };
        let mut out = String::new();
        out.push_str(&format!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{pw}\" height=\"{ph}\" viewBox=\"0 0 {pw} {ph}\" xmlns:xlink=\"http://www.w3.org/1999/xlink\">\n",
            pw = pw, ph = ph
        ));
        out.push_str(&format!(
            "<rect x=\"0\" y=\"0\" width=\"{pw}\" height=\"{ph}\" fill=\"{bg_hex}\"/>\n",
            pw = pw,
            ph = ph,
            bg_hex = bg_hex
        ));
        // A present camera wraps only the mobjects in a single global transform
        // group. The background and subtitle overlays stay fixed (drawn outside
        // this group) so they are unaffected by the camera; only the mobjects
        // move/scale/rotate under the view.
        if let Some(cam) = &camera {
            out.push_str(&format!(
                "<g transform=\"{}\">\n",
                camera_transform_svg(cam, pw, ph)
            ));
        }
        for label in labels {
            // Scene auto-hide: a mobject is visible ONLY when its owner scene IS
            // the active scene (`label_scene[label] == active`). This is what
            // makes scenes behave like independent slides — entering a child
            // scene hides its parent, and when the root scene is active only the
            // root's own mobjects are drawn (a child scene's content does NOT
            // leak onto the root canvas). Mobjects not attributed to any scene
            // (legacy / global) are kept visible.
            if !self.scene.scenes.is_empty() {
                let owner = self.label_scene.get(label).copied().unwrap_or(active);
                if owner != active {
                    continue;
                }
            }
            // Cross-page gate: skip mobjects that belong to a different page.
            // Their timeline is frozen (not drawn) until the renderer advances
            // to their page.
            if let Some(p) = self.pages.page_of(label) {
                if p != active_page {
                    continue;
                }
            }
            // Per-glyph transform: the `target`/`old` mobjects are replaced by
            // the interpolated glyph fragments, so skip them during the window.
            if self.transform_hidden(label, time_ms) {
                continue;
            }
            let st = &states[label];
            let obj_svg = self.render_object_svg(label, st, time_ms, pw, ph)?;
            // Wrap each object's SVG in a group with the per-frame opacity.
            // SVG <g opacity> applies to all descendants (shapes + text).
            let op = st.opacity.clamp(0.0, 1.0);
            out.push_str(&format!("<g opacity=\"{op}\">\n{obj_svg}\n</g>\n"));
        }
        // Per-glyph transform overlays (Manim-style), drawn INSIDE the camera
        // group so they move with the view like the other mobjects.
        //
        // For each active plan we embed the whole old/new formula exactly ONCE
        // (its inner markup, with symbol ids localized under a per-plan prefix
        // and wrapped in a single `<g id=…>` inside `<defs>`), then draw each
        // fragment as a clipped `<use>` of that group. This keeps the SVG small
        // (the formula is rasterized once, not once per fragment) and avoids
        // glyph-id collisions between the old and new formulas. The clip + the
        // translate follow the target mobject (nat + state) so the transform
        // stays aligned with the rest of the scene. Embedding the formula once
        // (instead of repeating the full markup inside every fragment's clip)
        // is also what prevents neighbouring glyphs from leaking through a
        // slightly-off clip box — the "residual garbage" artefact.
        out.push_str(&self.transform_overlay_svg(&states, time_ms));
        // Close the camera group BEFORE drawing subtitles so the captions are
        // not transformed by the camera — they stay pinned at a fixed page
        // position and fixed size regardless of the current view (pan/zoom/
        // rotate). The white background and the subtitle overlays therefore
        // remain static while only the mobjects move under the camera.
        if camera.is_some() {
            out.push_str("</g>\n");
        }
        // Subtitle overlays: one per visible scope, subject to
        // parental shadowing + auto-destroy. Drawn on top of the objects,
        // OUTSIDE any camera transform, at their fixed page anchors.
        for sub in &self.scene.subtitles {
            if self
                .scene
                .visible_subtitle_ids_at(time_ms)
                .contains(&sub.id)
            {
                let svg = self.render_subtitle_svg(sub, time_ms)?;
                out.push_str(&svg);
                out.push('\n');
            }
        }
        out.push_str("</svg>\n");
        Ok(out.into_bytes())
    }
    /// Render a single mobject at its placed position as an SVG string.
    /// Uses the same placement math as `render_object_pixels`.
    fn render_object_svg(
        &self,
        label: &Label,
        st: &FrameData,
        time_ms: u32,
        page_w: f64,
        page_h: f64,
    ) -> Result<String, CandyError> {
        let nat = self.nat.get(label).cloned().unwrap_or((0.0, 0.0));
        let nat_cm = (nat.0 / PT_PER_CM, nat.1 / PT_PER_CM);
        let abs_x_cm = nat_cm.0 + st.x;
        let abs_y_cm = nat_cm.1 + st.y;
        let scale_pct = st.scale * 100.0;
        let body = self.resolve_body(label, time_ms);
        let preamble = imports_preamble(&self.scene);
        let src = place_source(
            page_w,
            page_h,
            abs_x_cm,
            abs_y_cm,
            scale_pct,
            st.rotation,
            &body,
            &preamble,
        );
        let doc = self.compile_cached(&src)?;
        let page = doc
            .pages()
            .first()
            .ok_or_else(|| CandyError::Typst("document produced no pages".into()))?;
        Ok(typst_svg::svg(page, &SvgOptions::default()))
    }
    /// Render a single target's frame as an isolated SVG (spec §4.4 style).
    pub fn render_frame(&mut self, frame: &FrameData) -> Result<Vec<u8>, CandyError> {
        if !self.scene.items.contains_key(&frame.target)
            && !self.scene.content_timeline.contains_key(&frame.target)
        {
            return Err(CandyError::LabelNotFound(frame.target.clone()));
        }
        self.ensure_natural()?;
        let doc = self.compile_cached(&self.object_source(frame, frame.time_ms))?;
        let page = doc
            .pages()
            .first()
            .ok_or_else(|| CandyError::Typst("document produced no pages".into()))?;
        let svg = typst_svg::svg(page, &SvgOptions::default());
        Ok(svg.into_bytes())
    }
    /// Build the isolated per-object source for a single target.
    fn object_source(&self, st: &FrameData, time_ms: u32) -> String {
        let nat = self.nat.get(&st.target).cloned().unwrap_or((0.0, 0.0));
        let nat_cm = (nat.0 / PT_PER_CM, nat.1 / PT_PER_CM);
        let abs_x_cm = nat_cm.0 + st.x;
        let abs_y_cm = nat_cm.1 + st.y;
        let scale_pct = st.scale * 100.0;
        let body = self.resolve_body(&st.target, time_ms);
        let preamble = imports_preamble(&self.scene);
        place_source(
            self.page_w,
            self.page_h,
            abs_x_cm,
            abs_y_cm,
            scale_pct,
            st.rotation,
            &body,
            &preamble,
        )
    }
    /// Render a subtitle to an SVG string using the scene's page size.
    fn render_subtitle_svg(&self, sub: &Subtitle, time_ms: u32) -> Result<String, CandyError> {
        render_subtitle_svg_impl(&self.scene, sub, self.page_w, self.page_h, time_ms)
    }
    /// Render a subtitle to an RGBA frame (page-sized) for the pixel path.
    fn render_subtitle_pixels(
        &self,
        sub: &Subtitle,
        time_ms: u32,
        pixel_per_pt: f32,
    ) -> Result<crate::renderer::RenderedFrame, CandyError> {
        let doc = subtitle_doc(&self.scene, sub, self.page_w, self.page_h, time_ms)?;
        let page = doc
            .pages()
            .first()
            .ok_or_else(|| CandyError::Typst("document produced no pages".into()))?;
        let opts = RenderOptions {
            pixel_per_pt: Scalar::new(pixel_per_pt as f64),
            render_bleed: false,
        };
        let pix = render(page, &opts);
        Ok(crate::renderer::RenderedFrame {
            width: pix.width() as usize,
            height: pix.height() as usize,
            rgba: pix.data().to_vec(),
        })
    }
    /// Render a mobject body in isolation and return its largest outline shape
    /// (by absolute area) as a ring of points plus its paint. Returns `None` if
    /// the body produces no extractable outline (e.g. an image or a body whose
    /// shape candy can't morph — those fall back to the plain crossfade).
    fn body_largest_shape(
        &self,
        body: &str,
    ) -> Option<(Vec<[f64; 2]>, Option<String>, Option<String>)> {
        let preamble = imports_preamble(&self.scene);
        let pre = if preamble.is_empty() {
            String::new()
        } else {
            format!("{preamble}\n")
        };
        let src = format!(
            "{pre}#set page(width: {w}pt, height: {h}pt, margin: 0pt, fill: none)\n#{{ ({body}) }}\n",
            w = self.page_w,
            h = self.page_h,
        );
        let doc = self.compile(&src).ok()?;
        let page = doc.pages().first()?;
        let svg = typst_svg::svg(page, &SvgOptions::default());
        let shapes = extract_shapes_from_svg(&svg);
        shapes
            .into_iter()
            .max_by(|a, b| {
                polygon_area(&a.ring)
                    .abs()
                    .partial_cmp(&polygon_area(&b.ring).abs())
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .map(|s| (s.ring, s.fill, s.stroke))
    }
    /// If `label` is the `to` target of an active `#morph` pair at `time_ms`,
    /// return the morphed shape as a Typst `polygon(...)` body (without a
    /// leading `#` — the caller's `place_source` prepends it). Outside the pair
    /// window `None` is returned so the object renders its normal body (this
    /// also makes the hand-off at `end_ms` seamless: at `t = end_ms` the morphed
    /// polygon equals the `to` body's own outline).
    fn morph_body_for(&self, label: &Label, time_ms: u32) -> Option<String> {
        for pair in &self.scene.morph_pairs {
            if &pair.to != label {
                continue;
            }
            if time_ms < pair.start_ms || time_ms > pair.end_ms {
                return None;
            }
            let key = (pair.from.clone(), pair.to.clone());
            let plan = self.morph_cache.get(&key)?;
            let denom = (pair.end_ms - pair.start_ms).max(1) as f64;
            let p = (((time_ms - pair.start_ms) as f64) / denom).clamp(0.0, 1.0);
            let ring = plan.at(p);
            if ring.is_empty() {
                return None;
            }
            return Some(polygon_svg(&ring, &plan.fill, &plan.stroke));
        }
        None
    }
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
    let world = CandyWorld::new(&state, source);
    let warned = typst::compile::<PagedDocument>(&world);
    match warned.output {
        Ok(doc) => {
            let page = doc
                .pages()
                .first()
                .ok_or_else(|| CandyError::Typst("no pages".into()))?;
            Ok(typst_svg::svg(page, &SvgOptions::default()))
        }
        Err(e) => Err(CandyError::Typst(format!("{:?}", e))),
    }
}
#[test]
fn path_parser_handles_relative_hv_and_implicit_repeat() {
    // Typst emits the coloured layout-marker rects as relative `v`/`h` paths.
    // The old naive parser zipped every number into `(x, y)` pairs and
    // transposed width/height (44.18×13.16 reported as 13.16×44.18).
    let pts = collect_path_points("M 0 0v 13.16h 44.18v -13.16Z");
    let xs: Vec<f64> = pts.iter().map(|p| p.0).collect();
    let ys: Vec<f64> = pts.iter().map(|p| p.1).collect();
    let min_x = xs.iter().cloned().fold(f64::INFINITY, f64::min);
    let max_x = xs.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    let min_y = ys.iter().cloned().fold(f64::INFINITY, f64::min);
    let max_y = ys.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    assert_eq!(min_x, 0.0);
    assert_eq!(min_y, 0.0);
    assert!(
        (max_x - 44.18).abs() < 1e-6,
        "expected width 44.18, got {max_x}"
    );
    assert!(
        (max_y - 13.16).abs() < 1e-6,
        "expected height 13.16, got {max_y}"
    );
}
#[test]
fn path_parser_includes_bezier_control_points() {
    // Control points must be part of the returned hull, otherwise the bbox of
    // a curve would be under-reported (a Bézier lives inside its control hull).
    let pts = collect_path_points("M 0 0 C 10 20 30 -10 40 0 L 40 10");
    let ys: Vec<f64> = pts.iter().map(|p| p.1).collect();
    let max_y = ys.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    let min_y = ys.iter().cloned().fold(f64::INFINITY, f64::min);
    assert!(
        (max_y - 20.0).abs() < 1e-6,
        "control point y=20 must bound bbox, got {max_y}"
    );
    assert!(
        (min_y - (-10.0)).abs() < 1e-6,
        "control point y=-10 must bound bbox, got {min_y}"
    );
}
/// Verify the content timeline actually swaps an mobject's rendered body
/// between frames (this is what makes `transform` show the OLD content before
/// the switch and the NEW content after, without corrupting earlier frames).
#[test]
fn content_timeline_swaps_rendered_body() {
    use crate::core::ast::{Label, Scene, Slide};
    use crate::core::meta::PrivateMeta;
    use std::collections::HashMap;
    let mut items = HashMap::new();
    items.insert(Label("box".into()), "rect(width: 2cm, height: 2cm)".into());
    let mut timeline = HashMap::new();
    timeline.insert(
        Label("box".into()),
        vec![(50u32, "circle(radius: 1cm)".to_string())],
    );
    let scene = Scene {
        slides: vec![Slide {
            duration_ms: 100,
            actions: vec![],
        }],
        items,
        content_timeline: timeline,
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
        morph_pairs: Vec::new(),
        transform_plans: Vec::new(),
        groups: HashMap::new(),
        private_metadata: PrivateMeta::default(),
    };
    let mut r = Renderer::with_root(scene, PathBuf::new()).unwrap();
    // Before the switch (t=0): should render the original `rect`.
    let before = r.render_frame_at(0, &[]).unwrap();
    // After the switch (t=100): should render the new `circle`.
    let after = r.render_frame_at(100, &[]).unwrap();
    assert_ne!(
        before, after,
        "content timeline did not change rendered body"
    );
}
#[test]
fn substitute_counters_expands_ecval_as_ast_node() {
    use crate::core::ast::{CounterDef, Slide};
    use crate::core::easing::Easing;
    use crate::core::meta::PrivateMeta;
    use std::collections::HashMap;
    let mut counters = Vec::new();
    counters.push(CounterDef {
        name: "r".into(),
        scope: "0".into(),
        seed: 10,
        step: 1,
        duration_ms: None,
        easing: Easing::Linear,
        start_ms: 0,
    });
    let scene = Scene {
        slides: vec![Slide {
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
        private_metadata: PrivateMeta::default(),
    };
    // The canonical `ecval("name")` form: a real AST call expanded to an integer.
    assert_eq!(
        substitute_counters(&scene, "circle(radius: ecval(\"r\") * 1pt + 1cm)", 0),
        "circle(radius: 10 * 1pt + 1cm)"
    );
    // A long-lived counter steps once per ms: at t=5 → seed + step·5 = 15.
    assert_eq!(substitute_counters(&scene, "ecval(\"r\")", 5), "15");
    // The integer substitution yields valid Typst inside markup too.
    assert_eq!(
        substitute_counters(&scene, "text([Count: #ecval(\"r\")])", 5),
        "text([Count: #15])"
    );
    // An undeclared counter is left untouched (matches prior registry behaviour).
    assert_eq!(
        substitute_counters(&scene, "ecval(\"missing\")", 0),
        "ecval(\"missing\")"
    );
    // The bare-ident form stays accepted for backwards compatibility.
    assert_eq!(substitute_counters(&scene, "ecval(r)", 0), "10");
}
#[test]
fn subtitle_stays_in_viewport() {
    use crate::core::ast::{Scene, Slide, SubPos, Subtitle};
    use crate::core::easing::Easing;
    use crate::core::meta::PrivateMeta;
    use std::collections::HashMap;
    let page_w = 16.0 * PT_PER_CM;
    let page_h = 9.0 * PT_PER_CM;
    let mut subtitles = Vec::new();
    subtitles.push(Subtitle {
        id: "__sub_bottom".into(),
        scope: "0".into(),
        body: "[Bottom caption]".into(),
        start_ms: 0,
        end_ms: None,
        position: SubPos::Bottom,
        easing: Easing::Linear,
    });
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
        private_metadata: PrivateMeta::default(),
    };
    let mut r = Renderer::with_root(scene, PathBuf::new()).unwrap();
    let svg = r.render_frame_at(50, &[]).unwrap();
    let s = String::from_utf8(svg).unwrap();
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
        private_metadata: PrivateMeta::default(),
    };
    let mut r = Renderer::with_root(scene, PathBuf::new()).unwrap();
    r.ensure_natural_public().unwrap();
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
        body0.starts_with("polygon("),
        "morph body must be a polygon"
    );
    // Mid-window: still a polygon (interpolated shape).
    assert!(
        r.morph_body_for(&Label("b".into()), 50)
            .unwrap()
            .starts_with("polygon(")
    );
    // At the end of the window: polygon shaped like the *target* (square) — and
    // visually identical to rendering `b` normally (seamless hand-off).
    let body_end = r
        .morph_body_for(&Label("b".into()), 100)
        .expect("expected a morphed polygon at t=end");
    assert!(body_end.starts_with("polygon("));
    // The plan was actually precomputed (not empty).
    assert!(!r.morph_cache.is_empty(), "morph plan should be cached");
}
/// Regression test for two coupled rendering bugs:
///   1. Positioning must match native Typst — `nat` is the body's content-box
///      top-left (the coloured-block top-left), NOT shifted by the body's ink
///      offset. A text body has a nonzero ink offset, so this catches the
///      `nat = lx - ox` regression.
///   2. Multiple并列 mobjects must keep their *declaration* order top-to-bottom.
///      The labels below are deliberately declared as `zeta, alpha, mid` (not
///      alphabetical) so a stray alphabetical sort would be detected.
/// Independent ground-truth reference shared by the layout regression tests:
/// lay the bodies out in plain document flow (each wrapped in a uniquely
/// coloured block) and read back each block's top-left. This deliberately does
/// NOT call `ensure_natural`, so a regression in the production layout (ink
/// offset shift, scrambled order, or dropped hidden mobjects) is caught rather
/// than silently mirrored.
#[cfg(test)]
fn native_natural_positions(
    r: &Renderer,
    ordered: &[(String, String)],
    page_w: f64,
    page_h: f64,
) -> HashMap<Label, (f64, f64)> {
    let mut palette: Vec<(Label, String)> = Vec::new();
    let mut blocks = String::new();
    for (i, (label, body)) in ordered.iter().enumerate() {
        let color = format!("#{:06x}", 0x010101u32.wrapping_add(i as u32) & 0xFFFFFF);
        palette.push((Label(label.clone()), color.clone()));
        blocks.push_str(&format!(
            "\n#block(width: auto, fill: rgb(\"{color}\"))[#{{ {body} }}]"
        ));
    }
    let src = format!(
        "#set page(width: {page_w}pt, height: {page_h}pt, margin: 0pt, fill: none)\n{blocks}\n"
    );
    let doc = r.compile(&src).expect("native layout compile");
    let page = doc.pages().first().expect("native layout page");
    let svg = typst_svg::svg(page, &SvgOptions::default());
    let mut out = HashMap::new();
    for (label, color) in &palette {
        let (lx, ly, _, _) = bbox_of_svg_with_fill(&svg, color).expect("native bbox");
        out.insert(label.clone(), (lx, ly));
    }
    out
}
#[test]
fn renderer_natural_layout_matches_native_and_declaration_order() {
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
            owns_labels: owns.clone(),
        }],
        root_scene: Some(0),
        morph_pairs: Vec::new(),
        transform_plans: Vec::new(),
        groups: HashMap::new(),
        private_metadata: PrivateMeta::default(),
    };
    let mut r = Renderer::with_root(scene, PathBuf::new()).unwrap();
    r.ensure_natural_public().unwrap();
    let page_w = 16.0 * PT_PER_CM;
    let page_h = 9.0 * PT_PER_CM;
    // Independent ground-truth reference: lay the bodies out in plain document
    // flow (each wrapped in a uniquely coloured block) and read back each
    // block's top-left. This deliberately does NOT call `ensure_natural`, so a
    // regression in the production layout (ink-offset shift, scrambled order) is
    // caught rather than silently mirrored.
    let native = native_natural_positions(&r, &ordered, page_w, page_h);
    // (1) Each mobject's natural top-left must equal native Typst's content-box
    //     top-left (within 1pt — far smaller than a text ink offset of ~16pt).
    for (l, _) in &ordered {
        let label = Label(l.clone());
        let candy = r.nat_for(&label).expect("candy nat present");
        let nat = native.get(&label).expect("native nat present");
        assert!(
            (candy.0 - nat.0).abs() < 1.0 && (candy.1 - nat.1).abs() < 1.0,
            "label {l}: candy nat {candy:?} != native {nat:?}"
        );
    }
    // (2) Declaration order must be preserved top-to-bottom. With labels
    //     `zeta, alpha, mid`, an alphabetical sort would put `alpha` on top;
    //     assert `zeta` is highest and the order follows the source.
    let y = |l: &str| r.nat_for(&Label(l.into())).unwrap().1;
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
/// their natural space instead of being skipped". A mobject hidden at frame 0
/// (its content-timeline resolves to `none` at t=0, e.g. a `reveal`/`typewriter`
/// before its start) must STILL reserve its natural box in the flow — otherwise
/// every later mobject shifts up and the hidden mobject never gets a `nat` to be
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
            owns_labels: owns.clone(),
        }],
        root_scene: Some(0),
        morph_pairs: Vec::new(),
        transform_plans: Vec::new(),
        groups: HashMap::new(),
        private_metadata: PrivateMeta::default(),
    };
    let mut r = Renderer::with_root(scene, PathBuf::new()).unwrap();
    r.ensure_natural_public().unwrap();
    let page_w = 16.0 * PT_PER_CM;
    let page_h = 9.0 * PT_PER_CM;
    let native = native_natural_positions(&r, &ordered, page_w, page_h);
    // (1) The hidden mobject MUST have a natural position (it was not skipped).
    let hidden = r
        .nat_for(&Label("hidden".into()))
        .expect("hidden mobject must get a nat");
    // (2) Its natural position must match where it would sit if shown (native
    //     Typst, all-visible) — i.e. `#hide` reserved the same box.
    let nat_hidden = native
        .get(&Label("hidden".into()))
        .expect("native nat present");
    assert!(
        (hidden.0 - nat_hidden.0).abs() < 1.0 && (hidden.1 - nat_hidden.1).abs() < 1.0,
        "hidden: candy nat {hidden:?} != native {nat_hidden:?} (space not reserved)"
    );
    // (3) It must keep its slot in the flow: below `top`, above `bottom`.
    let y = |l: &str| r.nat_for(&Label(l.into())).unwrap().1;
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
    let scene = crate::parser::ast_walk::parse_tyx(&tmp).unwrap();
    let mut r = Renderer::with_root(scene, PathBuf::new()).unwrap();
    r.ensure_natural_public().unwrap();
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
    let scene = crate::parser::ast_walk::parse_tyx(&tmp).unwrap();
    let frames = crate::core::scheduler::schedule(&scene).unwrap();
    let mut r = Renderer::with_root(scene, PathBuf::new()).unwrap();
    r.ensure_natural_public().unwrap();
    // Mid-window: fragments present.
    let mid = 30u32;
    let svg_mid = String::from_utf8(r.render_frame_at(mid, &frames).unwrap()).unwrap();
    assert!(svg_mid.contains("<svg"), "mid-window svg empty");
    // After window (past the transform's end_ms, still inside the document): the
    // target shows its new content — must contain a nested `<svg` for the
    // rendered glyphs, not just the background rect. Scenes are mutually
    // exclusive slides, so this also verifies the scene stays on stage (its
    // interval is extended to the document end) and the target is not hidden.
    let after = 90u32;
    let svg_after = String::from_utf8(r.render_frame_at(after, &frames).unwrap()).unwrap();
    let nested = svg_after.matches("<svg").count();
    assert!(
        nested >= 2,
        "after window target must render (nested svg count={nested})"
    );
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
    let scene = crate::parser::ast_walk::parse_tyx(&tmp).unwrap();
    let frames = crate::core::scheduler::schedule(&scene).unwrap();
    let mut r = Renderer::with_root(scene, PathBuf::new()).unwrap();
    r.ensure_natural_public().unwrap();
    // Mid window: the transform is active, so fragments must be drawn.
    let mid = 30u32;
    let svg = String::from_utf8(r.render_frame_at(mid, &frames).unwrap()).unwrap();
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
               #animate(\"eq\", scale: 2.0, rotation: 30deg, duration: 60)\n\
               #pause(duration: 60)\n\
               ]\n";
    let tmp = std::env::temp_dir().join("candy_test_xf_compose.tyx");
    std::fs::write(&tmp, src).unwrap();
    let scene = crate::parser::ast_walk::parse_tyx(&tmp).unwrap();
    let frames = crate::core::scheduler::schedule(&scene).unwrap();
    let mut r = Renderer::with_root(scene, PathBuf::new()).unwrap();
    r.ensure_natural_public().unwrap();
    // Mid window: the transform is active AND the concurrent scale/rotate is
    // part-way through, so every fragment group must carry both transforms
    // (the transform inherits the target's live scale/rotation, not just x/y).
    let mid = 30u32;
    let svg = String::from_utf8(r.render_frame_at(mid, &frames).unwrap()).unwrap();
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

/// Regression: a `#transform` must compose with a concurrent `#animate(x: …)`
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
        let scene = crate::parser::ast_walk::parse_tyx(&tmp).unwrap();
        let frames = crate::core::scheduler::schedule(&scene).unwrap();
        let mut r = Renderer::with_root(scene, PathBuf::new()).unwrap();
        r.ensure_natural_public().unwrap();
        let svg = String::from_utf8(r.render_frame_at(mid, &frames).unwrap()).unwrap();
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
    // transform, but preceeded by `#animate(x: 5cm)` so the formula is
    // already shifted when the transform window runs. Because candy runs
    // `#animate` and `#transform` as *sequential* slides, the animate must
    // come BEFORE the transform for the translation to be live during the
    // transform window (at the transform's mid, the animate has already
    // finished and its x=5cm is inherited as the transform's base offset).
    let base = "#import \"candy\": *\n\
               #scene(width: 16cm, height: 9cm)[\n\
               #mobject(\"eq\", [$a + b = c$])\n\
               #transform(\"eq\", to: [$a + b + d = c$], duration: 60)\n\
               #pause(duration: 60)\n\
               ]\n";
    let moved = "#import \"candy\": *\n\
               #scene(width: 16cm, height: 9cm)[\n\
               #mobject(\"eq\", [$a + b = c$])\n\
               #animate(\"eq\", x: 5cm, duration: 60)\n\
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
    let scene = crate::parser::ast_walk::parse_tyx(&tmp).unwrap();
    let frames = crate::core::scheduler::schedule(&scene).unwrap();
    let mut r = Renderer::with_root(scene, PathBuf::new()).unwrap();
    r.ensure_natural_public().unwrap();
    let svg = String::from_utf8(r.render_frame_at(0, &frames).unwrap()).unwrap();
    // Single-page height in pt: 2cm * PT_PER_CM.
    let page_h_pt = 2.0 * crate::renderer::typst::PT_PER_CM;
    // Parse the root `<svg height="…">`.
    let h_attr = svg
        .lines()
        .find(|l| l.contains("<svg"))
        .and_then(|l| {
            let s = l.find("height=\"").unwrap();
            let start = s + "height=\"".len();
            let end = l[start..].find('"').unwrap();
            l[start..start + end].parse::<f64>().ok()
        })
        .expect("svg height attribute");
    // The canvas must stay exactly ONE page tall — not stacked, not grown.
    assert!(
        (h_attr - page_h_pt).abs() < 1.0,
        "cross-page scene canvas must stay a single page (height {h_attr} ≈ {page_h_pt}), not stacked"
    );
    // And the first frame must draw only the current page's mobjects (fewer than
    // all six), proving sequential page playback rather than one giant canvas.
    let drawn = svg.matches("<g opacity=").count();
    assert!(
        drawn > 0 && drawn < 6,
        "first frame should show only the current page's mobjects (drew {drawn} of 6)"
    );
    std::fs::remove_file(&tmp).ok();
}

