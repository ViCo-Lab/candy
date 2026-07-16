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
pub(crate) mod lru;
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
pub(crate) use self::lru::LruCache;
pub(crate) use self::morph::*;
pub(crate) use self::pages::*;
pub(crate) use self::svg::*;
pub(crate) use self::transform::*;
pub(crate) use self::world::*;
use crate::core::ast::{FrameData, Label, Scene, Subtitle};
#[cfg(test)]
use crate::core::ast::ParseArtifacts;
use crate::core::diag::{CandyError, CandyWarn};
use crate::core::morph::{MorphPlan, extract_shapes_from_svg, polygon_area};
use crate::parser::expr::strip_string_literal;
use crate::warn;
use std::collections::HashMap;
use std::hash::Hash;
use std::panic::{AssertUnwindSafe, catch_unwind};
#[cfg(test)]
use std::path::Path;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use typst_layout::PagedDocument;
use typst_library::foundations::{Dict, Smart, Value};
use typst_library::visualize::Paint;
use typst_render::{RenderOptions, render};
use typst_svg::SvgOptions;
use typst_syntax::ast::{self, Expr};
use typst_syntax::{LinkedNode, parse_code};
#[cfg(test)]
use typst_syntax::FileId;
#[cfg(test)]
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

/// A rasterized object sprite plus the pixel offset of its top-left within the
/// full canvas page.
///
/// Each mobject is rasterized on a *full-page* document (so its position is
/// correct), but the rendered frame is cropped to the object's tight bounding
/// box before caching. Storing the crop instead of the full page is what keeps
/// the `sprite_cache` bounded in memory: a small mobject becomes a KB-sized
/// sprite rather than an MB-sized full-canvas RGBA. `ox`/`oy` (page pixels)
/// let `render_frame_pixels_par` paste the crop back at the right place so the
/// composited result is bit-identical to compositing the full page.
struct CachedSprite {
    frame: crate::renderer::RenderedFrame,
    ox: i64,
    oy: i64,
}

/// Crop `rgba` (a `w`×`h` RGBA buffer) to the tight bounding box of its
/// non-transparent pixels. Returns the cropped buffer and the top-left offset
/// (in pixels) of that box within the original buffer. A fully transparent
/// buffer yields a 1×1 transparent sprite at offset (0,0).
fn crop_to_content(rgba: &[u8], w: usize, h: usize) -> CachedSprite {
    let mut min_x = w as i64;
    let mut min_y = h as i64;
    let mut max_x = -1i64;
    let mut max_y = -1i64;
    for y in 0..h as i64 {
        let row = (y * w as i64) * 4;
        for x in 0..w as i64 {
            if rgba[(row + x * 4 + 3) as usize] > 0 {
                if x < min_x {
                    min_x = x;
                }
                if x > max_x {
                    max_x = x;
                }
                if y < min_y {
                    min_y = y;
                }
                if y > max_y {
                    max_y = y;
                }
            }
        }
    }
    if max_x < min_x {
        return CachedSprite {
            frame: crate::renderer::RenderedFrame {
                width: 1,
                height: 1,
                rgba: vec![0u8; 4],
            },
            ox: 0,
            oy: 0,
        };
    }
    let cw = (max_x - min_x + 1) as usize;
    let ch = (max_y - min_y + 1) as usize;
    let mut out = vec![0u8; cw * ch * 4];
    for y in 0..ch as i64 {
        for x in 0..cw as i64 {
            let si = (((min_y + y) * w as i64 + (min_x + x)) * 4) as usize;
            let di = ((y * cw as i64 + x) * 4) as usize;
            out[di..di + 4].copy_from_slice(&rgba[si..si + 4]);
        }
    }
    CachedSprite {
        frame: crate::renderer::RenderedFrame {
            width: cw,
            height: ch,
            rgba: out,
        },
        ox: min_x,
        oy: min_y,
    }
}

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
/// Capacity of the per-object rasterized-sprite cache (`sprite_cache`).
///
/// After cropping (see [`render_object_pixels`] / [`crop_to_content`]) each
/// cached entry is only the object's tight bounding box (KB for a small
/// mobject), so this cap can stay high: it bounds the *count* of cached
/// sprites, not their pixel area. 512 cropped sprites is a few MB at most,
/// versus 512 full-canvas frames which would be gigabytes at HD and OOM.
const SPRITE_CACHE_CAP: usize = 512;

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
    body_cache: Mutex<LruCache<String, Arc<PagedDocument>>>,
    /// Per-object rasterized-sprite cache. Keyed by the effective render state
    /// (label + body source + quantized scale/rotation/position + ppi). This is
    /// the *second* performance layer on top of `body_cache`: even after the
    /// Typst source is memoized, rasterizing it to RGBA (`render`) is expensive,
    /// so identical states reuse the previously rasterized frame. The page
    /// size / canvas is constant, so the cached `RenderedFrame` composites
    /// directly. `Mutex` for the same `&self`-shared reason as `body_cache`.
    ///
    /// Bounded LRU: animated objects produce a distinct key every frame, so an
    /// unbounded `HashMap` would accumulate one sprite per frame and OOM. The
    /// LRU evicts that per-frame churn while keeping stable (paused) keys
    /// resident — see [`LruCache`].
    sprite_cache: Mutex<LruCache<SpriteKey, Arc<CachedSprite>>>,
    /// Memoized `#scene(bg: …)` expression → resolved `#rrggbb(aa)` hex.
    bg_cache: Mutex<HashMap<String, String>>,
    /// The stable, *parameterized* whole-document source used by the native
    /// Typst render path. Built once (in [`Renderer::with_root`]) from the
    /// parsed artifacts: every animatable mobject body is wrapped in a
    /// `sys.inputs.at("candy:<label>:…")` reader and every `#scene` call is
    /// gated by `sys.inputs.at("candy:active_scene")`. Because this string
    /// never changes across frames, the `source_cache` (parse) and
    /// `body_cache` (compile) hit on every frame — only the per-frame `inputs`
    /// dictionary varies, and that is supplied to the World without touching
    /// the source. `String::is_empty()` ⇒ no artifacts ⇒ legacy path.
    param_source: String,
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
        // Build the stable parameterized whole-document source once (only when
        // the parsed `.tyx` carries render artifacts; legacy hand-built scenes
        // leave it empty and use the per-object path).
        let param_source = if scene.artifacts.source.is_empty() {
            String::new()
        } else {
            Self::build_parameterized_source(&scene)
        };
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
            body_cache: Mutex::new(LruCache::with_capacity(BODY_CACHE_CAP)),
            sprite_cache: Mutex::new(LruCache::with_capacity(SPRITE_CACHE_CAP)),
            bg_cache: Mutex::new(HashMap::new()),
            param_source,
        })
    }
    /// Compile a Typst source string into a single-page document.
    fn compile(&self, src: &str, inputs: &Dict) -> Result<PagedDocument, CandyError> {
        let source = self.state.detached_cached(src);
        let world = CandyWorld::new(&self.state, source, inputs.clone());
        // Typst can *panic* (rather than return a diagnostic) on certain
        // malformed input — especially in release builds, where such a panic
        // would otherwise abort the process with no diagnostic. Catch it and
        // surface it as `E006` so a syntax error is always reported, never
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
                    return Err(CandyError::Typst(format!("typst panicked: {msg}")));
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
        warned.output.map_err(Into::into)
    }
    /// Compile a Typst source, memoized by the exact source string.
    ///
    /// This is the unified compile entry point for every object render path
    /// (`render_object_svg`, `render_object_pixels`, `render_frame`). It is
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
    fn resolve_bg_hex(&self, bg: &str) -> Result<String, CandyError> {
        if let Some(c) = self.bg_cache.lock().unwrap().get(bg) {
            return Ok(c.clone());
        }
        let src = format!("#set page(width: 1pt, height: 1pt, margin: 0pt, fill: {bg})\n#rect()");
        // A compile failure (e.g. a syntax error inside `bg`) is a real error and
        // must propagate as `E006`. Only a *successful* compile whose fill is not
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
        // Synthetic mobjects created by `#transform` (e.g. `__xf_eq_0`) are parked
        // copies of the *target* content. They must share the target's natural
        // position — if they were laid out as separate blocks they would push
        // the target and every later mobject down the page, making formulas fall
        // off-screen and causing the old content to render as a displaced ghost
        // while the target is translated.
        let mut tmp_to_target: HashMap<Label, Label> = HashMap::new();
        for p in &self.scene.transform_plans {
            if p.old.0.starts_with("__xf_") {
                tmp_to_target.insert(p.old.clone(), p.target.clone());
            }
        }
        for p in &self.scene.morph_pairs {
            if p.from.0.starts_with("__xf_") {
                tmp_to_target.insert(p.from.clone(), p.to.clone());
            }
        }
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
                // Synthetic `#transform` tmps are not laid out as their own
                // blocks; they inherit the target's natural position below.
                if tmp_to_target.contains_key(label) {
                    continue;
                }
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
            let doc = match self.compile(&src, &Dict::new()) {
                Ok(d) => d,
                // A scene whose blocks fail to compile is a real error — it must
                // propagate as `E006`, not be silently skipped (which would leave
                // the scene's `page_of` / page-count entries missing and surface as
                // a confusing downstream error later).
                Err(e) => return Err(e),
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
        // Synthetic `#transform` tmps inherit their target's natural position (and
        // page). The scheduler positions them relative to the target, so this keeps
        // old-content crossfades / morphs aligned with the target instead of
        // drifting down the page and ghosting as a duplicate.
        for (tmp, target) in &tmp_to_target {
            if let Some((x, y)) = nat.get(target).copied() {
                nat.insert(tmp.clone(), (x, y));
            }
            if let Some(p) = page_of.get(target).copied() {
                page_of.insert(tmp.clone(), p);
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
            let (fr, _, _) = match self.body_largest_shape(fb) {
                Ok(Some(r)) => r,
                // No extractable outline → not morphable; skip this pair
                // (legitimate fallback to a plain crossfade), not an error.
                Ok(None) => continue,
                Err(e) => return Err(e),
            };
            let (tr, fill, stroke) = match self.body_largest_shape(tb) {
                Ok(Some(r)) => r,
                Ok(None) => continue,
                Err(e) => return Err(e),
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
    ) -> Result<Arc<CachedSprite>, CandyError> {
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
            return Ok(cached.clone());
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
        let doc = self.compile_cached(&placed, &Dict::new())?;
        let page = doc
            .pages()
            .first()
            .ok_or_else(|| CandyError::Typst("document produced no pages".into()))?;
        let opts = RenderOptions {
            pixel_per_pt: Scalar::new(pixel_per_pt as f64),
            render_bleed: false,
        };
        let pix = render(page, &opts);
        let full = crate::renderer::RenderedFrame {
            width: pix.width() as usize,
            height: pix.height() as usize,
            rgba: pix.data().to_vec(),
        };
        // Crop to the object's tight bounding box. This is the core HD/high-FPS
        // OOM fix: without it the cache would hold full-canvas RGBA frames
        // (~8MB at 1080p), and 512 of them alone would blow memory. The crop is
        // only the object's ink (KB), and `ox`/`oy` let the compositor paste it
        // back at the exact page position so output is bit-identical to
        // compositing the full page.
        let sprite = Arc::new(crop_to_content(&full.rgba, full.width, full.height));
        self.sprite_cache
            .lock()
            .unwrap()
            .insert(key, sprite.clone());
        Ok(sprite)
    }
    // =========================================================================
    // Whole-document native-Typst render path (the authentic typesetting model)
    // =========================================================================
    //
    // Instead of re-placing every mobject on a full canvas (the old approach,
    // which both risked layout drift and blew memory on one full-page
    // `PagedDocument` per object), we let Typst typeset the *entire* document
    // natively each frame. Every mobject body is wrapped in
    // `#move`/`#scale`/`#rotate` (all exist in typst 0.15) so the animation is
    // just a code expansion driven by the eased per-frame counters — exactly the
    // "easing-counter → Typst code expansion" model. Static content stays in
    // native flow, so positions and Z-order are always correct and static +
    // dynamic content is freely interleaved.
    //
    // Per-object *opacity* is the one thing typst 0.15 cannot express in-document
    // (there is no `opacity()` function), so fading objects are omitted from the
    // base document and drawn as a small, object-sized opacity overlay on top.
    // Everything else is a single native compile → far fewer compiles than the
    // old N-objects-per-frame path and a tightly bounded `body_cache`.

    /// Build the stable, *parameterized* whole-document source from the parsed
    /// `Scene` — done **once** (in [`Renderer::with_root`]), never per frame.
    ///
    /// The result is byte-stable across frames: every per-frame-varying quantity
    /// is read from `sys.inputs` (the per-frame `Dict` supplied to the World), so
    /// the `source_cache` / `body_cache` keep hitting while the rendered document
    /// still changes per frame. Specifically:
    ///
    /// * Every animatable mobject body is wrapped (via [`wrap_mobject_inputs`])
    ///   so its transform (dx/dy/scale/rotation, opacity) is read from
    ///   `sys.inputs`.
    /// * `ecval("name")` counter reads inside mobject bodies are rewritten to
    ///   `sys.inputs.at("candy:counter:name", default: 0)` (see [`ecval_to_inputs`])
    ///   so the live counter value is also an input.
    /// * A `reveal`/`typewriter` target whose body is a string literal is wrapped
    ///   so the revealed prefix length comes from `sys.inputs.at(
    ///   "candy:<label>:reveal:len")` (see [`reveal_wrap_body`]) — the typewriter
    ///   effect is then pure input variation, no source change.
    /// * Every `#subtitle(...)` call is blanked to `#none` so the caption is NOT
    ///   rendered as part of the base document (it is drawn as a separate,
    ///   camera-independent overlay — leaving it in the base double-renders it).
    /// * Every `#scene` call is gated by `sys.inputs.at("candy:active_scene")` so
    ///   only the active scene emits a page — keeping every Typst invocation to a
    ///   single page and the `body_cache` hit rate high.
    ///
    /// Edits are applied in **character space** (not raw bytes) so a cumulative
    /// shift can never land inside a multi-byte character — this is what made
    /// the old per-frame `replace_range` byte-splicing panic ("end of range
    /// should be a character boundary") impossible.
    fn build_parameterized_source(scene: &Scene) -> String {
        let src = &scene.artifacts.source;
        let chars: Vec<char> = src.chars().collect();
        // `(char_start, char_end, replacement)` — `start == end` is an insertion.
        let mut edits: Vec<(usize, usize, String)> = Vec::new();
        // 1. Wrap each mobject body with the `sys.inputs`-driven transform,
        //    rewriting `ecval(...)` reads and `reveal`/typewriter prefixes to
        //    inputs so the body source stays byte-stable.
        for (label, &(bs, be)) in &scene.artifacts.mobject_body {
            let body = &src[bs..be];
            // `ecval("name")` → `sys.inputs.at("candy:counter:name", …)` so the
            // live counter value is supplied per frame as an input.
            let mut inner = Self::ecval_to_inputs(body);
            // A `reveal`/`typewriter` target whose body is a string literal gets
            // its revealed length driven by an input too.
            if let Some(full) = scene
                .items
                .get(label)
                .and_then(|b| strip_string_literal(b))
            {
                if scene.content_timeline.contains_key(label) {
                    inner = Self::reveal_wrap_body(&label.0, &inner, full.chars().count());
                }
            }
            let cs = src[..bs].chars().count();
            let ce = src[..be].chars().count();
            edits.push((cs, ce, Self::wrap_mobject_inputs(&label.0, &inner)));
        }
        // 2. Gate each `#scene` call so only the active scene emits a page.
        //    Insert the opening guard *before* the call and the closing brace
        //    *after* it; insertions (`start == end`) splice into `out`. The
        //    `scene_call` range excludes the leading `#` (markup prefix), so
        //    prepending `open` (which starts with `{`, not `#`) yields
        //    `#{ if <cond> { scene(…) } }`: the original `#` becomes the
        //    code-block entry `#{`, and `scene(…)` is a *code-mode* call (Typst
        //    calls functions without `#` inside code). The scene body `[…]` is
        //    markup, so the `#mobject` calls inside it stay valid. A false
        //    condition yields `none` (no page), so the compile emits exactly one
        //    page (the active scene).
        for (&sid, &(cs_b, ce_b)) in &scene.artifacts.scene_call {
            let cs = src[..cs_b].chars().count();
            let ce = src[..ce_b].chars().count();
            let open =
                format!("{{ if sys.inputs.at(\"candy:active_scene\", default: 0) == {} {{ ", sid);
            edits.push((cs, cs, open));
            edits.push((ce, ce, " } }".to_string()));
        }
        // 3. Blank each `#subtitle(...)` call out of the base document (it is
        //    drawn as a separate, camera-independent overlay). The `subtitle_call`
        //    range already includes the leading `#`, so replacing it with `#none`
        //    yields a no-op caption in the base.
        for (_, &(ss, se)) in &scene.artifacts.subtitle_call {
            let cs = src[..ss].chars().count();
            let ce = src[..se].chars().count();
            edits.push((cs, ce, "#none".to_string()));
        }
        // Apply right-to-left (descending start) so nested ranges stay correct.
        edits.sort_by(|a, b| b.0.cmp(&a.0));
        let mut out: Vec<char> = chars;
        for (s, e, rep) in edits {
            let rep_chars: Vec<char> = rep.chars().collect();
            out.splice(s..e, rep_chars);
        }
        let mut out = out.iter().collect::<String>();
        // Rewrite the bare `#import "candy"` form to the `@preview/candy` package
        // form so the World can resolve it in-process (see `WorldState::candy_local`).
        out = out
            .replace("#import \"candy\":", "#import \"@preview/candy:0.1.0\":")
            .replace("#import \"candy\"", "#import \"@preview/candy:0.1.0\"");
        out
    }

    /// Rewrite every `ecval("name")` / `ecval(name)` counter read in `body` to a
    /// `sys.inputs.at("candy:counter:name", default: 0)` reference, so the live
    /// counter value is supplied per frame as a `sys.inputs` entry (see
    /// [`Renderer::build_frame_inputs`]) instead of being hard-coded. The source
    /// stays byte-stable, so the `body_cache` keeps hitting.
    ///
    /// AST-driven (like [`crate::renderer::typst::content::substitute_counters`])
    /// so it never rewrites a substring that merely *looks* like the call (inside
    /// a string / comment). Only counters actually declared in the scene are
    /// rewritten; an undeclared `ecval` is left untouched (and resolves to `0`
    /// via the `default`, matching the legacy behaviour).
    fn ecval_to_inputs(body: &str) -> String {
        if !body.contains("ecval") {
            return body.to_string();
        }
        let root = parse_code(body);
        let node = LinkedNode::new(&root);
        let mut edits: Vec<(std::ops::Range<usize>, String)> = Vec::new();
        Self::collect_ecval_input_edits(&node, &mut edits);
        if edits.is_empty() {
            return body.to_string();
        }
        // Drop any edit whose range is nested inside another (keep innermost).
        let drop: Vec<bool> = edits
            .iter()
            .map(|(r, _)| edits.iter().any(|(o, _)| o != r && o.start <= r.start && r.end <= o.end))
            .collect();
        let mut kept: Vec<(std::ops::Range<usize>, String)> = Vec::new();
        for (keep, e) in drop.into_iter().zip(edits) {
            if !keep {
                kept.push(e);
            }
        }
        let mut edits = kept;
        edits.sort_by(|a, b| b.0.start.cmp(&a.0.start));
        let mut out = body.to_string();
        for (range, text) in edits {
            out.replace_range(range, &text);
        }
        out
    }

    /// Collect `(source range → input reference)` edits for every `ecval(name)`
    /// call in `node`. (We rewrite every `ecval` read unconditionally — the
    /// `default: 0` fallback matches the legacy behaviour for undeclared
    /// counters, and declared ones get their live value via `build_frame_inputs`.)
    fn collect_ecval_input_edits(node: &LinkedNode, edits: &mut Vec<(std::ops::Range<usize>, String)>) {
        if let Some(call) = node.get().cast::<ast::FuncCall>() {
            if let Some(name) = Self::ecval_input_name(&call) {
                let key = format!("candy:counter:{name}");
                edits.push((node.range(), format!("sys.inputs.at(\"{key}\", default: 0)")));
            }
        }
        for child in node.children() {
            Self::collect_ecval_input_edits(&child, edits);
        }
    }

    /// If `call` is an `ecval(..)` read, return the counter name it references
    /// (the canonical `ecval("name")` string form, or the bare-ident `ecval(name)`
    /// form for backwards compatibility).
    fn ecval_input_name(call: &ast::FuncCall) -> Option<String> {
        let is_ecval = match call.callee() {
            Expr::Ident(id) => id.as_str() == "ecval",
            Expr::FieldAccess(fa) => fa.field().as_str() == "ecval",
            _ => false,
        };
        if !is_ecval {
            return None;
        }
        for a in call.args().items() {
            if let ast::Arg::Pos(p) = a {
                return match p {
                    Expr::Str(s) => Some(s.get().to_string()),
                    Expr::Ident(i) => Some(i.as_str().to_string()),
                    _ => None,
                };
            }
            break;
        }
        None
    }

    /// Wrap a string-literal mobject body so its revealed prefix length is read
    /// from `sys.inputs.at("candy:<label>:reveal:len", default: <full_len>)`.
    /// Used for `reveal`/`typewriter` targets: the typewriter effect becomes pure
    /// per-frame input variation (no source change), so the `body_cache` keeps
    /// hitting. `inner` is the (already `ecval`-substituted) body expression —
    /// for a string target it is the string literal `"..."`.
    fn reveal_wrap_body(label: &str, inner: &str, full_len: usize) -> String {
        // The revealed length `__n` is a *codepoint* count (the renderer supplies
        // `full.chars().count()`), but Typst's `str.slice(start, end)` indexes by
        // *byte* offset and panics ("string index N is not a character boundary")
        // when the prefix ends inside a multi-byte character (e.g. an em-dash).
        // Slice the codepoint array instead so the prefix is always taken on a
        // character boundary; `calc.min` clamps against any out-of-range `__n`.
        format!(
            "{{ let __full = ({inner}); let __cp = str(__full).codepoints(); let __n = calc.min(int(sys.inputs.at(\"candy:{label}:reveal:len\", default: {full_len})), __cp.len()); __cp.slice(0, __n).join() }}",
            inner = inner,
            label = label,
            full_len = full_len,
        )
    }

    /// Wrap a mobject body in a `sys.inputs`-driven transform.
    ///
    /// `inner` is the (already `ecval`-substituted, and possibly `reveal`-wrapped)
    /// body expression. The wrapper reads the per-frame eased transform from
    /// `sys.inputs` (supplied by the World each frame) instead of embedding
    /// literal numbers, so the *source* stays byte-stable across frames and the
    /// caches keep hitting. The body is the *positional argument* of
    /// `#mobject(label, body)` and therefore sits in **code mode** — so the
    /// wrapper is a bare `{ … }` code block (NOT `#{ … }`). A `#{ … }` here would
    /// be parsed as a markup code block *inside* the `#mobject(…)` argument list
    /// and break parsing ("`#` is not valid in code"); a bare `{ … }` is a valid
    /// code-mode block.
    fn wrap_mobject_inputs(label: &str, inner: &str) -> String {
        format!(
            "{{ let __b = ({inner}); if sys.inputs.at(\"candy:{label}:hide\", default: false) {{ hide(__b) }} else {{ move(dx: sys.inputs.at(\"candy:{label}:dx\", default: 0) * 1cm, dy: sys.inputs.at(\"candy:{label}:dy\", default: 0) * 1cm, scale(origin: top + left, sys.inputs.at(\"candy:{label}:s\", default: 100) * 1%, rotate(origin: top + left, sys.inputs.at(\"candy:{label}:r\", default: 0) * 1deg, __b))) }} }}",
            inner = inner,
            label = label,
        )
    }

    /// Build the per-frame `sys.inputs` dictionary for the whole-document path.
    ///
    /// `hide_fading` controls whether opacity < 1 objects get a `…:hide` flag
    /// (the pixel path draws them via the opacity overlay; the SVG draft shows
    /// them at full opacity, so it passes `false`).
    fn build_frame_inputs(
        &self,
        states: &HashMap<Label, FrameData>,
        active: usize,
        active_page: usize,
        hide_fading: bool,
        time_ms: u32,
    ) -> Dict {
        let mut inputs = Dict::new();
        if !self.scene.scenes.is_empty() {
            inputs.insert("candy:active_scene".into(), Value::Int(active as i64));
        }
        for (label, st) in states {
            let owner = self.label_scene.get(label).copied().unwrap_or(active);
            if owner != active {
                continue;
            }
            if let Some(p) = self.pages.page_of(label) {
                if p != active_page {
                    continue;
                }
            }
            let l = &label.0;
            if self.transform_hidden(label, time_ms) {
                // The target/old mobjects are replaced by the interpolated
                // per-glyph fragments, so hide them in the base document.
                inputs.insert(format!("candy:{l}:hide").into(), Value::Bool(true));
                continue;
            }
            inputs.insert(format!("candy:{l}:dx").into(), Value::Float(st.x));
            inputs.insert(format!("candy:{l}:dy").into(), Value::Float(st.y));
            inputs.insert(
                format!("candy:{l}:s").into(),
                Value::Float(st.scale * 100.0),
            );
            inputs.insert(format!("candy:{l}:r").into(), Value::Float(st.rotation));
            if hide_fading && st.opacity < 1.0 - 1e-4 {
                inputs.insert(format!("candy:{l}:hide").into(), Value::Bool(true));
            }
        }
        // Easing-counter values: each declared counter's live value at this
        // frame is supplied as `candy:counter:<name>`, matching the
        // `ecval_to_inputs` rewrite in `build_parameterized_source`. This keeps
        // the source byte-stable (only the inputs vary) so the `body_cache` hits.
        for c in &self.scene.counters {
            let v = self.scene.counter_value_at(&c.name, time_ms);
            inputs.insert(format!("candy:counter:{}", c.name).into(), Value::Int(v));
        }
        // `reveal`/`typewriter` revealed-prefix lengths: each string target's
        // revealed character count at this frame is supplied as
        // `candy:<label>:reveal:len`, matching `reveal_wrap_body`.
        for (label, _timeline) in &self.scene.content_timeline {
            let Some(full) = self.scene.items.get(label).and_then(|b| strip_string_literal(b)) else {
                continue;
            };
            let len = Self::reveal_len_at(&self.scene, label, time_ms, full.chars().count());
            inputs.insert(
                format!("candy:{}:reveal:len", label.0).into(),
                Value::Int(len as i64),
            );
        }
        inputs
    }

    /// Resolve the revealed character count of a `reveal`/`typewriter` target at
    /// `time_ms`, following its `content_timeline` (`(t, "prefix")` entries).
    ///
    /// Mirrors the legacy `content_for` fallback: the latest timeline entry with
    /// `t <= time_ms` wins; `"none"` → `0` (hidden), otherwise the prefix's char
    /// length. Before any timeline entry the original (full) body length is used.
    fn reveal_len_at(scene: &Scene, label: &Label, time_ms: u32, full_len: usize) -> usize {
        let Some(timeline) = scene.content_timeline.get(label) else {
            return full_len;
        };
        let mut chosen: Option<&String> = None;
        for (t, body) in timeline {
            if *t <= time_ms {
                chosen = Some(body);
            }
        }
        let body = match chosen {
            Some(b) => b,
            None => return full_len,
        };
        if body == "none" {
            return 0;
        }
        strip_string_literal(body)
            .map(|s| s.chars().count())
            .unwrap_or(0)
    }

    /// Whole-document native-Typst pixel frame (see the module note above).
    fn render_frame_pixels_whole_doc_par(
        &self,
        time_ms: u32,
        all_frames: &[FrameData],
        pixel_per_pt: f32,
    ) -> Result<crate::renderer::RenderedFrame, CandyError> {
        let (states, camera) = self.prepare_states(all_frames, time_ms);
        let active = if self.scene.scenes.is_empty() {
            0
        } else {
            self.scene.active_scene_at(time_ms)
        };
        let active_page = self.pages.active_page_of(active, time_ms);
        let (pw, ph) = if self.scene.scenes.is_empty() {
            (self.page_w, self.page_h)
        } else {
            self.scene_pages
                .get(&active)
                .copied()
                .unwrap_or((self.page_w, self.page_h))
        };
        // The source is stable (`param_source`); only the per-frame `inputs`
        // vary. Compiling yields exactly the active scene's page.
        let inputs = self.build_frame_inputs(&states, active, active_page, true, time_ms);
        let doc = self.compile_cached(&self.param_source, &inputs)?;
        let page = doc
            .pages()
            .get(active_page)
            .or_else(|| doc.pages().first())
            .ok_or_else(|| CandyError::Typst("document produced no pages".into()))?;
        let opts = RenderOptions {
            pixel_per_pt: Scalar::new(pixel_per_pt as f64),
            render_bleed: false,
        };
        let pix = render(page, &opts);
        let w = pix.width() as usize;
        let h = pix.height() as usize;
        let mut canvas: Vec<u8> = pix.data().to_vec();
        // Opacity-overlay pass: draw fading objects (typst 0.15 has no
        // `opacity()`, so they were hidden in the base document above).
        for (label, st) in &states {
            if st.opacity >= 1.0 - 1e-4 {
                continue;
            }
            let owner = self.label_scene.get(label).copied().unwrap_or(active);
            if owner != active {
                continue;
            }
            if let Some(p) = self.pages.page_of(label) {
                if p != active_page {
                    continue;
                }
            }
            if self.transform_hidden(label, time_ms) {
                continue;
            }
            let sprite = self.render_object_pixels(label, st, time_ms, pw, ph, pixel_per_pt)?;
            composite_over_at(
                &mut canvas,
                &sprite.frame,
                st.opacity,
                sprite.ox as f64,
                sprite.oy as f64,
                w,
                h,
            );
        }
        // Per-glyph `#transform` overlay (Manim-style fragment tween): kept as
        // SVG and rasterized ONCE at the final step (the formula is embedded once
        // in `<defs>`, not copied per fragment), then composited over the base
        // canvas — so it is warped by the camera together with the rest.
        if let Some(overlay) =
            self.render_transform_overlay_pixels(&states, time_ms, pixel_per_pt, pw, ph)?
        {
            composite_over_at(&mut canvas, &overlay, 1.0, 0.0, 0.0, w, h);
        }
        // Camera warp (subtitles are composited afterwards, so they stay fixed).
        if let Some(cam) = &camera {
            let bg = if self.scene.scenes.is_empty() {
                [255u8, 255, 255, 255]
            } else {
                Self::hex_to_rgba(&self.scene_bg_hex(active)?)
            };
            warp_canvas_with_camera(&mut canvas, w, h, cam, pw, ph, pixel_per_pt, bg);
        }
        // Subtitle overlay (topmost, independent Typst layer).
        for sub in &self.scene.subtitles {
            if self
                .scene
                .visible_subtitle_ids_at(time_ms)
                .contains(&sub.id)
            {
                let frame = self.render_subtitle_pixels(sub, time_ms, pixel_per_pt)?;
                composite_over_at(
                    &mut canvas,
                    &frame.frame,
                    1.0,
                    frame.ox as f64,
                    frame.oy as f64,
                    w,
                    h,
                );
            }
        }
        Ok(crate::renderer::RenderedFrame {
            width: w,
            height: h,
            rgba: canvas,
        })
    }

    /// Whole-document native-Typst SVG draft (compatible standard Typst SVG,
    /// not the hand-rolled composite the old path emitted — so it opens in any
    /// viewer, not just Inkscape). The per-glyph `#transform` overlay and the
    /// subtitles are injected here, so the draft is the single source of truth
    /// for the whole frame (the "new" SVG path).
    fn render_frame_at_whole_doc(
        &self,
        time_ms: u32,
        all_frames: &[FrameData],
    ) -> Result<Vec<u8>, CandyError> {
        let (states, camera) = self.prepare_states(all_frames, time_ms);
        let active = if self.scene.scenes.is_empty() {
            0
        } else {
            self.scene.active_scene_at(time_ms)
        };
        let active_page = self.pages.active_page_of(active, time_ms);
        let (pw, ph) = if self.scene.scenes.is_empty() {
            (self.page_w, self.page_h)
        } else {
            self.scene_pages
                .get(&active)
                .copied()
                .unwrap_or((self.page_w, self.page_h))
        };
        // `hide_fading = false`: the draft shows fading objects at full opacity
        // (typst 0.15 cannot express per-object opacity in-document).
        let inputs = self.build_frame_inputs(&states, active, active_page, false, time_ms);
        let doc = self.compile_cached(&self.param_source, &inputs)?;
        let page = doc
            .pages()
            .get(active_page)
            .or_else(|| doc.pages().first())
            .ok_or_else(|| CandyError::Typst("document produced no pages".into()))?;
        let base = typst_svg::svg(page, &SvgOptions::default());
        // Compose the full draft: base document + (camera group wrapping the
        // mobjects and the per-glyph transform overlay) + subtitles. The transform
        // overlay is the "new" SVG path — the formula is embedded once in `<defs>`
        // and reused via `<use>`, so it is never copied many times.
        let out = self.compose_frame_svg(&base, &states, time_ms, &camera, pw, ph)?;
        Ok(out.into_bytes())
    }

    /// Compose the full draft SVG for one frame: the base document (typst_svg
    /// output) with the per-glyph `#transform` overlay and subtitles injected.
    ///
    /// The base document's mobjects and the transform overlay are wrapped in a
    /// single camera group (so they pan/zoom/rotate with the view together),
    /// while the background `<rect>` and the subtitle overlays stay fixed (drawn
    /// outside the camera group). The transform overlay embeds each formula once
    /// in `<defs>` and references it via `<use>`, so the formula is never
    /// duplicated.
    fn compose_frame_svg(
        &self,
        base_svg: &str,
        states: &HashMap<Label, FrameData>,
        time_ms: u32,
        camera: &Option<FrameData>,
        pw: f64,
        ph: f64,
    ) -> Result<String, CandyError> {
        // Extract the inner markup of the typst_svg document (between `<svg …>`
        // and `</svg>`), then split the leading background `<rect>` (which must
        // stay fixed, outside the camera group) from the mobject content.
        let open = base_svg
            .find("<svg")
            .ok_or_else(|| CandyError::Typst("bad svg".into()))?;
        let after = open
            + base_svg[open..]
                .find('>')
                .ok_or_else(|| CandyError::Typst("bad svg".into()))?
            + 1;
        let end = base_svg
            .rfind("</svg>")
            .ok_or_else(|| CandyError::Typst("bad svg".into()))?;
        let inner = &base_svg[after..end];
        let (bg, content) = match inner.find("<rect") {
            Some(i) => {
                let rend = i
                    + inner[i..]
                        .find("/>")
                        .ok_or_else(|| CandyError::Typst("bad svg".into()))?
                    + 2;
                (&inner[i..rend], &inner[rend..])
            }
            None => ("", inner),
        };
        let mut out = String::new();
        out.push_str(&format!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\" xmlns:xlink=\"http://www.w3.org/1999/xlink\" width=\"{pw}\" height=\"{ph}\" viewBox=\"0 0 {pw} {ph}\">\n",
            pw = pw, ph = ph,
        ));
        out.push_str(bg);
        out.push('\n');
        if let Some(cam) = camera {
            out.push_str(&format!("<g transform=\"{}\">\n", camera_transform_svg(cam, pw, ph)));
        }
        out.push_str(content);
        out.push('\n');
        out.push_str(&self.transform_overlay_svg(states, time_ms));
        if camera.is_some() {
            out.push_str("</g>\n");
        }
        for sub in &self.scene.subtitles {
            if self.scene.visible_subtitle_ids_at(time_ms).contains(&sub.id) {
                out.push_str(&self.render_subtitle_svg(sub, time_ms)?);
                out.push('\n');
            }
        }
        out.push_str("</svg>\n");
        Ok(out)
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
    /// Dispatch to the whole-document native-Typst path when the parsed source
    /// carries render artifacts (real `.tyx` inputs), otherwise fall back to the
    /// legacy per-object compositing path (hand-built test scenes without
    /// artifacts). Both satisfy the same `FrameData → RGBA` contract.
    pub fn render_frame_pixels_par(
        &self,
        time_ms: u32,
        all_frames: &[FrameData],
        pixel_per_pt: f32,
    ) -> Result<crate::renderer::RenderedFrame, CandyError> {
        if !self.scene.artifacts.source.is_empty() {
            return self.render_frame_pixels_whole_doc_par(time_ms, all_frames, pixel_per_pt);
        }
        self.render_frame_pixels_legacy_par(time_ms, all_frames, pixel_per_pt)
    }

    /// Legacy per-object compositing path (kept for hand-built test scenes that
    /// carry no parsed `artifacts`). See [`render_frame_pixels_par`].
    fn render_frame_pixels_legacy_par(
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
        let mut objs: Vec<(f64, Arc<CachedSprite>)> = Vec::new();
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
            let sprite = self.render_object_pixels(*label, st, time_ms, pw, ph, pixel_per_pt)?;
            objs.push((st.opacity, sprite));
        }
        // Subtitle overlays are collected separately: they must be composited
        // AFTER the global camera warp so they stay pinned at a fixed page
        // position/size regardless of the current view (pan/zoom/rotate).
        let mut subs: Vec<CachedSprite> = Vec::new();
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
            Self::hex_to_rgba(&self.scene_bg_hex(active)?)
        };
        let mut canvas = vec![0u8; w * h * 4];
        for chunk in canvas.chunks_mut(4) {
            chunk.copy_from_slice(&bg_rgba);
        }
        for (opacity, sprite) in &objs {
            // Paste the cropped sprite at its page-pixel offset so the result is
            // identical to compositing the full page (but at a fraction of the
            // memory: the sprite is only the object's ink, not the whole canvas).
            composite_over_at(
                &mut canvas,
                &sprite.frame,
                *opacity,
                sprite.ox as f64,
                sprite.oy as f64,
                w,
                h,
            );
        }
        // Per-glyph `#transform` overlay (Manim-style): kept as SVG and
        // rasterized ONCE at the final step (the formula is embedded once in
        // `<defs>`, not copied per fragment), then composited over the base
        // canvas so it is warped by the camera together with the other mobjects.
        if let Some(overlay) =
            self.render_transform_overlay_pixels(&states, time_ms, pixel_per_pt, pw, ph)?
        {
            composite_over_at(&mut canvas, &overlay, 1.0, 0.0, 0.0, w, h);
        }
        // Apply the global camera (pan + zoom + rotate) by warping the
        // composited object canvas through the inverse camera transform.
        // Subtitles are deliberately NOT warped here — they are overlaid
        // afterwards so they remain at a fixed page position and fixed size
        // no matter what the camera does.
        if let Some(cam) = &camera {
            warp_canvas_with_camera(&mut canvas, w, h, cam, pw, ph, pixel_per_pt, bg_rgba);
        }
        // Overlay subtitles on top of the warped canvas, at their fixed
        // page-anchored positions. They are cropped (offset stored), so paste
        // at the offset to reproduce the full-page position.
        for s in &subs {
            composite_over_at(&mut canvas, &s.frame, 1.0, s.ox as f64, s.oy as f64, w, h);
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
    /// Dispatch to the whole-document native-Typst SVG path (compatible
    /// standard Typst SVG) when artifacts are present, else the legacy
    /// hand-composed SVG (test scenes).
    pub fn render_frame_at(
        &mut self,
        time_ms: u32,
        all_frames: &[FrameData],
    ) -> Result<Vec<u8>, CandyError> {
        self.ensure_natural()?;
        if !self.scene.artifacts.source.is_empty() {
            return self.render_frame_at_whole_doc(time_ms, all_frames);
        }
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
            self.scene_bg_hex(active)?
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
        let doc = self.compile_cached(&src, &Dict::new())?;
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
        let doc = self.compile_cached(&self.object_source(frame, frame.time_ms), &Dict::new())?;
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
        render_subtitle_svg_impl(
            &self.state,
            &self.scene,
            sub,
            self.page_w,
            self.page_h,
            time_ms,
        )
    }
    /// Render a subtitle to an RGBA frame (page-sized) for the pixel path.
    fn render_subtitle_pixels(
        &self,
        sub: &Subtitle,
        time_ms: u32,
        pixel_per_pt: f32,
    ) -> Result<CachedSprite, CandyError> {
        let doc = subtitle_doc(
            &self.state,
            &self.scene,
            sub,
            self.page_w,
            self.page_h,
            time_ms,
        )?;
        let page = doc
            .pages()
            .first()
            .ok_or_else(|| CandyError::Typst("document produced no pages".into()))?;
        let opts = RenderOptions {
            pixel_per_pt: Scalar::new(pixel_per_pt as f64),
            render_bleed: false,
        };
        let pix = render(page, &opts);
        let full = crate::renderer::RenderedFrame {
            width: pix.width() as usize,
            height: pix.height() as usize,
            rgba: pix.data().to_vec(),
        };
        // Crop to the subtitle's tight bounding box (the text is positioned
        // inside a full page by `subtitle_place_expr`); the stored offset pastes
        // it back at the exact page position. This keeps per-frame allocation to
        // the text's ink instead of a full HD canvas, and the composite result
        // is identical to pasting the full page at (0,0).
        Ok(crop_to_content(&full.rgba, full.width, full.height))
    }
    /// Render a mobject body in isolation and return its largest outline shape
    /// (by absolute area) as a ring of points plus its paint. Returns `None` if
    /// the body produces no extractable outline (e.g. an image or a body whose
    /// shape candy can't morph — those fall back to the plain crossfade).
    fn body_largest_shape(
        &self,
        body: &str,
    ) -> Result<Option<(Vec<[f64; 2]>, Option<String>, Option<String>)>, CandyError> {
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
        // A compile failure (e.g. a syntax error in a morphable body) is a real
        // error and must propagate as `E006`. Only a *successful* compile that
        // yields no extractable outline legitimately returns `None` (the body
        // falls back to a plain crossfade).
        let doc = self.compile(&src, &Dict::new())?;
        let page = doc
            .pages()
            .first()
            .ok_or_else(|| CandyError::Typst("body produced no pages".into()))?;
        let svg = typst_svg::svg(page, &SvgOptions::default());
        let shapes = extract_shapes_from_svg(&svg);
        Ok(shapes
            .into_iter()
            .max_by(|a, b| {
                polygon_area(&a.ring)
                    .abs()
                    .partial_cmp(&polygon_area(&b.ring).abs())
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .map(|s| (s.ring, s.fill, s.stroke)))
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
    let world = CandyWorld::new(&state, source, Dict::new());
    let warned = typst::compile::<PagedDocument>(&world);
    match warned.output {
        Ok(doc) => {
            let page = doc
                .pages()
                .first()
                .ok_or_else(|| CandyError::Typst("no pages".into()))?;
            Ok(typst_svg::svg(page, &SvgOptions::default()))
        }
        Err(e) => Err(e.into()),
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
/// Regression test for the HD/high-FPS OOM fix: `crop_to_content` must shrink a
/// full-canvas RGBA frame that contains only a tiny opaque region down to a
/// small bounding-box sprite. This is what keeps `sprite_cache` (cap 512) at KB
/// instead of gigabytes — previously every cached sprite was the whole canvas.
#[test]
fn crop_to_content_shrinks_sparse_frame() {
    let w = 1920;
    let h = 1080;
    let mut rgba = vec![0u8; w * h * 4]; // fully transparent
    // Paint a 10×10 opaque red box near the top-left.
    for y in 5..15 {
        for x in 5..15 {
            let o = (y * w + x) * 4;
            rgba[o] = 255;
            rgba[o + 3] = 255;
        }
    }
    let sprite = crop_to_content(&rgba, w, h);
    // Cropped sprite is ~10×10, orders of magnitude smaller than the canvas.
    assert!(
        sprite.frame.width <= 12 && sprite.frame.height <= 12,
        "crop too large: {}x{}",
        sprite.frame.width,
        sprite.frame.height
    );
    assert_eq!(
        (sprite.ox, sprite.oy),
        (5, 5),
        "crop offset must be the bbox top-left"
    );
    // The opaque pixel is preserved at the right place in the crop.
    let o = (0 * sprite.frame.width + 0) * 4;
    assert_eq!((sprite.frame.rgba[o], sprite.frame.rgba[o + 3]), (255, 255));
    // A fully transparent frame yields a 1×1 transparent sprite at (0,0).
    let empty = crop_to_content(&vec![0u8; w * h * 4], w, h);
    assert_eq!(
        (empty.frame.width, empty.frame.height, empty.ox, empty.oy),
        (1, 1, 0, 0)
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
        artifacts: ParseArtifacts::default(),
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
        artifacts: ParseArtifacts::default(),
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
        artifacts: ParseArtifacts::default(),
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
        artifacts: ParseArtifacts::default(),
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
    let doc = r
        .compile(&src, &Dict::new())
        .expect("native layout compile");
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
        artifacts: ParseArtifacts::default(),
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
        artifacts: ParseArtifacts::default(),
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
    // target shows its new content — the whole-document render must therefore
    // emit glyph drawing (`<path`) beyond the background `<rect>`, not just an
    // empty background. Scenes are mutually exclusive slides, so this also
    // verifies the scene stays on stage (its interval is extended to the
    // document end) and the target is not hidden.
    let after = 90u32;
    let svg_after = String::from_utf8(r.render_frame_at(after, &frames).unwrap()).unwrap();
    let glyphs = svg_after.matches("<path").count();
    assert!(
        glyphs >= 1,
        "after window target must render its new content (glyph path count={glyphs})"
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
    let scene = crate::parser::ast_walk::parse_tyx(&tmp).unwrap();
    let frames = crate::core::scheduler::schedule(&scene).unwrap();
    let mut r = Renderer::with_root(scene, PathBuf::new()).unwrap();
    r.ensure_natural_public().unwrap();
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
               #animate(\"eq\", scale: 2.0, rotate: 30deg, duration: 60)\n\
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
/// push the target down the page via the natural layout (which would place the
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
    let tmp = std::env::temp_dir().join("candy_test_xf_chain.tyx");
    std::fs::write(&tmp, src).unwrap();
    let scene = crate::parser::ast_walk::parse_tyx(&tmp).unwrap();
    let frames = crate::core::scheduler::schedule(&scene).unwrap();
    let mut r = Renderer::with_root(scene, PathBuf::new()).unwrap();
    r.ensure_natural_public().unwrap();
    // Midpoint of the FIRST transform window: animate 0-60, first transform 61-120,
    // second transform 121-180. Mid of first window = 90.
    let mid = 90u32;
    let svg = String::from_utf8(r.render_frame_at(mid, &frames).unwrap()).unwrap();
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
    let scene = crate::parser::ast_walk::parse_tyx(&tmp).unwrap();
    let frames = crate::core::scheduler::schedule(&scene).unwrap();
    let mut r = Renderer::with_root(scene, PathBuf::new()).unwrap();
    r.ensure_natural_public().unwrap();
    let svg = String::from_utf8(r.render_frame_at(0, &frames).unwrap()).unwrap();
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
    // Native Typst wraps each mobject in a `<g>` group, so count those (the page
    // background is a `<path>`, not a `<g>`).
    let drawn = svg.matches("<g").count();
    assert!(
        drawn > 0 && drawn < 6,
        "first frame should show only the current page's mobjects (drew {drawn} of 6)"
    );
    std::fs::remove_file(&tmp).ok();
}
