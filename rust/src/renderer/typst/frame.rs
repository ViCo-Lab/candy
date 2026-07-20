use super::*;
use crate::renderer::RenderedFrame;
use crate::renderer::raster::cpu::rasterize_svg;

/// The largest morphable shape extracted from a body: its outline as a ring of
/// `[x, y]` points, plus optional fill and stroke paints.
pub(crate) type LargestShape = (Vec<[f64; 2]>, Option<String>, Option<String>);

impl Renderer {
    // =========================================================================
    // Whole-document native-Typst render path (the authentic typesetting model)
    // =========================================================================
    //
    // Typst typesets the *entire* document natively each frame. Every mobject
    // body is wrapped in `#move`/`#scale`/`#rotate` (all exist in typst 0.15) so
    // the animation is just a code expansion driven by the eased per-frame
    // counters — exactly the "easing-counter → Typst code expansion" model.
    // Static content stays in native flow, so positions and Z-order are always
    // correct and static + dynamic content is freely interleaved. The result is
    // a single standard SVG (`render_frame_at`), rasterized once by the `raster`
    // module — never a per-object pixel composite.

    /// Whole-document native-Typst SVG draft (compatible standard Typst SVG,
    /// not the hand-rolled composite the old path emitted — so it opens in any
    /// viewer, not just Inkscape). Delegates to [`render_frame_svg`].
    fn render_frame_at_whole_doc(
        &self,
        time_ms: u32,
        all_frames: &[FrameData],
    ) -> Result<String, CandyError> {
        let (states, camera) = self.prepare_states(all_frames, time_ms);
        self.render_frame_svg(&states, &camera, time_ms, false)
    }

    /// Build the SVG for one frame from precomputed `states` + `camera`.
    ///
    /// The base document is compiled (all transforms read from `sys.inputs`)
    /// and, when the document declares a `#camera` directive, composed with
    /// the camera `<g>` group + per-glyph transform/morph overlays + the
    /// camera-independent subtitle overlay ([`compose_frame_svg`]). When the
    /// document has no `#camera`, the base is returned directly (the
    /// "no-camera direct-output" path): subtitles render natively inside the
    /// document and there is no SVG assembly / overlay step. `hide_fading`
    /// controls whether opacity < 1 objects are hidden in the base (the
    /// pixel path draws them via the opacity overlay instead).
    fn render_frame_svg(
        &self,
        states: &HashMap<Label, FrameData>,
        camera: &Option<FrameData>,
        time_ms: u32,
        hide_fading: bool,
    ) -> Result<String, CandyError> {
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
        let inputs = self.build_frame_inputs(states, active, active_page, hide_fading, time_ms);
        let doc = self.compile_param_source(&inputs)?;
        let page = doc
            .pages()
            .get(active_page)
            .or_else(|| doc.pages().first())
            .ok_or_else(|| CandyError::Typst("document produced no pages".into(), None))?;
        let base = typst_svg::svg(page, &SvgOptions::default());
        // Canvas background color: honors the active scene's `bg` (inheriting
        // from a parent scene) and defaults to opaque white. Used as a fallback
        // in `compose_frame_svg` when `typst_svg` emits no recognizable page-fill
        // element, so the frame is never transparent.
        let bg_hex = if self.scene.scenes.is_empty() {
            "white".to_string()
        } else {
            self.scene_bg_hex(active)?
        };
        // Compose the full draft via `compose_frame_svg`. The camera `<g>`
        // group wrapping is applied *inside* that function only when
        // `camera.is_some()` (a per-frame decision, since the camera test
        // supplies `__camera__` through `frames` rather than a `#camera`
        // directive). The per-glyph transform/morph overlay is always
        // applied (it is needed even on no-camera documents, where the
        // base still carries animated transforms). Only the subtitle overlay
        // is gated on `has_camera_directive` (see `compose_frame_svg`),
        // so on no-camera documents the caption renders natively inside the
        // base document and is never overlaid.
        self.compose_frame_svg(&base, states, time_ms, camera, pw, ph, &bg_hex)
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
    #[allow(clippy::too_many_arguments)]
    fn compose_frame_svg(
        &self,
        base_svg: &str,
        states: &HashMap<Label, FrameData>,
        time_ms: u32,
        camera: &Option<FrameData>,
        pw: f64,
        ph: f64,
        bg_hex: &str,
    ) -> Result<String, CandyError> {
        // Extract the inner markup of the typst_svg document (between `<svg …>`
        // and `</svg>`), then split the leading background `<rect>` (which must
        // stay fixed, outside the camera group) from the mobject content.
        let open = base_svg
            .find("<svg")
            .ok_or_else(|| CandyError::Typst("bad svg".into(), None))?;
        let after = open
            + base_svg[open..]
                .find('>')
                .ok_or_else(|| CandyError::Typst("bad svg".into(), None))?
            + 1;
        let end = base_svg
            .rfind("</svg>")
            .ok_or_else(|| CandyError::Typst("bad svg".into(), None))?;
        let inner = &base_svg[after..end];
        // Split the leading page-fill background element from the mobject
        // content. `typst_svg` emits the scene's page background as the *first*
        // child of the document, but the tag varies by version: older builds use
        // a `<rect>`, current builds use a `<path>`. We accept either so the
        // background is always detected and drawn *outside* the camera group
        // (fixed, covering the whole canvas). If it were left inside the camera
        // group, a zoom/pan/rotate would transform the background too — it would
        // shrink on zoom-out and leave transparent (uncovered) edges instead of
        // the canvas background color.
        let (bg, content) = split_background(inner);
        let mut out = String::with_capacity(8192);
        out.push_str(&format!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\" xmlns:xlink=\"http://www.w3.org/1999/xlink\" width=\"{pw}\" height=\"{ph}\" viewBox=\"0 0 {pw} {ph}\">\n",
            pw = pw, ph = ph,
        ));
        if bg.is_empty() {
            // Defensive fallback: if `typst_svg` emitted no recognizable page-fill
            // background (or `split_background` couldn't bound it), draw the canvas
            // background ourselves so the frame is never transparent. `bg_hex`
            // honors the active scene's `bg` (inheriting from a parent scene) and
            // defaults to opaque white — exactly what the removed legacy path did
            // with its explicit `<rect fill=bg_hex>`.
            out.push_str(&format!(
                "<rect x=\"0\" y=\"0\" width=\"{pw}\" height=\"{ph}\" fill=\"{bg_hex}\"/>\n",
                pw = pw,
                ph = ph,
                bg_hex = bg_hex,
            ));
        } else {
            out.push_str(bg);
            out.push('\n');
        }
        if let Some(cam) = camera {
            out.push_str(&format!(
                "<g transform=\"{}\">\n",
                camera_transform_svg(cam, pw, ph)
            ));
        }
        out.push_str(content);
        out.push('\n');
        out.push_str(&self.morph_overlay_svg(states, time_ms));
        out.push_str(&self.transform_overlay_svg(states, time_ms));
        if camera.is_some() {
            out.push_str("</g>\n");
        }
        // Subtitles are drawn as a camera-independent overlay ONLY when the
        // document declares a `#camera` directive (in which case they were
        // blanked in the base document by `build_parameterized_source`).
        // On no-camera documents the caption renders natively inside the
        // document, so it must NOT be overlaid here (that would double-draw
        // it). `has_camera_directive` is the single source of truth for
        // both the blanking and the overlay, keeping them in sync.
        if self.has_camera_directive {
            let vis_subs = self.scene.visible_subtitle_ids_at(time_ms);
            for sub in &self.scene.subtitles {
                if vis_subs.contains(&sub.id) {
                    let svg = self.render_subtitle_svg(sub, time_ms)?;
                    // The subtitle SVG is a complete <svg>...</svg> document;
                    // extract only the inner content to avoid nested <svg> tags.
                    let inner = extract_svg_inner(&svg);
                    out.push_str(inner);
                    out.push('\n');
                }
            }
        }
        out.push_str("</svg>\n");
        Ok(out)
    }

    /// Public wrapper around `ensure_flow` so callers (e.g. the parallel
    /// rasterization loop in `build_input_with_gpu`) can pre-compute the
    /// flow layout before spawning parallel frame renders.
    pub fn ensure_flow_public(&mut self) -> Result<(), CandyError> {
        self.ensure_flow()
    }
    /// Test-only accessor for the computed flow (first-frame) top-left of a
    /// mobject, in Typst points (page origin). Used by the native-consistency /
    /// declaration-order regression tests.
    #[cfg(test)]
    pub(crate) fn flow_pos_for(&self, label: &Label) -> Option<(f64, f64)> {
        self.flow_pos.get(label).copied()
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
            .filter(|p| time_ms >= p.start_ms && time_ms <= p.end_ms)
            .map(|p| p.anims.len())
            .sum()
    }
    /// Render the full scene at a frame index to an SVG string (draft / fallback).
    ///
    /// The renderer has a single code path: the whole-document native-Typst SVG
    /// path (`render_frame_at_whole_doc`), which typesets the entire document
    /// natively each frame and is the single source of truth for the frame.
    pub fn render_frame_at(
        &mut self,
        time_ms: u32,
        all_frames: &[FrameData],
    ) -> Result<String, CandyError> {
        self.ensure_flow()?;
        self.render_frame_at_par(time_ms, all_frames)
    }
    /// Parallel-safe variant of [`render_frame_at`].
    ///
    /// Takes `&self` so it can be called from a rayon parallel iterator.
    /// **Precondition:** `ensure_flow()` must have been called once before
    /// any parallel call (it initializes `nat`/`page_w`/`page_h`). The
    /// [`Renderer::ensure_flow_public`] method exposes this.
    ///
    /// The renderer has a single code path now: the whole-document native-Typst
    /// SVG path (`render_frame_at_whole_doc`). The old hand-composed per-object
    /// SVG path was removed — the whole-document path is the single source of
    /// truth and the only one that keeps static + dynamic content, Z-order and
    /// typesetting faithful to native Typst.
    pub(crate) fn render_frame_at_par(
        &self,
        time_ms: u32,
        all_frames: &[FrameData],
    ) -> Result<String, CandyError> {
        self.render_frame_at_whole_doc(time_ms, all_frames)
    }
    /// Render a frame to RGBA pixels with per-object opacity applied
    /// through the SVG/pixel **bypass**.
    ///
    /// Typst 0.15 has no in-document `opacity()`, so a fading object
    /// (0 < opacity < 1) cannot be expressed in the compiled SVG.
    /// Instead the base frame is rasterized with every fading object *hidden*
    /// (full opacity for everything else), and each fading object is then
    /// rendered as its own full-opacity layer (everything else hidden) and
    /// alpha-composited over the base at its target opacity — exactly the
    /// "opacity changes go through the SVG bypass" model. Frames with no
    /// fading objects take the fast path (a single rasterization), so the
    /// common case is unchanged. The per-object layering reuses the same
    /// whole-document `sys.inputs` pipeline (no resurrection of the old
    /// per-object pixel path); only the final composite is a pixel op.
    pub(crate) fn render_frame_pixels(
        &self,
        time_ms: u32,
        all_frames: &[FrameData],
        tw: u32,
        th: u32,
    ) -> Result<RenderedFrame, CandyError> {
        let (states, camera) = self.prepare_states(all_frames, time_ms);
        let active = if self.scene.scenes.is_empty() {
            0
        } else {
            self.scene.active_scene_at(time_ms)
        };
        let active_page = self.pages.active_page_of(active, time_ms);
        // Base: every *non-fading* object at full opacity. Fading objects
        // are excluded from the base and re-composited on top at their
        // opacity, so they never show at full opacity in the base.
        let mut base_states = states.clone();
        let fading: Vec<(Label, f64)> = states
            .iter()
            .filter(|(l, s)| {
                let owner = self.label_scene.get(*l).copied().unwrap_or(active);
                owner == active
                    && self.pages.page_of(l).is_none_or(|p| p == active_page)
                    && !self.transform_hidden(l, time_ms)
                    && s.opacity > 1e-4
                    && s.opacity < 1.0 - 1e-4
            })
            .map(|(l, s)| {
                base_states.insert(
                    l.clone(),
                    FrameData {
                        opacity: 0.0,
                        ..s.clone()
                    },
                );
                (l.clone(), s.opacity)
            })
            .collect();
        let base_svg = self.render_frame_svg(&base_states, &camera, time_ms, true)?;
        let mut out = rasterize_svg(&base_svg, tw, th)?;
        if fading.is_empty() {
            return Ok(out);
        }
        for (label, op) in &fading {
            // One full-opacity layer: only `label` shown, everything else
            // (and the canvas background) hidden, so the layer is transparent
            // except for `label` — compositing it over the base at `op`
            // yields `label` at exactly `op` over the already-correct base.
            let mut layer_states = states.clone();
            for (k, s) in layer_states.iter_mut() {
                if k != label {
                    s.opacity = 0.0;
                }
            }
            if let Some(s) = layer_states.get_mut(label) {
                s.opacity = 1.0;
            }
            let layer_svg = self.render_frame_svg(&layer_states, &camera, time_ms, true)?;
            let layer = rasterize_svg(&layer_svg, tw, th)?;
            Self::composite_over(&mut out, &layer, *op);
        }
        Ok(out)
    }
    /// Alpha-composite `layer` over `base` at `op` opacity, premultiplied
    /// "over" (tiny-skia's `Pixmap` is premultiplied). Mutates `base`.
    fn composite_over(base: &mut RenderedFrame, layer: &RenderedFrame, op: f64) {
        if base.width != layer.width || base.height != layer.height {
            return;
        }
        let o = op.clamp(0.0, 1.0) as f32;
        let ba = &mut base.rgba;
        let la = &layer.rgba;
        for i in (0..ba.len()).step_by(4) {
            let ls = la[i + 3] as f32 / 255.0;
            let lo = ls * o;
            if lo <= 0.0 {
                continue;
            }
            let bo = ba[i + 3] as f32 / 255.0;
            let out_a = lo + bo * (1.0 - lo);
            if out_a <= 0.0 {
                continue;
            }
            // Premultiplied over: layer contribution scales by `o`, base by
            // `(1 - lo)` (base alpha already premultiplied).
            let lr = la[i] as f32;
            let lg = la[i + 1] as f32;
            let lb = la[i + 2] as f32;
            let br = ba[i] as f32;
            let bg = ba[i + 1] as f32;
            let bb = ba[i + 2] as f32;
            ba[i] = (lr * o + br * (1.0 - lo)).clamp(0.0, 255.0) as u8;
            ba[i + 1] = (lg * o + bg * (1.0 - lo)).clamp(0.0, 255.0) as u8;
            ba[i + 2] = (lb * o + bb * (1.0 - lo)).clamp(0.0, 255.0) as u8;
            ba[i + 3] = (out_a * 255.0).clamp(0.0, 255.0) as u8;
        }
    }
    /// Render a single target's frame as an isolated SVG (spec §4.4 style).
    pub fn render_frame(&mut self, frame: &FrameData) -> Result<String, CandyError> {
        if !self.scene.items.contains_key(&frame.target)
            && !self.scene.content_timeline.contains_key(&frame.target)
        {
            return Err(CandyError::LabelNotFound(
                frame.target.clone(),
                self.scene.artifacts.label_locs.get(&frame.target).cloned(),
            ));
        }
        self.ensure_flow()?;
        let source = self.object_source(frame, frame.time_ms)?;
        let doc = self.compile_cached(&source, &Dict::new())?;
        let page = doc
            .pages()
            .first()
            .ok_or_else(|| CandyError::Typst("document produced no pages".into(), None))?;
        let svg = typst_svg::svg(page, &SvgOptions::default());
        Ok(svg)
    }
    /// Build the isolated per-object source for a single target.
    fn object_source(&self, st: &FrameData, time_ms: u32) -> Result<String, CandyError> {
        let flow_pos = self.flow_pos.get(&st.target).cloned().unwrap_or((0.0, 0.0));
        let nat_cm = (flow_pos.0 / PT_PER_CM, flow_pos.1 / PT_PER_CM);
        let abs_x_cm = nat_cm.0 + st.x;
        let abs_y_cm = nat_cm.1 + st.y;
        let scale_pct = st.scale * 100.0;
        let (body, unknown_counters) = self.resolve_body(&st.target, time_ms);

        // Report E009 errors for any unknown counters found during rendering.
        if let Some(counter_name) = unknown_counters.first() {
            return Err(CandyError::UnknownKey(
                "ecnew".to_string(),
                counter_name.clone(),
                None,
            ));
        }

        let preamble = imports_preamble(&self.scene);
        Ok(place_source(
            self.page_w,
            self.page_h,
            abs_x_cm,
            abs_y_cm,
            scale_pct,
            st.rotation,
            &body,
            &preamble,
        ))
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
    /// Render a mobject body in isolation and return its largest outline shape
    /// (by absolute area) as a ring of points plus its paint. Returns `None` if
    /// the body produces no extractable outline (e.g. an image or a body whose
    /// shape candy can't morph — those fall back to the plain crossfade).
    pub(crate) fn body_largest_shape(
        &self,
        body: &str,
    ) -> Result<Option<LargestShape>, CandyError> {
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
            .ok_or_else(|| CandyError::Typst("body produced no pages".into(), None))?;
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
    pub(crate) fn morph_body_for(
        &self,
        label: &Label,
        time_ms: u32,
    ) -> Option<(String, Vec<String>)> {
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
            return Some((polygon_svg(&ring, &plan.fill, &plan.stroke), Vec::new()));
        }
        None
    }
}

/// Split the leading page-fill background element (the scene's canvas
/// background, always the first child emitted by `typst_svg`) from the rest of
/// the inner SVG markup.
///
/// `typst_svg` may render the page fill as a `<rect>` (older builds) or a
/// `<path>` (current builds); we accept whichever appears first. The returned
/// background slice is drawn *outside* the camera group so the canvas stays
/// filled with the background color regardless of any camera zoom/pan/rotate;
/// if it stayed inside the camera group it would be transformed along with the
/// mobjects and shrink on zoom-out, exposing transparent (uncovered) edges.
///
/// Returns `("", inner)` (no background extracted) when the inner markup has
/// neither a leading `<rect>` nor `<path>` (e.g. a transparent page fill).
fn split_background(inner: &str) -> (&str, &str) {
    let rect = inner.find("<rect");
    let path = inner.find("<path");
    let tag = match (rect, path) {
        (Some(r), Some(p)) => Some(r.min(p)),
        (Some(r), None) => Some(r),
        (None, Some(p)) => Some(p),
        (None, None) => None,
    };
    match tag {
        Some(i) => {
            // `typst_svg` emits these elements self-closing (`<rect .../>`,
            // `<path .../>`). If the closing `/>` is missing we cannot safely
            // bound the element, so fall back to "no background".
            match inner[i..].find("/>") {
                Some(j) => {
                    let close = i + j + 2;
                    (&inner[i..close], &inner[close..])
                }
                None => ("", inner),
            }
        }
        None => ("", inner),
    }
}

/// Extract the inner content of an `<svg>...</svg>` document, stripping
/// the outer `<svg ...>` and `</svg>` tags. This is used when embedding
/// subtitle SVGs (which are complete documents) into the frame's outer SVG.
fn extract_svg_inner(svg: &str) -> &str {
    // Find the first `>` after `<svg` (the end of the opening tag).
    let open = match svg.find("<svg") {
        Some(i) => match svg[i..].find('>') {
            Some(j) => i + j + 1,
            None => return svg,
        },
        None => return svg,
    };
    // Find the last `</svg>`.
    let close = match svg.rfind("</svg>") {
        Some(i) => i,
        None => return svg,
    };
    if close > open {
        svg[open..close].trim()
    } else {
        svg
    }
}
