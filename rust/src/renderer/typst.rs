//! Render `FrameData` into SVG (and, for the video path, RGBA) using the
//! `typst` compiler library in-process — no `typst` CLI is spawned.
//!
//! Auto-positioning: each `mobject`'s body is laid out by Typst *naturally*
//! (the user never specifies a position). To recover those positions we render
//! a "natural" document — every body tagged with a `<__candy_<label>>` label —
//! to SVG and read each labeled group's `transform` (Typst emits
//! `data-typst-label` + `transform` on the group). Per animation frame we then
//! place each body at `natural + delta` and `scale`/opacity it, compositing the
//! objects (with per-object opacity) onto a single opaque-white canvas.

use std::collections::HashMap;

use typst::{Library, LibraryExt, World};
use typst_layout::PagedDocument;
use typst_library::diag::FileError;
use typst_library::foundations::{Bytes, Datetime, Duration};
use typst_library::text::{Font, FontBook};
use typst_render::{render, RenderOptions};
use typst_svg::SvgOptions;
use typst_syntax::{FileId, Source as TypstSource};
use typst_utils::{LazyHash, Scalar};

use crate::core::ast::{FrameData, Label, Scene};
use crate::core::error::CandyError;

/// Centimeters per Typst point (1pt = 1/72in, 1in = 2.54cm).
const PT_PER_CM: f64 = 28.346_456_692_913_385;

/// A `World` that compiles a single detached source string with a shared
/// library/book (so we don't rebuild the std library per frame).
struct CandyWorld<'a> {
    main: TypstSource,
    library: &'a LazyHash<Library>,
    book: &'a LazyHash<FontBook>,
}

impl<'a> World for CandyWorld<'a> {
    fn library(&self) -> &LazyHash<Library> {
        self.library
    }

    fn book(&self) -> &LazyHash<FontBook> {
        self.book
    }

    fn main(&self) -> FileId {
        self.main.id()
    }

    fn source(&self, id: FileId) -> Result<TypstSource, FileError> {
        if id == self.main.id() {
            Ok(self.main.clone())
        } else {
            Err(FileError::NotFound(std::path::PathBuf::from("missing")))
        }
    }

    fn file(&self, _id: FileId) -> Result<Bytes, FileError> {
        Err(FileError::NotFound(std::path::PathBuf::from("missing")))
    }

    fn font(&self, _index: usize) -> Option<Font> {
        None
    }

    fn today(&self, _offset: Option<Duration>) -> Option<Datetime> {
        None
    }
}

/// Renders a [`Scene`] into frames, with auto-detected mobject positions.
pub struct Renderer {
    scene: Scene,
    library: LazyHash<Library>,
    book: LazyHash<FontBook>,
    /// Natural (first-frame) position of each mobject, in Typst points.
    nat: HashMap<Label, (f64, f64)>,
    /// Full-canvas page size in points (from the natural document).
    page_w: f64,
    page_h: f64,
    natural_computed: bool,
}

impl Renderer {
    /// Build a renderer from a parsed [`Scene`].
    pub fn new(scene: Scene) -> Result<Self, CandyError> {
        scene.validate().map_err(CandyError::Parse)?;
        Ok(Self {
            scene,
            library: LazyHash::new(Library::default()),
            book: LazyHash::new(FontBook::new()),
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
            main: source,
            library: &self.library,
            book: &self.book,
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
        let mut src = String::from("#set page(width: auto, height: auto, margin: 0pt, fill: white)\n");
        // Deterministic order so positions are stable.
        let mut labels: Vec<&Label> = self.scene.items.keys().collect();
        labels.sort_by(|a, b| a.0.cmp(&b.0));
        for label in labels {
            let body = &self.scene.items[label];
            src.push_str(body);
            src.push_str(&format!(" <__candy_{}>\n", label.0));
        }

        let doc = self.compile(&src)?;
        let page = doc
            .pages()
            .first()
            .ok_or_else(|| CandyError::Typst("document produced no pages".into()))?;
        let size = page.frame.size();
        let (w, h) = (size.x.to_pt(), size.y.to_pt());
        let svg = typst_svg::svg(page, &SvgOptions::default());
        let positions = parse_svg_positions(&svg)?;

        self.page_w = w;
        self.page_h = h;
        self.nat = positions;
        self.natural_computed = true;
        Ok(())
    }

    /// The frame-0 visual state for a label (opacity 0 for `play` blocks).
    fn initial_for(&self, label: Label, frame_idx: u32) -> FrameData {
        match self.scene.initial.get(&label) {
            Some(f) => FrameData {
                frame_idx,
                target: label,
                x: f.x,
                y: f.y,
                scale: f.scale,
                opacity: f.opacity,
                easing: f.easing,
            },
            None => FrameData::new(frame_idx, label),
        }
    }

    /// Render a single mobject at its placed position onto a transparent
    /// full-canvas RGBA frame (page-sized).
    fn render_object_pixels(
        &self,
        label: &Label,
        st: &FrameData,
        pixel_per_pt: f32,
    ) -> Result<crate::renderer::RenderedFrame, CandyError> {
        let nat = self.nat.get(label).cloned().unwrap_or((0.0, 0.0));
        let nat_cm = (nat.0 / PT_PER_CM, nat.1 / PT_PER_CM);
        let abs_x_cm = nat_cm.0 + st.x;
        let abs_y_cm = nat_cm.1 + st.y;
        let scale_pct = st.scale * 100.0;
        let body = self.scene.items.get(label).map(|s| s.as_str()).unwrap_or("");

        let src = format!(
            "#set page(width: {}pt, height: {}pt, margin: 0pt, fill: none)\n\
             #place(top + left, dx: {}cm, dy: {}cm)[ #scale(origin: top + left, {}%)[ {} ] ]\n",
            self.page_w, self.page_h, abs_x_cm, abs_y_cm, scale_pct, body
        );

        let doc = self.compile(&src)?;
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
        frame_idx: u32,
        all_frames: &[FrameData],
        pixel_per_pt: f32,
    ) -> Result<crate::renderer::RenderedFrame, CandyError> {
        self.ensure_natural()?;

        let mut states: HashMap<Label, FrameData> = HashMap::new();
        for f in all_frames {
            if f.frame_idx == frame_idx {
                states.insert(f.target.clone(), f.clone());
            }
        }
        for label in self.scene.items.keys() {
            states
                .entry(label.clone())
                .or_insert_with(|| self.initial_for(label.clone(), frame_idx));
        }

        // Deterministic z-order.
        let mut labels: Vec<&Label> = states.keys().collect();
        labels.sort_by(|a, b| a.0.cmp(&b.0));

        let mut objs: Vec<(f64, crate::renderer::RenderedFrame)> = Vec::new();
        for label in &labels {
            let st = states.get(*label).unwrap();
            let frame = self.render_object_pixels(*label, st, pixel_per_pt)?;
            objs.push((st.opacity, frame));
        }

        let (w, h) = match objs.first() {
            Some((_, f)) => (f.width, f.height),
            None => (1, 1),
        };
        let mut canvas = vec![255u8; w * h * 4]; // opaque white
        for (opacity, f) in &objs {
            composite_over(&mut canvas, f, *opacity, w, h);
        }
        Ok(crate::renderer::RenderedFrame {
            width: w,
            height: h,
            rgba: canvas,
        })
    }

    /// Render the full scene at a frame index to an SVG string (draft / fallback).
    ///
    /// Unlike the older implementation, this applies per-object `opacity` by
    /// rendering each mobject as its own SVG and composing them via nested
    /// `<svg opacity="...">` elements. This closes the gap with the video path
    /// (which always applied opacity via `composite_over`) — the SVG draft and
    /// the encoded video now agree visually.
    pub fn render_frame_at(&mut self, frame_idx: u32, all_frames: &[FrameData]) -> Result<Vec<u8>, CandyError> {
        self.ensure_natural()?;
        let mut states: HashMap<Label, FrameData> = HashMap::new();
        for f in all_frames {
            if f.frame_idx == frame_idx {
                states.insert(f.target.clone(), f.clone());
            }
        }
        for label in self.scene.items.keys() {
            states
                .entry(label.clone())
                .or_insert_with(|| self.initial_for(label.clone(), frame_idx));
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
            let obj_svg = self.render_object_svg(label, st)?;
            // Wrap each object's SVG in a group with the per-frame opacity.
            // SVG <g opacity> applies to all descendants (shapes + text).
            let op = st.opacity.clamp(0.0, 1.0);
            out.push_str(&format!("<g opacity=\"{op}\">\n{obj_svg}\n</g>\n"));
        }

        out.push_str("</svg>\n");
        Ok(out.into_bytes())
    }

    /// Render a single mobject at its placed position as an SVG string.
    /// Uses the same placement math as `render_object_pixels`.
    fn render_object_svg(&self, label: &Label, st: &FrameData) -> Result<String, CandyError> {
        let nat = self.nat.get(label).cloned().unwrap_or((0.0, 0.0));
        let nat_cm = (nat.0 / PT_PER_CM, nat.1 / PT_PER_CM);
        let abs_x_cm = nat_cm.0 + st.x;
        let abs_y_cm = nat_cm.1 + st.y;
        let scale_pct = st.scale * 100.0;
        let body = self.scene.items.get(label).map(|s| s.as_str()).unwrap_or("");

        let src = format!(
            "#set page(width: {}pt, height: {}pt, margin: 0pt, fill: none)\n\
             #place(top + left, dx: {}cm, dy: {}cm)[ #scale(origin: top + left, {}%)[ {} ] ]\n",
            self.page_w, self.page_h, abs_x_cm, abs_y_cm, scale_pct, body
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
        if !self.scene.items.contains_key(&frame.target) {
            return Err(CandyError::LabelNotFound(frame.target.clone()));
        }
        self.ensure_natural()?;
        let doc = self.compile(&self.object_source(frame))?;
        let page = doc
            .pages()
            .first()
            .ok_or_else(|| CandyError::Typst("document produced no pages".into()))?;
        let svg = typst_svg::svg(page, &SvgOptions::default());
        Ok(svg.into_bytes())
    }

    /// Build the isolated per-object source for a single target.
    fn object_source(&self, st: &FrameData) -> String {
        let nat = self.nat.get(&st.target).cloned().unwrap_or((0.0, 0.0));
        let nat_cm = (nat.0 / PT_PER_CM, nat.1 / PT_PER_CM);
        let abs_x_cm = nat_cm.0 + st.x;
        let abs_y_cm = nat_cm.1 + st.y;
        let scale_pct = st.scale * 100.0;
        let body = self.scene.items.get(&st.target).map(|s| s.as_str()).unwrap_or("");
        format!(
            "#set page(width: {}pt, height: {}pt, margin: 0pt, fill: none)\n\
             #place(top + left, dx: {}cm, dy: {}cm)[ #scale(origin: top + left, {}%)[ {} ] ]\n",
            self.page_w, self.page_h, abs_x_cm, abs_y_cm, scale_pct, body
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
        let Some(lt) = svg[idx..].find('<') else { break };
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
        Matrix { a: 1.0, b: 0.0, c: 0.0, d: 1.0, e: 0.0, f: 0.0 }
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
            "translate" if nums.len() >= 2 => Matrix { a: 1.0, b: 0.0, c: 0.0, d: 1.0, e: nums[0], f: nums[1] },
            "translate" if nums.len() == 1 => Matrix { a: 1.0, b: 0.0, c: 0.0, d: 1.0, e: nums[0], f: 0.0 },
            "scale" if nums.len() >= 2 => Matrix { a: nums[0], b: 0.0, c: 0.0, d: nums[1], e: 0.0, f: 0.0 },
            "scale" if nums.len() == 1 => Matrix { a: nums[0], b: 0.0, c: 0.0, d: nums[0], e: 0.0, f: 0.0 },
            "matrix" if nums.len() >= 6 => Matrix { a: nums[0], b: nums[1], c: nums[2], d: nums[3], e: nums[4], f: nums[5] },
            _ => Matrix::identity(),
        };
        m = compose(m, tm);
        rest = &rest[close + 1..];
    }
    m
}

/// Parse whitespace/comma-separated floats.
fn parse_floats(s: &str) -> Vec<f64> {
    s.split(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-' || c == '+' || c == 'e' || c == 'E'))
        .filter(|t| !t.is_empty())
        .filter_map(|t| t.parse::<f64>().ok())
        .collect()
}

/// Test helper: compile a Typst source string to SVG (used to confirm the
/// shipped `lib.typ` is valid standard Typst).
#[cfg(test)]
pub(crate) fn compile_svg_for_test(src: &str) -> Result<String, CandyError> {
    let source = TypstSource::detached(src.to_string());
    let library = LazyHash::new(Library::default());
    let book = LazyHash::new(FontBook::new());
    let world = CandyWorld {
        main: source,
        library: &library,
        book: &book,
    };
    let warned = typst::compile::<PagedDocument>(&world);
    match warned.output {
        Ok(doc) => {
            let page = doc.pages().first().ok_or_else(|| CandyError::Typst("no pages".into()))?;
            Ok(typst_svg::svg(page, &SvgOptions::default()))
        }
        Err(e) => Err(CandyError::Typst(format!("{:?}", e))),
    }
}
