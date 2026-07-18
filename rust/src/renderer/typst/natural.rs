use super::*;

impl Renderer {
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
    pub(crate) fn prepare_states(
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
        if !self.scene.scenes.is_empty() && camera.is_some() {
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
        // Synthetic group parents (empty body) are containers, not drawn.
        let parent_labels: std::collections::HashSet<&Label> = self.scene.groups.values().collect();
        let mut out: HashMap<Label, FrameData> = HashMap::new();
        for label in states.keys() {
            if parent_labels.contains(label)
                && self
                    .scene
                    .items
                    .get(label)
                    .is_some_and(|b| b.trim() == "none")
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
    pub(crate) fn ensure_natural(&mut self) -> Result<(), CandyError> {
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
                let (frame0, _unknown) = content_for(&self.scene, label, 0);
                let frame0_t = frame0.trim();
                if (natural.is_empty() || natural == "none")
                    && (frame0_t.is_empty() || frame0_t == "none")
                {
                    continue;
                }
                // Distinct, safe 24-bit colour (skips black/white corners).
                let color = format!("#{:06x}", 0x010101u32.wrapping_add(i as u32) & 0xFFFFFF);
                palette.push((label.clone(), color.clone()));
                let (natural_sub, _unknown) = substitute_counters(&self.scene, natural, 0);
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
            let doc = self.compile(&src, &Dict::new())?;
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
        // A cross-page scene's mobjects play out across its overflow pages, so
        // its content is "on stage" for the *entire* page-playback schedule —
        // not just its (often zero-duration) content interval. Extend each
        // scene's `end_ms` to cover its schedule so `active_scene_at` keeps the
        // scene active while the pages play in sequence; otherwise the scene's
        // interval would close immediately and `active_scene_at` would fall back
        // to the empty root scene, leaving every page after the first blank.
        if !self.scene.scenes.is_empty() {
            for s in self.scene.scenes.iter_mut() {
                if let Some(end) = self.pages.schedule_end_ms(s.id) {
                    if end > s.end_ms {
                        s.end_ms = end;
                    }
                }
            }
        }
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
}
