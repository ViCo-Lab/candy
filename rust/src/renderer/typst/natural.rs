use super::*;
use typst_library::introspection::Label as TypstLabel;

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
        // Binary search: all_frames is sorted by time_ms, so partition_point
        // gives us the index of the first frame with time_ms > time_ms.
        // We only iterate frames[0..idx] — O(T·log N) instead of O(N·T).
        let idx = all_frames.partition_point(|f| f.time_ms <= time_ms);
        let mut states: HashMap<Label, FrameData> = HashMap::new();
        for f in &all_frames[..idx] {
            states
                .entry(f.target.clone())
                .and_modify(|e| {
                    if f.time_ms >= e.time_ms {
                        *e = f.clone();
                    }
                })
                .or_insert_with(|| f.clone());
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
            let cam_start = if let Some(cs) = self.cam_start {
                cs
            } else {
                let cs = all_frames
                    .iter()
                    .filter(|f| f.target.0 == CAMERA_LABEL && is_nonidentity(f))
                    .map(|f| f.time_ms)
                    .min()
                    .unwrap_or(0);
                // self is &self so we can't mutate; store in local for reuse
                // within this call. The cam_start is constant across frames,
                // so this scan runs at most once per parallel batch.
                cs
            };
            let home = self.scene.active_scene_at(cam_start);
            let active = self.scene.active_scene_at(time_ms);
            if active != home {
                camera = None;
            }
        }
        // Use cached parent_labels (precomputed in ensure_natural).
        let mut out: HashMap<Label, FrameData> = HashMap::new();
        for label in states.keys() {
            if self.parent_labels.contains(label)
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
    /// `#mobject(name, body)` now returns `block(body) + label(name)`, so
    /// Typst stacks them top-to-bottom (left-aligned) with its standard
    /// spacing, exactly as the same bodies would land if written directly in a
    /// Typst source. This is what makes `#play` beats and `mobject` text appear
    /// at their "standard mode" positions instead of all piling up at the page
    /// origin (0, 0), and it stays consistent with how hand-written Typst would
    /// typeset the same content (no synthetic `#stack` gap or centring).
    ///
    /// The natural (first-frame) position of each mobject is read straight
    /// from the compiled document via the Typst **introspector**: because every
    /// mobject carries its own `label(name)`, `introspector.query_label`
    /// resolves the labelled content and `position()` yields its plain-Typst
    /// top-left — no colour-bbox measurement trick is needed. The whole
    /// parameterized source is compiled once per scene with that scene active and
    /// all transforms at their identity defaults, so every object sits exactly
    /// where plain Typst would lay it.
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
        // Measure each mobject's natural (first-frame) flow position directly
        // from the compiled document via the Typst introspector — no colour-bbox
        // trick. Every `#mobject(name, body)` now returns
        // `block(body) + label(name)`, so `introspector.query_label` resolves
        // the labelled content and `position()` yields its plain-Typst
        // top-left. Because the whole-document source is compiled with all
        // transforms at their *identity* defaults (`dx`/`dy` = 0, `s` = 100%,
        // `r` = 0, `hide` = false), every mobject sits exactly where
        // plain Typst would lay it — faithful to native layout, and a mobject
        // hidden at frame 0 (its `content_timeline` resolves to `none`) still
        // reserves its box because the wrapper shows the full body at the default
        // `reveal:len` / `body_idx`, so it keeps its natural slot.
        let mut nat: HashMap<Label, (f64, f64)> = HashMap::new();
        // Synthetic mobjects created by `#transform` / `#morph` (e.g.
        // `__xf_eq_0`, `__xf_eq_0_from`) are parked copies of the *target*
        // content. They must share the target's natural position — if they were
        // laid out as separate blocks they would push the target and every later
        // mobject down the page, making formulas fall off-screen and causing
        // the old content to render as a displaced ghost while the target is
        // translated.
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
        // Compile once per scene with that scene active (nested scenes render
        // only while active) and all transforms at identity, then introspect
        // each owning mobject's label position.
        let scene_ids: Vec<usize> = if self.scene.scenes.is_empty() {
            vec![0]
        } else {
            self.scene.scenes.iter().map(|s| s.id).collect()
        };
        for sid in scene_ids {
            let mut inputs = Dict::new();
            if !self.scene.scenes.is_empty() {
                inputs.insert("candy:active_scene".into(), Value::Int(sid as i64));
            }
            let doc = self.compile(&self.param_source, &inputs)?;
            scene_page_counts.insert(sid, doc.pages().len().max(1));
            let intro = doc.introspector();
            let labels: Vec<Label> = if self.scene.scenes.is_empty() {
                self.scene.items.keys().cloned().collect()
            } else {
                self.scene
                    .scenes
                    .iter()
                    .find(|s| s.id == sid)
                    .map(|s| s.owns_labels.clone())
                    .unwrap_or_default()
            };
            for label in labels {
                // Synthetic tmps inherit the target's natural position below.
                if tmp_to_target.contains_key(&label) {
                    continue;
                }
                let Ok(content) = intro.query_label(TypstLabel::new(label.0.to_string().into()))
                else {
                    continue;
                };
                let Some(loc) = content.location() else {
                    continue;
                };
                let Some(pos) = intro.position(loc) else {
                    continue;
                };
                nat.insert(label.clone(), (pos.point.x, pos.point.y));
                page_of.insert(label.clone(), pos.page.get() - 1);
            }
        }
        // Synthetic `#transform` / `#morph` tmps inherit their target's
        // natural position (and page). The scheduler positions them relative to
        // the target, so this keeps old-content crossfades / morphs aligned
        // with the target instead of drifting down the page and ghosting as a
        // duplicate.
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
        // Precompute invariant per-frame data to avoid O(N·T) scans in prepare_states.
        // Camera start: first non-identity keyframe time.
        self.parent_labels = self.scene.groups.values().cloned().collect();
        // cam_start is computed lazily in prepare_states on first call (needs all_frames).
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
