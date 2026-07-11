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

use std::collections::HashMap;
use std::hash::Hash;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use typst::{Library, LibraryExt, World};
use typst_kit::files::{FileStore, FsRoot, SystemFiles};
use typst_kit::fonts::FontStore;
use typst_kit::packages::SystemPackages;
use typst_layout::PagedDocument;
use typst_library::diag::FileError;
use typst_library::foundations::{Bytes, Datetime, Duration, Smart};
use typst_library::text::Font;
use typst_library::visualize::Paint;
use typst_render::{RenderOptions, render};
use typst_svg::SvgOptions;
use typst_syntax::ast::{self, Expr};
use typst_syntax::{FileId, LinkedNode, Source as TypstSource};
use typst_utils::{LazyHash, Scalar};

use crate::core::ast::{FrameData, Label, Scene, SubPos, Subtitle};
use crate::core::error::CandyError;
use crate::core::morph::{MorphPlan, extract_shapes_from_svg, polygon_area};

#[cfg(test)]
use std::path::Path;
#[cfg(test)]
use typst_syntax::{RootedPath, VirtualPath, VirtualRoot};

/// Centimeters per Typst point (1pt = 1/72in, 1in = 2.54cm).
const PT_PER_CM: f64 = 28.346_456_692_913_385;

/// Maximum segment length (in Typst points) when bisecting morph outline rings.
/// Smaller = smoother morph but more points (the plan is sampled per frame, so
/// the per-frame cost is linear in the point count — 3pt is a good balance).
const MORPH_MAX_SEGMENT: f64 = 3.0;

/// A no-op downloader used when the `system-downloader` feature is disabled.
/// Returns NotFound for every URL, so @preview packages resolve only from
/// the local cache (pre-populated via `typst compile`).
#[cfg(not(feature = "system-downloader"))]
#[derive(Debug, Clone, Copy)]
struct NoDownload;

#[cfg(not(feature = "system-downloader"))]
impl typst_kit::downloader::Downloader for NoDownload {
    fn stream(
        &self,
        _key: &dyn std::any::Any,
        _url: &str,
    ) -> std::io::Result<(Option<usize>, Box<dyn std::io::Read>)> {
        Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "candy was built without the 'system-downloader' feature; \
             @preview packages must be pre-cached via 'typst compile'",
        ))
    }
}

/// @preview package downloader backed by `ureq` with the pure-Rust `rustls`
/// TLS backend (replaces typst-kit's `SystemDownloader`, which uses
/// `native-tls` + OpenSSL). This avoids linking the system OpenSSL entirely, so
/// the build stays self-contained and works for both host and cross targets
/// with no OpenSSL dev package and no perl. Root CAs come from the bundled
/// `webpki-roots`, so no system cert store is required.
#[cfg(feature = "system-downloader")]
struct RustlsDownloader {
    agent: ureq::Agent,
}

#[cfg(feature = "system-downloader")]
impl RustlsDownloader {
    fn new(user_agent: &str) -> Self {
        Self {
            agent: ureq::AgentBuilder::new().user_agent(user_agent).build(),
        }
    }
}

#[cfg(feature = "system-downloader")]
impl typst_kit::downloader::Downloader for RustlsDownloader {
    fn stream(
        &self,
        _key: &dyn std::any::Any,
        url: &str,
    ) -> std::io::Result<(Option<usize>, Box<dyn std::io::Read>)> {
        let response = self.agent.get(url).call().map_err(|err| match err {
            ureq::Error::Status(404, _) => std::io::Error::new(std::io::ErrorKind::NotFound, err),
            err => std::io::Error::other(err),
        })?;
        let content_len: Option<usize> = response
            .header("Content-Length")
            .and_then(|header| header.parse().ok());
        Ok((content_len, Box::new(response.into_reader())))
    }
}

/// Shared, reusable Typst World state (fonts + file resolver + standard
/// library). Built once per [`Renderer`] and reused across every frame
/// compile, so the cost of system font scanning is paid exactly once.
struct WorldState {
    library: LazyHash<Library>,
    fonts: FontStore,
    files: FileStore<SystemFiles>,
}

impl WorldState {
    /// Build a World state with:
    /// - the standard Typst library
    /// - embedded fallback fonts + all system fonts
    /// - a project root (the `.tyx` source's parent directory) so local
    ///   `#import "file.typ"` works, and `@preview` packages resolve from
    ///   the local cache (downloading on demand when the
    ///   `system-downloader` feature is enabled)
    fn new(project_root: PathBuf) -> Self {
        let library = LazyHash::new(Library::default());

        let mut fonts = FontStore::new();
        fonts.extend(typst_kit::fonts::embedded());
        fonts.extend(typst_kit::fonts::system());

        // Package resolver: @preview packages from the local cache, with
        // on-demand download (pure-Rust `rustls` TLS, no OpenSSL) when the
        // `system-downloader` feature is enabled.
        #[cfg(feature = "system-downloader")]
        let packages = SystemPackages::new(RustlsDownloader::new("candy/0.1"));
        #[cfg(not(feature = "system-downloader"))]
        let packages = SystemPackages::new(NoDownload);

        let root = FsRoot::new(project_root);
        let files = FileStore::new(SystemFiles::new(root, packages));

        Self {
            library,
            fonts,
            files,
        }
    }
}

/// A per-compile `World` view that borrows the shared [`WorldState`] and
/// fixes a specific `main` source.
struct CandyWorld<'a> {
    state: &'a WorldState,
    main: TypstSource,
}

impl<'a> World for CandyWorld<'a> {
    fn library(&self) -> &LazyHash<Library> {
        &self.state.library
    }

    fn book(&self) -> &LazyHash<typst_library::text::FontBook> {
        self.state.fonts.book()
    }

    fn main(&self) -> FileId {
        self.main.id()
    }

    fn source(&self, id: FileId) -> Result<TypstSource, FileError> {
        if id == self.main.id() {
            return Ok(self.main.clone());
        }
        // Delegate to the file store — this resolves local imports via FsRoot
        // and package imports via SystemPackages. The store caches, so
        // repeated imports of the same file are cheap.
        self.state.files.source(id)
    }

    fn file(&self, id: FileId) -> Result<Bytes, FileError> {
        self.state.files.file(id)
    }

    fn font(&self, index: usize) -> Option<Font> {
        self.state.fonts.font(index)
    }

    fn today(&self, _offset: Option<Duration>) -> Option<Datetime> {
        None
    }
}

/// Cache key for the per-object rasterized **sprite** cache (see
/// `sprite_cache`). Identical key ⇒ the object's full-canvas RGBA can be
/// reused without re-running Typst rasterization (`render`). Positions are
/// quantized to 0.01cm, scale to 0.1%, rotation to 0.1° — fine enough that
/// paused / static objects (the bulk of most timelines) hit the cache every
/// frame, while genuinely moving objects miss it (correctly re-rasterizing).
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
    /// Precomputed outline interpolators for `#morph` pairs, keyed by
    /// `(from, to)`. Built once in `ensure_natural` (the expensive part:
    /// render both bodies to SVG, extract + align their outline rings). Each
    /// frame then just samples the plan — this is the performance-first design.
    morph_cache: HashMap<(Label, Label), MorphPlan>,
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
            morph_cache: HashMap::new(),
            body_cache: Mutex::new(HashMap::new()),
            sprite_cache: Mutex::new(HashMap::new()),
            bg_cache: Mutex::new(HashMap::new()),
        })
    }

    /// Compile a Typst source string into a single-page document.
    fn compile(&self, src: &str) -> Result<PagedDocument, CandyError> {
        let source = TypstSource::detached(src.to_string());
        let world = CandyWorld {
            state: &self.state,
            main: source,
        };
        let warned = typst::compile::<PagedDocument>(&world);
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
            let src = format!(
                "#set page(width: 1pt, height: 1pt, margin: 0pt, fill: {bg})\n#rect()"
            );
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
                vec![Self::hex_digit(r), Self::hex_digit(g), Self::hex_digit(b), 255u8]
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

        for (_sid, (pw, ph), labels) in &by_scene {
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
                    "\n#block(width: auto, fill: rgb(\"{color}\"))[#{{ {body} }}]"
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
            let page = match doc.pages().first() {
                Some(p) => p,
                None => continue,
            };
            let svg = typst_svg::svg(page, &SvgOptions::default());
            // Read each object's natural top-left back from its colour box.
            for (label, color) in &palette {
                let Some(layout_bbox) = bbox_of_svg_with_fill(&svg, color) else {
                    continue;
                };
                // The natural position is exactly where plain Typst lays the
                // object's content box: the coloured `block` we wrap it in
                // shrinks to the body, so its fill footprint's top-left *is* the
                // body's native content-box top-left (`lx, ly`). At render time
                // the per-frame `#place(top + left, …)` aligns the body's content
                // box to this anchor, and the body's ink follows its own intrinsic
                // offset — the same offset native Typst applies. So placing at
                // `(lx, ly)` reproduces native Typst positioning exactly. Using
                // `lx - ox` would instead shift every object by its ink offset
                // (left/up for text), which is the positioning anomaly.
                let (lx, ly, _, _) = layout_bbox;
                nat.insert(label.clone(), (lx, ly));
            }
        }

        self.nat = nat;

        // Build per-scene canvas sizes + label→scene ownership for auto-hide.
        // When `scenes` is empty (legacy single-scene document) we fall back to
        // the whole document as one scene (id 0) — behavior identical to v0.1.
        self.label_scene = self.scene.label_scene_map();
        let mut sp: HashMap<usize, (f64, f64)> = HashMap::new();
        if self.scene.scenes.is_empty() {
            sp.insert(0, (self.page_w, self.page_h));
        } else {
            for s in &self.scene.scenes {
                sp.insert(s.id, self.scene.effective_page_pt(s.id));
            }
        }
        self.scene_pages = sp;

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
            // Parent auto-hide: skip mobjects not owned by the active scene.
            if !self.scene.scenes.is_empty()
                && self.label_scene.get(*label).copied().unwrap_or(0) != active
            {
                continue;
            }
            let st = states.get(*label).unwrap();
            let frame = self.render_object_pixels(*label, st, time_ms, pw, ph, pixel_per_pt)?;
            objs.push((st.opacity, frame));
        }

        // Subtitle overlays on top of the objects.
        for sub in &self.scene.subtitles {
            if self
                .scene
                .visible_subtitle_ids_at(time_ms)
                .contains(&sub.id)
            {
                let frame = self.render_subtitle_pixels(sub, time_ms, pixel_per_pt)?;
                objs.push((1.0, frame));
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
        // Apply the global camera (pan + zoom + rotate) by warping the
        // composited canvas through the inverse camera transform.
        if let Some(cam) = &camera {
            warp_canvas_with_camera(&mut canvas, w, h, cam, pw, ph, pixel_per_pt, bg_rgba);
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

    /// GPU-accelerated variant of [`render_frame_pixels`](Self::render_frame_pixels).
    ///
    /// Available only when the `gpu` cargo feature is enabled. Renders the
    /// frame to SVG (same as `render_frame_at`, with per-object opacity
    /// already applied via `<g opacity>` wrappers), then rasterizes the SVG on
    /// the GPU via vello + wgpu. The result is identical to the CPU path
    /// (modulo GPU rasterization differences like anti-aliasing quality), so
    /// the downstream video encoder consumes it unchanged.
    ///
    /// Pass a reusable [`crate::renderer::gpu::GpuRenderer`] — constructing a
    /// wgpu device is expensive, so it should be created once and reused
    /// across every frame in the animation.
    #[cfg(feature = "gpu")]
    pub fn render_frame_pixels_gpu(
        &mut self,
        time_ms: u32,
        all_frames: &[FrameData],
        pixel_per_pt: f32,
        gpu: &mut crate::renderer::gpu::GpuRenderer,
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

        // Deterministic z-order (same as the video path), following source
        // declaration order so并列 mobjects paint in the order they were written.
        let mut labels: Vec<&Label> = states.keys().collect();
        let order = self.draw_order_index();
        labels.sort_by(|a, b| order.get(*a).cmp(&order.get(*b)).then(a.0.cmp(&b.0)));

        // Resolve the active scene + its canvas. Only the active scene's
        // mobjects are rendered; a parent scene is auto-hidden while a child
        // scene is active.
        let active = if self.scene.scenes.is_empty() {
            0
        } else {
            self.scene.active_scene_at(time_ms)
        };
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

        // A present camera wraps the whole scene (objects + subtitles) in a
        // single global transform group. The white background stays fixed.
        if let Some(cam) = &camera {
            out.push_str(&format!(
                "<g transform=\"{}\">\n",
                camera_transform_svg(cam, pw, ph)
            ));
        }

        for label in labels {
            // Parent auto-hide: skip mobjects not owned by the active scene.
            if !self.scene.scenes.is_empty()
                && self.label_scene.get(label).copied().unwrap_or(0) != active
            {
                continue;
            }
            let st = &states[label];
            let obj_svg = self.render_object_svg(label, st, time_ms, pw, ph)?;
            // Wrap each object's SVG in a group with the per-frame opacity.
            // SVG <g opacity> applies to all descendants (shapes + text).
            let op = st.opacity.clamp(0.0, 1.0);
            out.push_str(&format!("<g opacity=\"{op}\">\n{obj_svg}\n</g>\n"));
        }

        // Subtitle overlays: one per visible scope, subject to
        // parental shadowing + auto-destroy. Drawn on top of the objects.
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

        if camera.is_some() {
            out.push_str("</g>\n");
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
            "{pre}#set page(width: {w}pt, height: {h}pt, margin: 0pt, fill: none)\n#{body}\n",
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

/// Build the Typst source that places a single mobject body at `(x_cm, y_cm)`
/// from the top-left corner, scaled by `scale_pct`% and rotated by `rotation`
/// degrees (clockwise, around the object's top-left origin).
///
/// When `rotation == 0.0` the `rotate(..)` wrapper is omitted, keeping the
/// generated source minimal for the common case (and matching the v0.1 output
/// exactly, so existing SVG drafts are byte-identical when no rotation is
/// applied).
/// Build a Typst preamble that re-declares every `@preview`/package import
/// captured from the source `.tyx`, so the detached per-object compile snippets
/// (which would otherwise lose the binding) can reference package symbols used
/// inside mobject bodies.
fn imports_preamble(scene: &Scene) -> String {
    if scene.imports.is_empty() {
        String::new()
    } else {
        let mut s = String::new();
        for imp in &scene.imports {
            s.push_str(imp);
            s.push('\n');
        }
        s
    }
}

/// Resolve the Typst body for `label` at frame time `time_ms`.
///
/// A `transform` records content switches on `Scene.content_timeline` as
/// `(time_ms, new_body)` pairs. For a given frame we use the latest switch
/// whose `time_ms <= frame`, falling back to `items[label]` (the original
/// body) before any transform. This lets a single label render different
/// content before/after a `transform` without corrupting earlier slides.
fn content_for(scene: &Scene, label: &Label, time_ms: u32) -> String {
    let body = if let Some(timeline) = scene.content_timeline.get(label) {
        let mut chosen: Option<&String> = None;
        for (t, body) in timeline {
            if *t <= time_ms {
                chosen = Some(body);
            }
        }
        if let Some(b) = chosen {
            b.clone()
        } else {
            scene.items.get(label).cloned().unwrap_or_default()
        }
    } else {
        scene.items.get(label).cloned().unwrap_or_default()
    };
    // Substitute `ecval(name)` counter references with their integer value at
    // this frame (honoring shadowing + lifecycle).
    substitute_counters(scene, &body, time_ms)
}

/// Replace every `ecval("name")` (or `ecval(name)`) counter reference in `body`
/// with the integer value of counter `name` at `time_ms`, per the scene's scope
/// shadowing / lifecycle rules.
///
/// Expansion is **AST-driven**, not naive string replacement: `body` is parsed
/// into a Typst `SyntaxNode` tree and every *real* `ecval(..)` function-call
/// node is swapped for an integer literal. This keeps `ecval` a valid AST node
/// that composes like any other Typst expression (e.g. inside
/// `rect(width: ecval("n") * 1cm)`) and avoids rewriting substrings that merely
/// *look* like the call (inside strings, comments, …). The canonical call form
/// is `ecval("name")` (a quoted string); the bare-ident form `ecval(name)` is
/// also accepted for backwards compatibility with existing `.tyx` sources.
fn substitute_counters(scene: &Scene, body: &str, time_ms: u32) -> String {
    // Fast path: no counter read at all → short-circuit.
    if !body.contains("ecval") {
        return body.to_string();
    }
    // Parse as *code* (the body is a Typst expression, not a markup document),
    // so `ecval(..)` parses to a real `FuncCall` node whose source range maps
    // 1:1 onto `body`.
    let root = typst_syntax::parse_code(body);
    let node = LinkedNode::new(&root);

    // Collect (source range → replacement) for every `ecval(..)` call.
    let mut edits: Vec<(std::ops::Range<usize>, String)> = Vec::new();
    collect_ecval_edits(&node, scene, time_ms, &mut edits);
    // Drop any edit whose range is nested inside another (a nested `ecval`),
    // keeping the innermost node so we never clobber an already-replaced child.
    let drop: Vec<bool> = edits
        .iter()
        .map(|(r, _)| {
            edits
                .iter()
                .any(|(o, _)| o != r && o.start <= r.start && r.end <= o.end)
        })
        .collect();
    let mut kept: Vec<(std::ops::Range<usize>, String)> = Vec::new();
    for (keep, e) in drop.into_iter().zip(edits) {
        if !keep {
            kept.push(e);
        }
    }
    let mut edits = kept;
    if edits.is_empty() {
        return body.to_string();
    }
    // Apply right-to-left so earlier edits don't invalidate later offsets.
    edits.sort_by(|a, b| b.0.start.cmp(&a.0.start));
    let mut out = body.to_string();
    for (range, text) in edits {
        out.replace_range(range, &text);
    }
    out
}

/// Walk `node`, appending an edit that swaps each `ecval(name)` call for its
/// current integer value (only for counters actually declared in the scene).
fn collect_ecval_edits(
    node: &LinkedNode,
    scene: &Scene,
    time_ms: u32,
    edits: &mut Vec<(std::ops::Range<usize>, String)>,
) {
    if let Some(call) = node.get().cast::<ast::FuncCall>() {
        if let Some(name) = ecval_counter_name(&call) {
            // Only substitute declared counters, mirroring the previous
            // registry-based behaviour (an unrelated user `ecval` is left
            // untouched). Unknown counters still resolve to `seed`/0 below.
            if scene.counters.iter().any(|c| c.name == name) {
                let val = scene.counter_value_at(&name, time_ms).to_string();
                edits.push((node.range(), val));
            }
        }
    }
    for child in node.children() {
        collect_ecval_edits(&child, scene, time_ms, edits);
    }
}

/// If `call` is an `ecval(..)` read, return the counter name it references.
fn ecval_counter_name(call: &ast::FuncCall) -> Option<String> {
    let is_ecval = match call.callee() {
        Expr::Ident(id) => id.as_str() == "ecval",
        Expr::FieldAccess(fa) => fa.field().as_str() == "ecval",
        _ => false,
    };
    if !is_ecval {
        return None;
    }
    // The first positional argument is the counter name. A leading named
    // argument means this isn't the canonical read form → bail.
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

/// Inset (in cm) from the page edge for the named subtitle anchors.
const SUBTITLE_MARGIN_CM: f64 = 1.0;

/// Build the Typst `place(...)` expression that anchors a subtitle's body,
/// keeping the caption fully inside the viewport. Named anchors use
/// alignment (e.g. `bottom + center`) so the caption's box hugs the requested
/// edge instead of overflowing it — the old code placed the box's *top-left*
/// corner at the anchor, which pushed bottom/top captions off-screen.
fn subtitle_place_expr(sub: &Subtitle, margin: f64) -> String {
    match sub.position {
        SubPos::Absolute(x, y) => {
            // Anchor the box's top-left corner at the absolute (x, y) in cm.
            format!("place(top + left, dx: {x}cm, dy: {y}cm)")
        }
        SubPos::Bottom => format!("place(bottom + center, dy: -{margin}cm)"),
        SubPos::Top => format!("place(top + center, dy: {margin}cm)"),
        SubPos::Center => "place(center + center)".to_string(),
        SubPos::BottomLeft => {
            format!("place(bottom + left, dx: {margin}cm, dy: -{margin}cm)")
        }
        SubPos::BottomRight => {
            format!("place(bottom + right, dx: -{margin}cm, dy: -{margin}cm)")
        }
        SubPos::TopLeft => format!("place(top + left, dx: {margin}cm, dy: {margin}cm)"),
        SubPos::TopRight => {
            format!("place(top + right, dx: -{margin}cm, dy: {margin}cm)")
        }
    }
}

/// Compile a subtitle's body to a single-page Typst document, placed at the
/// subtitle's resolved anchor and with `ecval(...)` counters substituted.
fn subtitle_doc(
    scene: &Scene,
    sub: &Subtitle,
    page_w: f64,
    page_h: f64,
    time_ms: u32,
) -> Result<PagedDocument, CandyError> {
    let body = substitute_counters(scene, &sub.body, time_ms);
    let preamble = imports_preamble(scene);
    let pre = if preamble.is_empty() {
        String::new()
    } else {
        format!("{preamble}\n")
    };
    let place = subtitle_place_expr(sub, SUBTITLE_MARGIN_CM);
    let src = format!(
        "{pre}#set page(width: {pw}pt, height: {ph}pt, margin: 0pt, fill: none)\n\
         #{place}[ #{body} ]\n",
        pw = page_w,
        ph = page_h,
    );
    let state = WorldState::new(std::path::PathBuf::new());
    let source = TypstSource::detached(src);
    let world = CandyWorld {
        state: &state,
        main: source,
    };
    let warned = typst::compile::<PagedDocument>(&world);
    warned
        .output
        .map_err(|errs| CandyError::Typst(format!("{:?}", errs)))
}

/// Render a subtitle to an SVG string (used by the SVG frame path).
fn render_subtitle_svg_impl(
    scene: &Scene,
    sub: &Subtitle,
    page_w: f64,
    page_h: f64,
    time_ms: u32,
) -> Result<String, CandyError> {
    let doc = subtitle_doc(scene, sub, page_w, page_h, time_ms)?;
    let page = doc
        .pages()
        .first()
        .ok_or_else(|| CandyError::Typst("document produced no pages".into()))?;
    Ok(typst_svg::svg(page, &SvgOptions::default()))
}

/// Translate a ring so its bounding-box top-left sits at the origin. Morph
/// outlines are interpolated in this local frame and later placed (via
/// `place_source`) at the target mobject's natural top-left, so the morph is
/// anchored correctly and matches standard Typst positioning at `t = 1`.
fn localize_ring(ring: Vec<[f64; 2]>) -> Vec<[f64; 2]> {
    if ring.is_empty() {
        return ring;
    }
    let mut min_x = f64::INFINITY;
    let mut min_y = f64::INFINITY;
    for p in &ring {
        if p[0] < min_x {
            min_x = p[0];
        }
        if p[1] < min_y {
            min_y = p[1];
        }
    }
    ring.into_iter()
        .map(|p| [p[0] - min_x, p[1] - min_y])
        .collect()
}

/// Convert an SVG paint color string (as captured from a Typst-rendered SVG,
/// which uses hex like `#0074d9` or `rgb(...)`) into a Typst color expression
/// that is safe to embed in code mode (e.g. inside `polygon(fill: …, …)`).
///
/// Typst's `#` is a code-mode marker, so a raw `#0074d9` would be a syntax
/// error — we wrap hex colors as `rgb("#0074d9")`, which Typst accepts.
fn svg_color_to_typst(color: &str) -> String {
    let c = color.trim();
    if let Some(hex) = c.strip_prefix('#') {
        // `#rrggbb` / `#rrggbbaa` → rgb("#…")
        format!("rgb(\"#{hex}\")")
    } else if c.starts_with("rgb(") || c.starts_with("rgba(") || c.starts_with("hsl(") {
        // Already a valid Typst color expression.
        c.to_string()
    } else {
        // Named color (`red`, `blue`, …) or anything else — pass through.
        c.to_string()
    }
}

/// Build a Typst `polygon(...)` body (no leading `#`) from a ring, preserving
/// the target shape's paint. Points are emitted as absolute `(x*pt, y*pt)`.
fn polygon_svg(ring: &[[f64; 2]], fill: &Option<String>, stroke: &Option<String>) -> String {
    let pts: Vec<String> = ring
        .iter()
        .map(|p| format!("({:.2}pt, {:.2}pt)", p[0], p[1]))
        .collect();
    let fill = svg_color_to_typst(fill.clone().unwrap_or_else(|| "black".to_string()).as_str());
    let stroke_attr = match stroke {
        Some(s) => format!(", stroke: {}", svg_color_to_typst(s)),
        None => String::new(),
    };
    format!(
        "polygon(fill: {fill}{stroke_attr}, {pts})",
        pts = pts.join(", ")
    )
}

fn place_source(
    page_w: f64,
    page_h: f64,
    x_cm: f64,
    y_cm: f64,
    scale_pct: f64,
    rotation: f64,
    body: &str,
    preamble: &str,
) -> String {
    // The body is a raw Typst expression (e.g. "rect(width: 2cm, fill: red)")
    // captured from the .tyx source. Inside a content block `[...]`, function
    // calls MUST be prefixed with `#` — otherwise Typst treats them as plain
    // text. We add the `#` here so the body renders as an object, not text.
    let pre = if preamble.is_empty() {
        String::new()
    } else {
        format!("{preamble}\n")
    };
    if rotation.abs() < 1e-9 {
        format!(
            "{pre}#set page(width: {page_w}pt, height: {page_h}pt, margin: 0pt, fill: none)\n\
             #place(top + left, dx: {x_cm}cm, dy: {y_cm}cm)[#scale(origin: top + left, {scale_pct}%)[#{body}]]\n"
        )
    } else {
        format!(
            "{pre}#set page(width: {page_w}pt, height: {page_h}pt, margin: 0pt, fill: none)\n\
             #place(top + left, dx: {x_cm}cm, dy: {y_cm}cm)[#scale(origin: top + left, {scale_pct}%)[#rotate(origin: top + left, {rotation}deg)[#{body}]]]\n"
        )
    }
}

/// Composite a (possibly transparent) source frame over an opaque destination
/// canvas using the "over" operator, scaled by `opacity`.
fn composite_over(
    dst: &mut [u8],
    src: &crate::renderer::RenderedFrame,
    opacity: f64,
    w: usize,
    h: usize,
) {
    let op = opacity.clamp(0.0, 1.0);
    for y in 0..h.min(src.height) {
        for x in 0..w.min(src.width) {
            let di = (y * w + x) * 4;
            let si = (y * src.width + x) * 4;
            let sa = (src.rgba[si + 3] as f32 / 255.0) * op as f32;
            if sa <= 0.0 {
                continue;
            }
            for c in 0..3 {
                let s = src.rgba[si + c] as f32;
                let d = dst[di + c] as f32;
                dst[di + c] = (s * sa + d * (1.0 - sa)).round() as u8;
            }
            dst[di + 3] = 255;
        }
    }
}

/// Synthetic label used by `#camera` (a global pan/zoom/rotate transform).
/// Never rendered as an object — the renderer reads its per-frame state and
/// applies it as a wrapping transform over the whole scene.
pub(crate) const CAMERA_LABEL: &str = "__camera__";

/// SVG `<g transform>` attribute for the camera (pan + zoom + rotate about the
/// page center), in Typst points.
fn camera_transform_svg(cam: &FrameData, page_w: f64, page_h: f64) -> String {
    let (cx, cy) = (page_w / 2.0, page_h / 2.0);
    let ncx = -cx;
    let ncy = -cy;
    let dx = cam.x * PT_PER_CM;
    let dy = cam.y * PT_PER_CM;
    let s = cam.scale;
    let r = cam.rotation;
    format!(
        "translate({cx} {cy}) rotate({r}) scale({s}) translate({ncx} {ncy}) translate({dx} {dy})"
    )
}

/// Forward camera matrix (scene → screen) in *pixel* space, for the pixel-path
/// warp. `ppi` is `pixel_per_pt`.
fn camera_matrix_px(cam: &FrameData, page_w_pt: f64, page_h_pt: f64, ppi: f32) -> Matrix {
    let (cx, cy) = (page_w_pt * ppi as f64 / 2.0, page_h_pt * ppi as f64 / 2.0);
    let dx = cam.x * PT_PER_CM * ppi as f64;
    let dy = cam.y * PT_PER_CM * ppi as f64;
    let s = cam.scale;
    let r = cam.rotation;
    compose(
        compose(
            compose(
                compose(Matrix::translation(cx, cy), Matrix::rotation(r)),
                Matrix::scaling(s),
            ),
            Matrix::translation(-cx, -cy),
        ),
        Matrix::translation(dx, dy),
    )
}

/// Bilinear-sample a RGBA canvas at `(x, y)` (in pixels). Out-of-bounds samples
/// return the scene background colour `bg` — the same fill native Typst paints
/// on the page, so a camera pan/zoom/rotate that exposes area outside the
/// original canvas reveals the configured background (e.g. a dark night sky),
/// not a hardcoded white edge.
fn sample_bilinear(
    src: &[u8],
    w: usize,
    h: usize,
    x: f64,
    y: f64,
    bg: [u8; 4],
) -> (u8, u8, u8, u8) {
    if x < 0.0 || y < 0.0 || x > w as f64 - 1.0 || y > h as f64 - 1.0 {
        return (bg[0], bg[1], bg[2], bg[3]);
    }
    let x0 = x.floor() as usize;
    let y0 = y.floor() as usize;
    let x1 = (x0 + 1).min(w - 1);
    let y1 = (y0 + 1).min(h - 1);
    let fx = x - x0 as f64;
    let fy = y - y0 as f64;
    let idx = |x: usize, y: usize| -> [u8; 4] {
        let i = (y * w + x) * 4;
        [src[i], src[i + 1], src[i + 2], src[i + 3]]
    };
    let p00 = idx(x0, y0);
    let p10 = idx(x1, y0);
    let p01 = idx(x0, y1);
    let p11 = idx(x1, y1);
    let lerp = |a: u8, b: u8, t: f64| (a as f64 + (b as f64 - a as f64) * t).round() as u8;
    let top = [
        lerp(p00[0], p10[0], fx),
        lerp(p00[1], p10[1], fx),
        lerp(p00[2], p10[2], fx),
        lerp(p00[3], p10[3], fx),
    ];
    let bot = [
        lerp(p01[0], p11[0], fx),
        lerp(p01[1], p11[1], fx),
        lerp(p01[2], p11[2], fx),
        lerp(p01[3], p11[3], fx),
    ];
    (
        lerp(top[0], bot[0], fy),
        lerp(top[1], bot[1], fy),
        lerp(top[2], bot[2], fy),
        lerp(top[3], bot[3], fy),
    )
}

/// Warp a fully-composited canvas through the inverse camera transform,
/// sampling the source with bilinear filtering. `bg` is the scene background
/// colour used for samples that fall outside the original canvas (so exposed
/// margins match native Typst's page fill instead of hardcoded white).
fn warp_canvas_with_camera(
    canvas: &mut [u8],
    w: usize,
    h: usize,
    cam: &FrameData,
    page_w_pt: f64,
    page_h_pt: f64,
    ppi: f32,
    bg: [u8; 4],
) {
    let m = camera_matrix_px(cam, page_w_pt, page_h_pt, ppi);
    let inv = m.inverse();
    let src = canvas.to_vec();
    for y in 0..h {
        for x in 0..w {
            let (sx, sy) = inv.apply(x as f64, y as f64);
            let (r, g, b, a) = sample_bilinear(&src, w, h, sx, sy, bg);
            let di = (y * w + x) * 4;
            canvas[di] = r;
            canvas[di + 1] = g;
            canvas[di + 2] = b;
            canvas[di + 3] = a;
        }
    }
}

/// A 2-D affine matrix `x' = a*x + c*y + e`, `y' = b*x + d*y + f`.
#[derive(Clone, Copy)]
struct Matrix {
    a: f64,
    b: f64,
    c: f64,
    d: f64,
    e: f64,
    f: f64,
}

impl Matrix {
    /// Translation matrix (cm/pt units, same space as the rest of the pipeline).
    fn translation(x: f64, y: f64) -> Self {
        Matrix {
            a: 1.0,
            b: 0.0,
            c: 0.0,
            d: 1.0,
            e: x,
            f: y,
        }
    }

    /// Uniform scale matrix.
    fn scaling(s: f64) -> Self {
        Matrix {
            a: s,
            b: 0.0,
            c: 0.0,
            d: s,
            e: 0.0,
            f: 0.0,
        }
    }

    /// Rotation matrix, `deg` degrees clockwise (Typst convention; +y down).
    fn rotation(deg: f64) -> Self {
        let r = deg.to_radians();
        let (s, c) = (r.sin(), r.cos());
        Matrix {
            a: c,
            b: s,
            c: -s,
            d: c,
            e: 0.0,
            f: 0.0,
        }
    }

    /// Apply the affine to a point `(x, y)`, returning the mapped point.
    fn apply(&self, x: f64, y: f64) -> (f64, f64) {
        (
            self.a * x + self.c * y + self.e,
            self.b * x + self.d * y + self.f,
        )
    }

    /// Inverse affine. Panics only on a degenerate (zero-determinant) matrix,
    /// which the camera never produces (zoom is clamped to `> 0`).
    fn inverse(&self) -> Matrix {
        let det = self.a * self.d - self.b * self.c;
        let inv = 1.0 / det;
        Matrix {
            a: self.d * inv,
            b: -self.b * inv,
            c: -self.c * inv,
            d: self.a * inv,
            e: (self.c * self.f - self.d * self.e) * inv,
            f: (self.b * self.e - self.a * self.f) * inv,
        }
    }
}

/// Compose `a` after `b` (apply `b` first, then `a`).
fn compose(a: Matrix, b: Matrix) -> Matrix {
    Matrix {
        a: a.a * b.a + a.c * b.b,
        b: a.b * b.a + a.d * b.b,
        c: a.a * b.c + a.c * b.d,
        d: a.b * b.c + a.d * b.d,
        e: a.a * b.e + a.c * b.f + a.e,
        f: a.b * b.e + a.d * b.f + a.f,
    }
}

/// Union only the geometry that carries the given `fill` colour
/// (case-insensitive, `#rrggbb`). Used by the native layout pass: each mobject
/// is wrapped in a uniquely-coloured `box`, so locating that colour's footprint
/// recovers the object's natural top-left as laid out by Typst itself — no
/// hand-computed coordinates.
fn bbox_of_svg_with_fill(svg: &str, fill: &str) -> Option<(f64, f64, f64, f64)> {
    let target = fill.to_ascii_lowercase();
    let mut fill_stack: Vec<String> = Vec::new();
    let mut cur_fill = String::new();
    let mut stack: Vec<[f64; 6]> = Vec::new();
    let mut cur: [f64; 6] = [1.0, 0.0, 0.0, 1.0, 0.0, 0.0];
    let mut min_x = f64::INFINITY;
    let mut min_y = f64::INFINITY;
    let mut max_x = f64::NEG_INFINITY;
    let mut max_y = f64::NEG_INFINITY;

    let mut idx = 0;
    while idx < svg.len() {
        let Some(lt) = svg[idx..].find('<') else {
            break;
        };
        let lt = idx + lt;
        if svg[lt..].starts_with("</g>") {
            if let Some(m) = stack.pop() {
                cur = m;
            }
            if let Some(f) = fill_stack.pop() {
                cur_fill = f;
            }
            idx = lt + 4;
            continue;
        }
        let Some(gt) = svg[lt..].find('>') else {
            break;
        };
        let gt = lt + gt;
        let tag = &svg[lt + 1..gt];
        let is_g_open = tag == "g" || tag.starts_with("g ") || tag.starts_with("g>");
        let mut el_matrix = cur;
        if let Some(t) = svg_attr(tag, "transform") {
            el_matrix = compose_matrix(cur, &parse_transform_attr(&t));
        }
        // Effective fill for this element: an explicit `fill` attr wins,
        // otherwise it inherits from the nearest ancestor group.
        let mut el_fill = cur_fill.clone();
        if let Some(f) = svg_attr(tag, "fill") {
            el_fill = f.to_ascii_lowercase();
        }
        if is_g_open {
            stack.push(cur);
            fill_stack.push(cur_fill.clone());
            cur = el_matrix;
            cur_fill = el_fill;
            idx = gt + 1;
            continue;
        }
        if el_fill == target {
            let pts: Vec<(f64, f64)> = match tag.split_whitespace().next() {
                Some("rect") => {
                    let (x, y) = (svg_num(tag, "x"), svg_num(tag, "y"));
                    let (w, h) = (svg_num(tag, "width"), svg_num(tag, "height"));
                    vec![(x, y), (x + w, y), (x + w, y + h), (x, y + h)]
                }
                Some("circle") => {
                    let (cx, cy, r) = (svg_num(tag, "cx"), svg_num(tag, "cy"), svg_num(tag, "r"));
                    vec![(cx - r, cy - r), (cx + r, cy + r)]
                }
                Some("ellipse") => {
                    let (cx, cy) = (svg_num(tag, "cx"), svg_num(tag, "cy"));
                    let (rx, ry) = (svg_num(tag, "rx"), svg_num(tag, "ry"));
                    vec![(cx - rx, cy - ry), (cx + rx, cy + ry)]
                }
                Some("polygon") | Some("polyline") => svg_points(svg_attr(tag, "points")),
                Some("path") => match svg_attr(tag, "d") {
                    Some(d) => collect_path_points(&d),
                    None => vec![],
                },
                _ => vec![],
            };
            for (x, y) in pts {
                let (px, py) = apply_matrix(&el_matrix, x, y);
                if px < min_x {
                    min_x = px;
                }
                if py < min_y {
                    min_y = py;
                }
                if px > max_x {
                    max_x = px;
                }
                if py > max_y {
                    max_y = py;
                }
            }
        }
        idx = gt + 1;
    }
    if min_x.is_finite() {
        Some((min_x, min_y, max_x, max_y))
    } else {
        None
    }
}

/// Extract `name="value"` (single or double quoted) from a tag string.
fn svg_attr(tag: &str, name: &str) -> Option<String> {
    let pat = format!("{name}=");
    let i = tag.find(&pat)? + pat.len();
    let b = tag.as_bytes().get(i)?;
    if *b != b'"' && *b != b'\'' {
        return None;
    }
    let q = *b as char;
    let start = i + 1;
    let end = start + tag[start..].find(q)?;
    Some(tag[start..end].to_string())
}

fn svg_num(tag: &str, name: &str) -> f64 {
    svg_attr(tag, name)
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(0.0)
}

/// Parse a `points="x1,y1 x2,y2 ..."` attribute into coordinate pairs.
fn svg_points(s: Option<String>) -> Vec<(f64, f64)> {
    let Some(s) = s else {
        return vec![];
    };
    s.split(|c: char| c == ',' || c.is_whitespace())
        .filter(|t| !t.is_empty())
        .filter_map(|t| t.trim().parse::<f64>().ok())
        .collect::<Vec<_>>()
        .chunks(2)
        .filter_map(|c| {
            if c.len() == 2 {
                Some((c[0], c[1]))
            } else {
                None
            }
        })
        .collect()
}

/// Loose extent of an SVG path: every coordinate pair in `d` (control points
/// included). Good enough for layout spacing.
/// A token in an SVG path `d` string: a command letter or a numeric argument.
enum PathTok {
    Cmd(char),
    Num(f64),
}

/// Pull the next numeric argument, skipping any interleaved command letters
/// (which belong to a later group). Returns `None` at end-of-input or when the
/// next token is a command (so the caller can stop consuming this group).
fn next_path_num(toks: &[PathTok], i: &mut usize) -> Option<f64> {
    while *i < toks.len() {
        match toks[*i] {
            PathTok::Num(v) => {
                *i += 1;
                return Some(v);
            }
            PathTok::Cmd(_) => return None,
        }
    }
    None
}

/// Parse an SVG path `d` attribute into the set of points that bound it.
///
/// Unlike a naive "pair up all numbers" scheme, this honours command letters,
/// relative (lowercase) vs absolute (uppercase) coordinates, the single-axis
/// `h`/`v` commands, and implicit command repetition (e.g. `M 0 0 1 1` draws a
/// move followed by a line). Bézier control points are included so the returned
/// hull bounds the whole curve (a Bézier lies inside its control-point convex
/// hull). Previously this function just zipped every number into `(x, y)` pairs,
/// which silently transposed `v`/`h` rects and broke any non-square path.
fn collect_path_points(d: &str) -> Vec<(f64, f64)> {
    // Tokenize: command letters vs numbers (scientific notation is allowed).
    let mut toks: Vec<PathTok> = Vec::new();
    let mut num = String::new();
    let flush = |num: &mut String, toks: &mut Vec<PathTok>| {
        if !num.is_empty() {
            if let Ok(v) = num.parse::<f64>() {
                toks.push(PathTok::Num(v));
            }
            num.clear();
        }
    };
    for c in d.chars() {
        if matches!(
            c,
            'M' | 'm'
                | 'L'
                | 'l'
                | 'H'
                | 'h'
                | 'V'
                | 'v'
                | 'C'
                | 'c'
                | 'S'
                | 's'
                | 'Q'
                | 'q'
                | 'T'
                | 't'
                | 'A'
                | 'a'
                | 'Z'
                | 'z'
        ) {
            flush(&mut num, &mut toks);
            toks.push(PathTok::Cmd(c));
        } else if c.is_ascii_digit() || c == '.' || c == '-' || c == '+' || c == 'e' || c == 'E' {
            // `e`/`E` only appear inside scientific-notation numbers, so they are
            // part of a numeric token rather than a (non-existent) command.
            num.push(c);
        } else {
            flush(&mut num, &mut toks);
        }
    }
    flush(&mut num, &mut toks);

    let mut pts: Vec<(f64, f64)> = Vec::new();
    let mut cx = 0.0;
    let mut cy = 0.0;
    let mut sx = 0.0; // current subpath start (for `Z`)
    let mut sy = 0.0;
    let mut cmd: Option<char> = None;
    let mut first = true; // first argument group of the current command run
    let mut i = 0;
    while i < toks.len() {
        if let PathTok::Cmd(c) = toks[i] {
            cmd = Some(c);
            first = true;
            i += 1;
        }
        let base = match cmd {
            Some(c) => c,
            None => {
                i += 1;
                continue;
            }
        };
        let rel = base.is_lowercase();
        // A `M`/`m` run emits move then implicit lineto for the rest of the group.
        let eff = if first {
            base
        } else {
            match base {
                'M' => 'L',
                'm' => 'l',
                o => o,
            }
        };
        match eff {
            'Z' | 'z' => {
                cx = sx;
                cy = sy;
                pts.push((cx, cy));
                first = false;
            }
            'H' | 'h' => {
                let x = match next_path_num(&toks, &mut i) {
                    Some(v) => v,
                    None => break,
                };
                cx = if rel { cx + x } else { x };
                pts.push((cx, cy));
                first = false;
            }
            'V' | 'v' => {
                let y = match next_path_num(&toks, &mut i) {
                    Some(v) => v,
                    None => break,
                };
                cy = if rel { cy + y } else { y };
                pts.push((cx, cy));
                first = false;
            }
            'L' | 'l' | 'M' | 'm' | 'T' | 't' => {
                let x = match next_path_num(&toks, &mut i) {
                    Some(v) => v,
                    None => break,
                };
                let y = match next_path_num(&toks, &mut i) {
                    Some(v) => v,
                    None => break,
                };
                let (nx, ny) = if rel { (cx + x, cy + y) } else { (x, y) };
                cx = nx;
                cy = ny;
                if eff == 'M' || eff == 'm' {
                    sx = cx;
                    sy = cy;
                }
                pts.push((cx, cy));
                first = false;
            }
            'Q' | 'q' => {
                let x1 = next_path_num(&toks, &mut i);
                let y1 = next_path_num(&toks, &mut i);
                let x = match next_path_num(&toks, &mut i) {
                    Some(v) => v,
                    None => break,
                };
                let y = match next_path_num(&toks, &mut i) {
                    Some(v) => v,
                    None => break,
                };
                let (cx1, cy1) = if rel {
                    (cx + x1.unwrap_or(0.0), cy + y1.unwrap_or(0.0))
                } else {
                    (x1.unwrap_or(0.0), y1.unwrap_or(0.0))
                };
                let (nx, ny) = if rel { (cx + x, cy + y) } else { (x, y) };
                pts.push((cx1, cy1));
                pts.push((nx, ny));
                cx = nx;
                cy = ny;
                first = false;
            }
            'S' | 's' => {
                let x2 = next_path_num(&toks, &mut i);
                let y2 = next_path_num(&toks, &mut i);
                let x = match next_path_num(&toks, &mut i) {
                    Some(v) => v,
                    None => break,
                };
                let y = match next_path_num(&toks, &mut i) {
                    Some(v) => v,
                    None => break,
                };
                let (cx2, cy2) = if rel {
                    (cx + x2.unwrap_or(0.0), cy + y2.unwrap_or(0.0))
                } else {
                    (x2.unwrap_or(0.0), y2.unwrap_or(0.0))
                };
                let (nx, ny) = if rel { (cx + x, cy + y) } else { (x, y) };
                pts.push((cx2, cy2));
                pts.push((nx, ny));
                cx = nx;
                cy = ny;
                first = false;
            }
            'C' | 'c' => {
                let x1 = next_path_num(&toks, &mut i);
                let y1 = next_path_num(&toks, &mut i);
                let x2 = next_path_num(&toks, &mut i);
                let y2 = next_path_num(&toks, &mut i);
                let x = match next_path_num(&toks, &mut i) {
                    Some(v) => v,
                    None => break,
                };
                let y = match next_path_num(&toks, &mut i) {
                    Some(v) => v,
                    None => break,
                };
                let (cx1, cy1) = if rel {
                    (cx + x1.unwrap_or(0.0), cy + y1.unwrap_or(0.0))
                } else {
                    (x1.unwrap_or(0.0), y1.unwrap_or(0.0))
                };
                let (cx2, cy2) = if rel {
                    (cx + x2.unwrap_or(0.0), cy + y2.unwrap_or(0.0))
                } else {
                    (x2.unwrap_or(0.0), y2.unwrap_or(0.0))
                };
                let (nx, ny) = if rel { (cx + x, cy + y) } else { (x, y) };
                pts.push((cx1, cy1));
                pts.push((cx2, cy2));
                pts.push((nx, ny));
                cx = nx;
                cy = ny;
                first = false;
            }
            'A' | 'a' => {
                // rx ry x-axis-rotation large-arc-flag sweep-flag x y
                let _rx = next_path_num(&toks, &mut i);
                let _ry = next_path_num(&toks, &mut i);
                let _rot = next_path_num(&toks, &mut i);
                let _la = next_path_num(&toks, &mut i);
                let _sw = next_path_num(&toks, &mut i);
                let x = match next_path_num(&toks, &mut i) {
                    Some(v) => v,
                    None => break,
                };
                let y = match next_path_num(&toks, &mut i) {
                    Some(v) => v,
                    None => break,
                };
                let (nx, ny) = if rel { (cx + x, cy + y) } else { (x, y) };
                pts.push((nx, ny));
                cx = nx;
                cy = ny;
                first = false;
            }
            _ => {
                // Unknown command: consume one number and move on.
                next_path_num(&toks, &mut i);
                first = false;
            }
        }
    }
    pts
}

/// Apply a 2-D affine `[a, b, c, d, e, f]` to a point.
fn apply_matrix(m: &[f64; 6], x: f64, y: f64) -> (f64, f64) {
    (m[0] * x + m[2] * y + m[4], m[1] * x + m[3] * y + m[5])
}

/// Compose two affines so that `result` applies `b` then `a` (SVG `a b` order).
fn compose_matrix(a: [f64; 6], b: &[f64; 6]) -> [f64; 6] {
    [
        a[0] * b[0] + a[2] * b[1],
        a[1] * b[0] + a[3] * b[1],
        a[0] * b[2] + a[2] * b[3],
        a[1] * b[2] + a[3] * b[3],
        a[0] * b[4] + a[2] * b[5] + a[4],
        a[1] * b[4] + a[3] * b[5] + a[5],
    ]
}

/// Parse a `transform` attribute (`translate` / `scale` / `rotate` / `matrix`).
fn parse_transform_attr(s: &str) -> [f64; 6] {
    let mut m = [1.0, 0.0, 0.0, 1.0, 0.0, 0.0];
    let mut rest = s;
    while let Some(open) = rest.find('(') {
        let Some(close) = rest[open..].find(')') else {
            break;
        };
        let close = open + close;
        let name_start = rest[..open]
            .rfind(|c: char| !(c.is_alphabetic() || c == '-'))
            .map(|i| i + 1)
            .unwrap_or(0);
        let name = &rest[name_start..open];
        let args: Vec<f64> = rest[open + 1..close]
            .split(|c: char| {
                !(c.is_ascii_digit() || c == '.' || c == '-' || c == '+' || c == 'e' || c == 'E')
            })
            .filter(|t| !t.is_empty())
            .filter_map(|t| t.parse::<f64>().ok())
            .collect();
        let tm = match name {
            "translate" if args.len() >= 2 => [1.0, 0.0, 0.0, 1.0, args[0], args[1]],
            "translate" => [
                1.0,
                0.0,
                0.0,
                1.0,
                args.first().copied().unwrap_or(0.0),
                0.0,
            ],
            "scale" if args.len() >= 2 => [args[0], 0.0, 0.0, args[1], 0.0, 0.0],
            "scale" => {
                let s = args.first().copied().unwrap_or(1.0);
                [s, 0.0, 0.0, s, 0.0, 0.0]
            }
            "rotate" if args.len() >= 1 => {
                let r = args[0].to_radians();
                let (s, c) = (r.sin(), r.cos());
                [c, s, -s, c, 0.0, 0.0]
            }
            "matrix" if args.len() >= 6 => [args[0], args[1], args[2], args[3], args[4], args[5]],
            _ => [1.0, 0.0, 0.0, 1.0, 0.0, 0.0],
        };
        m = compose_matrix(m, &tm);
        rest = &rest[close + 1..];
    }
    m
}

/// Test helper: compile a `.typ` *file* (not a detached string) to SVG.
///
/// This resolves relative `#import "x.typ"` statements against the file's
/// directory — required to verify the split `lib.typ` entrypoint, which pulls
/// its directives from sibling submodules.
#[cfg(test)]
pub(crate) fn compile_file_for_test(path: &Path) -> Result<String, CandyError> {
    let dir = path.parent().unwrap_or_else(|| Path::new("")).to_path_buf();
    let vpath =
        VirtualPath::virtualize(&dir, path).expect("test file must sit under the project root");
    let id = FileId::new(RootedPath::new(VirtualRoot::Project, vpath));
    let state = WorldState::new(dir);
    let text = std::fs::read_to_string(path)?; // E001 on missing file
    let source = TypstSource::new(id, text);
    let world = CandyWorld {
        state: &state,
        main: source,
    };
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
    assert!(y("zeta") < y("alpha"), "order scrambled: zeta must sit above alpha");
    assert!(y("alpha") < y("mid"), "order scrambled: alpha must sit above mid");
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
        groups: HashMap::new(),
        private_metadata: PrivateMeta::default(),
    };

    let mut r = Renderer::with_root(scene, PathBuf::new()).unwrap();
    r.ensure_natural_public().unwrap();

    let page_w = 16.0 * PT_PER_CM;
    let page_h = 9.0 * PT_PER_CM;
    let native = native_natural_positions(&r, &ordered, page_w, page_h);

    // (1) The hidden mobject MUST have a natural position (it was not skipped).
    let hidden = r.nat_for(&Label("hidden".into())).expect("hidden mobject must get a nat");
    // (2) Its natural position must match where it would sit if shown (native
    //     Typst, all-visible) — i.e. `#hide` reserved the same box.
    let nat_hidden = native.get(&Label("hidden".into())).expect("native nat present");
    assert!(
        (hidden.0 - nat_hidden.0).abs() < 1.0 && (hidden.1 - nat_hidden.1).abs() < 1.0,
        "hidden: candy nat {hidden:?} != native {nat_hidden:?} (space not reserved)"
    );
    // (3) It must keep its slot in the flow: below `top`, above `bottom`.
    let y = |l: &str| r.nat_for(&Label(l.into())).unwrap().1;
    assert!(y("top") < y("hidden"), "hidden must sit below top");
    assert!(y("hidden") < y("bottom"), "hidden must reserve space above bottom");
}
