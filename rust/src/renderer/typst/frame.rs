use super::*;

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
    /// viewer, not just Inkscape). The per-glyph `#transform` overlay and the
    /// subtitles are injected here, so the draft is the single source of truth
    /// for the whole frame (the "new" SVG path).
    fn render_frame_at_whole_doc(
        &self,
        time_ms: u32,
        all_frames: &[FrameData],
    ) -> Result<String, CandyError> {
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
        // element, so the frame is never transparent. (Salvaged from the removed
        // legacy path, which always drew an explicit `<rect fill=bg_hex>`.)
        let bg_hex = if self.scene.scenes.is_empty() {
            "white".to_string()
        } else {
            self.scene_bg_hex(active)?
        };
        // Compose the full draft: base document + (camera group wrapping the
        // mobjects and the per-glyph transform overlay) + subtitles. The transform
        // overlay is the "new" SVG path — the formula is embedded once in `<defs>`
        // and reused via `<use>`, so it is never copied many times.
        let out = self.compose_frame_svg(&base, &states, time_ms, &camera, pw, ph, &bg_hex)?;
        Ok(out)
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
        out.push_str("</svg>\n");
        Ok(out)
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
        self.ensure_natural()?;
        self.render_frame_at_par(time_ms, all_frames)
    }
    /// Parallel-safe variant of [`render_frame_at`].
    ///
    /// Takes `&self` so it can be called from a rayon parallel iterator.
    /// **Precondition:** `ensure_natural()` must have been called once before
    /// any parallel call (it initializes `nat`/`page_w`/`page_h`). The
    /// [`Renderer::ensure_natural_public`] method exposes this.
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
        self.ensure_natural()?;
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
        let nat = self.nat.get(&st.target).cloned().unwrap_or((0.0, 0.0));
        let nat_cm = (nat.0 / PT_PER_CM, nat.1 / PT_PER_CM);
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
