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
use std::path::PathBuf;
use std::sync::Arc;

use typst::{Library, LibraryExt, World};
use typst_kit::files::{FileStore, FsRoot, SystemFiles};
use typst_kit::fonts::FontStore;
use typst_kit::packages::SystemPackages;
use typst_layout::PagedDocument;
use typst_library::diag::FileError;
use typst_library::foundations::{Bytes, Datetime, Duration};
use typst_library::text::Font;
use typst_render::{RenderOptions, render};
use typst_svg::SvgOptions;
use typst_syntax::{FileId, Source as TypstSource};
use typst_utils::{LazyHash, Scalar};

use crate::core::ast::{FrameData, Label, Scene, SubPos, Subtitle};
use crate::core::error::CandyError;

/// Centimeters per Typst point (1pt = 1/72in, 1in = 2.54cm).
const PT_PER_CM: f64 = 28.346_456_692_913_385;

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
        let response = self
            .agent
            .get(url)
            .call()
            .map_err(|err| match err {
                ureq::Error::Status(404, _) => {
                    std::io::Error::new(std::io::ErrorKind::NotFound, err)
                }
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

    /// Compute (once) the natural layout of every mobject by tagging each body
    /// with a label and reading back its position from the SVG.
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
        let mut src = format!(
            "{preamble}\n#set page(width: {w}pt, height: {h}pt, margin: 0pt, fill: white)\n",
            preamble = preamble,
            w = self.page_w,
            h = self.page_h,
        );
        // Deterministic order so positions are stable.
        let mut labels: Vec<&Label> = self.scene.items.keys().collect();
        labels.sort_by(|a, b| a.0.cmp(&b.0));
        for label in labels {
            // Substitute `ecval(...)` counter references (at t=0, i.e. the seed)
            // before compiling. This isolated layout pass has no `#let name =
            // ecounter(...)` binding in scope, so a bareword counter reference
            // like `ecval(r)` would otherwise fail with "unknown variable: r".
            let raw = self.scene.items[label].clone();
            let body = substitute_counters(&self.scene, &raw, 0);
            // Prefix with # so the body (a function-call expression like
            // "rect(width: 2cm, fill: red)") is evaluated, not treated as text.
            src.push_str(&format!("#{}\n", body));
            src.push_str(&format!(" <__candy_{}>\n", label.0));
        }

        let doc = self.compile(&src)?;
        let page = doc
            .pages()
            .first()
            .ok_or_else(|| CandyError::Typst("document produced no pages".into()))?;
        let svg = typst_svg::svg(page, &SvgOptions::default());
        let positions = parse_svg_positions(&svg)?;

        self.nat = positions;
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
        pixel_per_pt: f32,
    ) -> Result<crate::renderer::RenderedFrame, CandyError> {
        let nat = self.nat.get(label).cloned().unwrap_or((0.0, 0.0));
        let nat_cm = (nat.0 / PT_PER_CM, nat.1 / PT_PER_CM);
        let abs_x_cm = nat_cm.0 + st.x;
        let abs_y_cm = nat_cm.1 + st.y;
        let scale_pct = st.scale * 100.0;
        let body = content_for(&self.scene, label, time_ms);
        let preamble = imports_preamble(&self.scene);
        let placed = place_source(
            self.page_w,
            self.page_h,
            abs_x_cm,
            abs_y_cm,
            scale_pct,
            st.rotation,
            &body,
            &preamble,
        );

        let doc = self.compile(&placed)?;
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

        let mut labels: Vec<&Label> = states.keys().collect();
        labels.sort_by(|a, b| a.0.cmp(&b.0));

        let mut objs: Vec<(f64, crate::renderer::RenderedFrame)> = Vec::new();
        for label in &labels {
            let st = states.get(*label).unwrap();
            let frame = self.render_object_pixels(*label, st, time_ms, pixel_per_pt)?;
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

        let (w, h) = match objs.first() {
            Some((_, f)) => (f.width, f.height),
            None => (1, 1),
        };
        let mut canvas = vec![255u8; w * h * 4];
        for (opacity, f) in &objs {
            composite_over(&mut canvas, f, *opacity, w, h);
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

        // 2. Compute target pixel dimensions from the page size + ppi.
        let width = (self.page_w * pixel_per_pt as f64).round().max(1.0) as u32;
        let height = (self.page_h * pixel_per_pt as f64).round().max(1.0) as u32;

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

        // Deterministic z-order (same as the video path).
        let mut labels: Vec<&Label> = states.keys().collect();
        labels.sort_by(|a, b| a.0.cmp(&b.0));

        // White background, page-sized canvas.
        let mut out = String::new();
        out.push_str(&format!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{}\" height=\"{}\" viewBox=\"0 0 {} {}\" xmlns:xlink=\"http://www.w3.org/1999/xlink\">\n",
            self.page_w, self.page_h, self.page_w, self.page_h
        ));
        out.push_str(&format!(
            "<rect x=\"0\" y=\"0\" width=\"{}\" height=\"{}\" fill=\"white\"/>\n",
            self.page_w, self.page_h
        ));

        for label in labels {
            let st = &states[label];
            let obj_svg = self.render_object_svg(label, st, time_ms)?;
            // Wrap each object's SVG in a group with the per-frame opacity.
            // SVG <g opacity> applies to all descendants (shapes + text).
            let op = st.opacity.clamp(0.0, 1.0);
            out.push_str(&format!("<g opacity=\"{op}\">\n{obj_svg}\n</g>\n"));
        }

        // Subtitle overlays: one per visible scope, subject to
        // parental shadowing + auto-destroy. Drawn on top of the objects.
        for sub in &self.scene.subtitles {
            if self.scene.visible_subtitle_ids_at(time_ms).contains(&sub.id) {
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
    ) -> Result<String, CandyError> {
        let nat = self.nat.get(label).cloned().unwrap_or((0.0, 0.0));
        let nat_cm = (nat.0 / PT_PER_CM, nat.1 / PT_PER_CM);
        let abs_x_cm = nat_cm.0 + st.x;
        let abs_y_cm = nat_cm.1 + st.y;
        let scale_pct = st.scale * 100.0;
        let body = content_for(&self.scene, label, time_ms);
        let preamble = imports_preamble(&self.scene);

        let src = place_source(
            self.page_w,
            self.page_h,
            abs_x_cm,
            abs_y_cm,
            scale_pct,
            st.rotation,
            &body,
            &preamble,
        );

        let doc = self.compile(&src)?;
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
        let doc = self.compile(&self.object_source(frame, frame.time_ms))?;
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
        let body = content_for(&self.scene, &st.target, time_ms);
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

/// Replace every `ecval(<name>)` (or `ecval("name")`) reference in `body` with
/// the integer value of counter `name` at `time_ms`, per the scene's scope
/// shadowing / lifecycle rules. The integer is valid Typst, so the substituted
/// body still compiles.
fn substitute_counters(scene: &Scene, body: &str, time_ms: u32) -> String {
    let mut out = body.to_string();
    for c in &scene.counters {
        let val = scene.counter_value_at(&c.name, time_ms).to_string();
        for pat in [
            format!("ecval(\"{}\")", c.name),
            format!("ecval({})", c.name),
        ] {
            if out.contains(&pat) {
                out = out.replace(&pat, &val);
            }
        }
    }
    out
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
             #place(top + left, dx: {x_cm}cm, dy: {y_cm}cm)[ #scale(origin: top + left, {scale_pct}%)[ #{body} ] ]\n"
        )
    } else {
        format!(
            "{pre}#set page(width: {page_w}pt, height: {page_h}pt, margin: 0pt, fill: none)\n\
             #place(top + left, dx: {x_cm}cm, dy: {y_cm}cm)[ #scale(origin: top + left, {scale_pct}%)[ #rotate(origin: top + left, {rotation}deg)[ #{body} ] ] ]\n"
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

/// Parse `data-typst-label` positions out of a Typst SVG, accumulating group
/// transforms to recover each labeled element's absolute (x, y) in points.
fn parse_svg_positions(svg: &str) -> Result<HashMap<Label, (f64, f64)>, CandyError> {
    let mut positions: HashMap<Label, (f64, f64)> = HashMap::new();
    let mut stack: Vec<Matrix> = Vec::new();
    let mut current = Matrix::identity();

    let mut idx = 0;
    while idx < svg.len() {
        let Some(lt) = svg[idx..].find('<') else {
            break;
        };
        let lt = idx + lt;
        if svg[lt..].starts_with("</g>") {
            if let Some(m) = stack.pop() {
                current = m;
            }
            idx = lt + 4;
            continue;
        }
        let Some(gt) = svg[lt..].find('>') else { break };
        let gt = lt + gt;
        let tag = &svg[lt + 1..gt];
        if tag.starts_with("g ") || tag.starts_with("g>") || tag == "g" {
            let mut m = current;
            if let Some(t) = attr(tag, "transform") {
                m = compose(current, parse_transform(&t));
            }
            if let Some(label) = attr(tag, "data-typst-label") {
                positions.insert(Label(label), (m.e, m.f));
            }
            stack.push(current);
            current = m;
        }
        idx = gt + 1;
    }
    Ok(positions)
}

/// Extract `name="value"` (single or double quoted) from a tag string.
fn attr(tag: &str, name: &str) -> Option<String> {
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
    fn identity() -> Self {
        Matrix {
            a: 1.0,
            b: 0.0,
            c: 0.0,
            d: 1.0,
            e: 0.0,
            f: 0.0,
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

/// Parse a `transform` attribute (`translate(..)`, `scale(..)`, `matrix(..)`).
fn parse_transform(s: &str) -> Matrix {
    let mut m = Matrix::identity();
    let mut rest = s;
    while let Some(open) = rest.find('(') {
        let close = match rest[open..].find(')') {
            Some(c) => open + c,
            None => break,
        };
        let name_start = rest[..open]
            .rfind(|c: char| !(c.is_alphabetic() || c == '-'))
            .map(|i| i + 1)
            .unwrap_or(0);
        let name = &rest[name_start..open];
        let args = &rest[open + 1..close];
        let nums = parse_floats(args);
        let tm = match name {
            "translate" if nums.len() >= 2 => Matrix {
                a: 1.0,
                b: 0.0,
                c: 0.0,
                d: 1.0,
                e: nums[0],
                f: nums[1],
            },
            "translate" if nums.len() == 1 => Matrix {
                a: 1.0,
                b: 0.0,
                c: 0.0,
                d: 1.0,
                e: nums[0],
                f: 0.0,
            },
            "scale" if nums.len() >= 2 => Matrix {
                a: nums[0],
                b: 0.0,
                c: 0.0,
                d: nums[1],
                e: 0.0,
                f: 0.0,
            },
            "scale" if nums.len() == 1 => Matrix {
                a: nums[0],
                b: 0.0,
                c: 0.0,
                d: nums[0],
                e: 0.0,
                f: 0.0,
            },
            "matrix" if nums.len() >= 6 => Matrix {
                a: nums[0],
                b: nums[1],
                c: nums[2],
                d: nums[3],
                e: nums[4],
                f: nums[5],
            },
            _ => Matrix::identity(),
        };
        m = compose(m, tm);
        rest = &rest[close + 1..];
    }
    m
}

/// Parse whitespace/comma-separated floats.
fn parse_floats(s: &str) -> Vec<f64> {
    s.split(|c: char| {
        !(c.is_ascii_digit() || c == '.' || c == '-' || c == '+' || c == 'e' || c == 'E')
    })
    .filter(|t| !t.is_empty())
    .filter_map(|t| t.parse::<f64>().ok())
    .collect()
}

/// Test helper: compile a Typst source string to SVG (used to confirm the
/// shipped `lib.typ` is valid standard Typst).
#[cfg(test)]
pub(crate) fn compile_svg_for_test(src: &str) -> Result<String, CandyError> {
    // Use the same WorldState as the production Renderer: system fonts +
    // embedded fallbacks + local file resolver. This makes the test compile
    // identical to `typst compile`.
    let state = WorldState::new(PathBuf::new());
    let source = TypstSource::detached(src.to_string());
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
        slides: vec![Slide { duration_ms: 100, actions: vec![] }],
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
