use super::*;

impl Renderer {
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
    pub(crate) fn body_largest_shape(
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
    pub(crate) fn morph_body_for(&self, label: &Label, time_ms: u32) -> Option<String> {
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
