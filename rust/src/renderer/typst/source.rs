use super::*;
use crate::core::ast::SceneInfo;

impl Renderer {
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
    ///   `sys.inputs`. Each mobject is pinned at the page origin with
    ///   `place(top + left)` and then shifted by the per-frame `dx`/`dy`, so all
    ///   mobjects share a single reference point (the origin) regardless of how
    ///   many there are — they never overflow the canvas into extra pages.
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
    /// * Every `#scene` call is wrapped by **Rust-generated** gating source (no
    ///   weird parameters are added to the Typst `scene` function): a code block
    ///   that reads `sys.inputs.at("candy:active_scene")` and, using the scene's
    ///   (Rust-known) id and descendant set as literal values, decides whether to
    ///   render the scene's `page()` (active == 0 or == its id), emit just the
    ///   scene *body* with **no** `page()` (so a nested descendant scene inside
    ///   it can render — a `page()` inside another `page()` is illegal in Typst),
    ///   or emit `none`. This is what makes *nested* scenes work while still
    ///   emitting exactly one page per frame and keeping the `body_cache` hit
    ///   rate high. Scenes are processed innermost-first so a parent's gated body
    ///   already contains its (wrapped) child scene.
    ///
    /// Edits are applied in **character space** (not raw bytes) so a cumulative
    /// shift can never land inside a multi-byte character — this is what made
    /// the old per-frame `replace_range` byte-splicing panic ("end of range
    /// should be a character boundary") impossible.
    pub(crate) fn build_parameterized_source(scene: &Scene) -> String {
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
            // A target that has a `content_timeline` swaps its body at some
            // frame(s): a `reveal`/`typewriter` target (string literal) grows a
            // revealed prefix, while a formula/shape `#transform` swaps the whole
            // body. Both must be expressed as per-frame inputs so the *single*
            // parameterized source keeps rendering the right content (otherwise the
            // base document would freeze on the original body and the formula would
            // snap back after every transform).
            let string_full = scene.items.get(label).and_then(|b| strip_string_literal(b));
            if let Some(full) = string_full {
                if scene.content_timeline.contains_key(label) {
                    inner = Self::reveal_wrap_body(&label.0, &inner, full.chars().count());
                }
            } else if scene.content_timeline.contains_key(label) {
                // Non-string (formula / shape) `#transform`: select among the
                // original body and each swapped body via a `…:body_idx` input.
                if let Some(sel) = Self::content_selection_body(label, body, scene) {
                    inner = sel;
                }
            }
            let cs = src[..bs].chars().count();
            let ce = src[..be].chars().count();
            edits.push((cs, ce, Self::wrap_mobject_inputs(&label.0, &inner)));
        }
        // 2. Blank each `#subtitle(...)` call out of the base document. The
        //    caption is ALWAYS drawn as a separate, camera-independent overlay
        //    (see `compose_frame_svg`), so it must never render natively inside
        //    the base document — leaving it in would both double-draw it and
        //    (on no-camera documents) render every caption on every frame at
        //    once, breaking automatic subtitle switching. The `subtitle_call`
        //    range already includes the leading `#`, so replacing it with `#none`
        //    yields a no-op caption in the base.
        for &(ss, se) in scene.artifacts.subtitle_call.values() {
            let cs = src[..ss].chars().count();
            let ce = src[..se].chars().count();
            edits.push((cs, ce, "#none".to_string()));
        }
        // Apply right-to-left (descending start) so nested ranges stay correct.
        edits.sort_by_key(|e| std::cmp::Reverse(e.0));
        let mut out: Vec<char> = chars;
        for (s, e, rep) in edits {
            let rep_chars: Vec<char> = rep.chars().collect();
            out.splice(s..e, rep_chars);
        }
        let mut cur = out.iter().collect::<String>();
        // 3. Rust-managed scene gating. Each `#scene(...)` call is wrapped in a
        //    code block that reads `candy:active_scene` and, using the scene's
        //    (Rust-known) id and descendant set as literal values, decides what to
        //    emit: its own `page()` (active == 0 or == its id), the *page* of the
        //    active descendant scene (so a nested child renders without a
        //    page-in-page — a `page()` inside another `page()` is illegal in
        //    Typst), or `none`. No weird parameters are added to the Typst `scene`
        //    function; the gating is fully generated by Rust. Scenes are processed
        //    innermost-first so a parent's wrapper already contains its (wrapped)
        //    child scene.
        let ordered: Vec<usize> = {
            let mut v: Vec<usize> = scene
                .scenes
                .iter()
                .filter(|s| scene.artifacts.scene_call.contains_key(&s.id))
                .map(|s| s.id)
                .collect();
            v.sort_by_key(|id| {
                scene
                    .artifacts
                    .scene_call
                    .get(id)
                    .map(|(st, _)| *st)
                    .unwrap_or(0)
            });
            v
        };
        let descendants = Self::build_descendants(&scene.scenes);
        let depth_of = Self::scene_depth_map(&scene.scenes);
        let mut order_inner = ordered.clone();
        order_inner.sort_by_key(|id| std::cmp::Reverse(depth_of.get(id).copied().unwrap_or(0)));
        let mut wrapped: HashMap<usize, String> = HashMap::new();
        for &sid in &order_inner {
            let k = match ordered.iter().position(|x| *x == sid) {
                Some(k) => k,
                None => continue,
            };
            let Some((cs, ce)) = Self::find_kth_scene_call(&cur, k) else {
                continue;
            };
            let call_text = &cur[cs..ce];
            let call_inner = call_text.strip_prefix('#').unwrap_or(call_text);
            let mut wrapper = format!(
                "{{{{ let __a = sys.inputs.at(\"candy:active_scene\", default: 0); if __a == 0 or __a == {sid} {{ {call_inner} }}"
            );
            if let Some(desc) = descendants.get(&sid) {
                for &d in desc {
                    if let Some(w) = wrapped.get(&d) {
                        wrapper.push_str(&format!(" else if __a == {d} {{ {w} }}"));
                    }
                }
            }
            wrapper.push_str(" else { none } }}");
            wrapped.insert(sid, wrapper.clone());
            cur.replace_range(cs..ce, &wrapper);
        }
        // Rewrite any file-style `#import "candy"` to the canonical
        // `@preview/candy:<version>` form so the World can resolve it
        // in-process (see `WorldState::candy_local`). In production the parser
        // rejects file-style imports (CandyDumpedYou), but test code and
        // `--ignore-version` mode still use them, so we rewrite here too.
        let v = crate::CANDY_VERSION;
        cur = cur
            .replace(
                "#import \"candy\":",
                &format!("#import \"@preview/candy:{v}\":"),
            )
            .replace(
                "#import \"candy\"",
                &format!("#import \"@preview/candy:{v}\""),
            );
        cur
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
        edits.sort_by_key(|e| std::cmp::Reverse(e.0.start));
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
    fn collect_ecval_input_edits(
        node: &LinkedNode,
        edits: &mut Vec<(std::ops::Range<usize>, String)>,
    ) {
        if let Some(call) = node.get().cast::<ast::FuncCall>() {
            if let Some(name) = Self::ecval_input_name(&call) {
                let key = format!("candy:counter:{name}");
                edits.push((
                    node.range(),
                    format!("sys.inputs.at(\"{key}\", default: 0)"),
                ));
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
        // The first positional argument is the counter name. A leading named
        // argument means this isn't the canonical read form → bail.
        match call.args().items().next() {
            Some(ast::Arg::Pos(p)) => match p {
                Expr::Str(s) => Some(s.get().to_string()),
                Expr::Ident(i) => Some(i.as_str().to_string()),
                _ => None,
            },
            _ => None,
        }
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
        // Position model: `move` is a *relative* transform, so `dx`/`dy` are the
        // delta from the mobject's flow position (computed in
        // `build_frame_inputs` as `target − flow_pos` for positioned mobjects, or
        // `(0, 0)` for un-positioned ones so they stay in their flow
        // slot). `scale`/`rotate` are absolute transforms with **origin: center**
        // so the object scales/rotates around its own centre (Manim semantics).
        // Opacity is NOT applied here — it is composited via the SVG bypass.
        format!(
            "{{ let __b = ({inner}); if sys.inputs.at(\"candy:{label}:hide\", default: false) {{ none }} else {{ move(dx: sys.inputs.at(\"candy:{label}:dx\", default: 0) * 1cm, dy: sys.inputs.at(\"candy:{label}:dy\", default: 0) * 1cm)[#scale(origin: center, sys.inputs.at(\"candy:{label}:s\", default: 100) * 1%)[#rotate(origin: center, sys.inputs.at(\"candy:{label}:r\", default: 0) * 1deg)[#__b]]] }} }}",
            inner = inner,
            label = label,
        )
    }

    /// For a `#transform` target whose body is NOT a string literal (a formula
    /// or a shape), build a body expression that selects among the original body
    /// and each `content_timeline` entry, driven by the per-frame
    /// `sys.inputs.at("candy:<label>:body_idx")` index (0 = original, 1 = first
    /// swap, …). This is what lets the *single* parameterized whole-document
    /// source keep rendering the transformed content after a morph — without it
    /// the base document would freeze on the original body and the formula would
    /// snap back to its pre-transform state at the end of every `#transform`
    /// (and, for chained transforms, the intermediate steps would never persist).
    ///
    /// The selection mirrors [`crate::renderer::typst::content::content_for`]:
    /// the latest timeline entry with `t <= frame` wins. Each branch gets the
    /// `ecval(..)` → input rewrite so counter reads work inside swapped bodies
    /// too. Returns `None` (leaving `inner` untouched) when the label has no
    /// timeline entries.
    fn content_selection_body(label: &Label, original: &str, scene: &Scene) -> Option<String> {
        let timeline = scene.content_timeline.get(label)?;
        if timeline.is_empty() {
            return None;
        }
        let mut branches: Vec<String> = Vec::with_capacity(timeline.len() + 1);
        branches.push(Self::ecval_to_inputs(original));
        for (_, b) in timeline {
            branches.push(Self::ecval_to_inputs(b));
        }
        let key = format!("candy:{}:body_idx", label.0);
        let mut s = String::new();
        s.push_str(&format!(
            "( if sys.inputs.at(\"{key}\", default: 0) == 0 {{ {} }}",
            branches[0]
        ));
        for (i, b) in branches.iter().enumerate().skip(1) {
            s.push_str(&format!(
                " else if sys.inputs.at(\"{key}\", default: 0) == {i} {{ {b} }}"
            ));
        }
        s.push_str(&format!(" else {{ {} }} )", branches.last().unwrap()));
        Some(s)
    }

    /// Build the set of *all* descendant scene ids for every scene, from the
    /// scene tree (`SceneInfo.parent` links). Used by the Rust-managed gating
    /// wrapper: when a descendant is the active scene, the ancestor wrapper emits
    /// that descendant's (already-wrapped) `page()` call instead of its own —
    /// so a nested child renders without a page-in-page.
    fn build_descendants(scenes: &[SceneInfo]) -> HashMap<usize, Vec<usize>> {
        let mut children: HashMap<usize, Vec<usize>> = HashMap::new();
        for s in scenes {
            if let Some(p) = s.parent {
                children.entry(p).or_default().push(s.id);
            }
        }
        let mut out: HashMap<usize, Vec<usize>> = HashMap::new();
        for s in scenes {
            let mut stack = children.get(&s.id).cloned().unwrap_or_default();
            let mut acc = Vec::new();
            while let Some(id) = stack.pop() {
                acc.push(id);
                if let Some(c) = children.get(&id) {
                    stack.extend(c.iter().copied());
                }
            }
            if !acc.is_empty() {
                out.insert(s.id, acc);
            }
        }
        out
    }

    /// Depth of each scene in the scene tree (root = 0). Used to process scenes
    /// innermost-first when building the gating wrappers.
    fn scene_depth_map(scenes: &[SceneInfo]) -> HashMap<usize, usize> {
        let parent_of: HashMap<usize, usize> = scenes
            .iter()
            .filter_map(|s| s.parent.map(|p| (s.id, p)))
            .collect();
        let mut depth: HashMap<usize, usize> = HashMap::new();
        for s in scenes {
            let mut d = 0;
            let mut cur = s.id;
            while let Some(p) = parent_of.get(&cur) {
                d += 1;
                cur = *p;
            }
            depth.insert(s.id, d);
        }
        depth
    }

    /// Find the byte span of the `k`-th `#scene(...)` call (in source order) in
    /// `src`. Used to locate each scene call so it can be wrapped by the
    /// Rust-managed gating logic.
    fn find_kth_scene_call(src: &str, k: usize) -> Option<(usize, usize)> {
        let root = typst_syntax::parse(src);
        let node = LinkedNode::new(&root);
        let mut spans: Vec<(usize, usize)> = Vec::new();
        Self::collect_scene_call_spans(&node, &mut spans);
        spans.get(k).copied()
    }

    fn collect_scene_call_spans(node: &LinkedNode, spans: &mut Vec<(usize, usize)>) {
        if let Some(call) = node.get().cast::<ast::FuncCall>() {
            if let Expr::Ident(id) = call.callee() {
                if id.as_str() == "scene" {
                    let r = node.range();
                    spans.push((r.start, r.end));
                }
            }
        }
        for child in node.children() {
            Self::collect_scene_call_spans(&child, spans);
        }
    }

    /// Build the per-frame `sys.inputs` dictionary for the whole-document path.
    ///
    /// `hide_fading` controls whether opacity < 1 objects get a `…:hide` flag
    /// (the pixel path draws them via the opacity overlay; the SVG draft shows
    /// them at full opacity, so it passes `false`).
    pub(crate) fn build_frame_inputs(
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
                // A mobject owned by a non-active scene (e.g. a parent scene
                // whose child is currently active) must be hidden so the active
                // scene visually replaces it ("parent auto-hide"). We emit
                // `hide` (resolved to `none` by `wrap_mobject_inputs`) rather
                // than skipping the input entirely, because the mobject's body
                // is still reached when an ancestor scene is rendered — leaving
                // it at its default transform would show it on top of the
                // active scene.
                inputs.insert(format!("candy:{}:hide", label.0).into(), Value::Bool(true));
                continue;
            }
            // Cross-page scenes: only draw mobjects that landed on the page
            // currently playing. Mobjects without a recorded `page_of` (e.g.
            // those absent from the flow layout) are drawn on every page.
            if let Some(p) = self.pages.page_of(label) {
                if p != active_page {
                    continue;
                }
            }
            let l = &label.0;
            if self.transform_hidden(label, time_ms) {
                // The target/old mobjects are replaced by the interpolated
                // per-glyph fragments, so hide them in the base document.
                inputs.insert(format!("candy:{}:hide", label.0).into(), Value::Bool(true));
                continue;
            }
            // Position model (cm, matching `tuple_cm` / `st` units): `#move` is a
            // *relative* transform, so the input is the delta from the mobject's
            // flow position. An un-positioned mobject (`st` still (0, 0))
            // gets `(0, 0)` and stays exactly where plain Typst laid it; a
            // positioned one (`#animate`/`#track`/… set `st` to the absolute
            // `to:` in cm) gets `target − flow_pos` so the native `move` lands it on
            // its absolute eased target. `scale` / `rotate` are absolute and are
            // read straight from `st`. (Opacity is intentionally NOT a native
            // transform here — opacity changes are composited via the SVG bypass
            // in the raster path, not written into the document.)
            let (dx, dy) = match self.flow_pos.get(label) {
                Some((nx, ny)) => {
                    let (nx_cm, ny_cm) = (nx / PT_PER_CM, ny / PT_PER_CM);
                    if st.x.abs() < 1e-9 && st.y.abs() < 1e-9 {
                        (0.0, 0.0)
                    } else {
                        (st.x - nx_cm, st.y - ny_cm)
                    }
                }
                None => (st.x, st.y),
            };
            inputs.insert(format!("candy:{l}:dx").into(), Value::Float(dx));
            inputs.insert(format!("candy:{l}:dy").into(), Value::Float(dy));
            inputs.insert(
                format!("candy:{l}:s").into(),
                Value::Float(st.scale * 100.0),
            );
            inputs.insert(format!("candy:{l}:r").into(), Value::Float(st.rotation));
            if hide_fading && st.opacity < 1.0 - 1e-4 {
                inputs.insert(format!("candy:{}:hide", label.0).into(), Value::Bool(true));
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
        // `reveal`/`typewriter` revealed-prefix lengths and non-string `#transform`
        // body swaps: string targets are driven by `candy:<label>:reveal:len`,
        // formula/shape targets by `candy:<label>:body_idx`. Both inputs are
        // consumed by the corresponding wrappers in `build_parameterized_source`.
        for label in self.scene.content_timeline.keys() {
            if let Some(full) = self
                .scene
                .items
                .get(label)
                .and_then(|b| strip_string_literal(b))
            {
                let len = Self::reveal_len_at(&self.scene, label, time_ms, full.chars().count());
                inputs.insert(
                    format!("candy:{}:reveal:len", label.0).into(),
                    Value::Int(len as i64),
                );
            } else {
                let idx = Self::body_idx_at(&self.scene, label, time_ms);
                inputs.insert(
                    format!("candy:{}:body_idx", label.0).into(),
                    Value::Int(idx as i64),
                );
            }
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

    /// The active `content_timeline` index for `label` at `time_ms`: `0` = the
    /// original body, `1` = the first swap, … (the count of timeline entries
    /// with `t <= time_ms`). Mirrors the latest-wins selection in
    /// [`crate::renderer::typst::content::content_for`] so the whole-document
    /// path and the legacy path agree on which body is current.
    fn body_idx_at(scene: &Scene, label: &Label, time_ms: u32) -> usize {
        let mut idx = 0usize;
        if let Some(timeline) = scene.content_timeline.get(label) {
            for (t, _) in timeline {
                if *t <= time_ms {
                    idx += 1;
                }
            }
        }
        idx
    }
    /// Build a minimal whole-document Typst source for a hand-built `Scene` that
    /// has no parsed `.tyx` (`artifacts.source` is empty). Each declared mobject
    /// becomes a `#mobject(label, <body>)` call; the returned `mobject_body` map
    /// records the byte range of each `<body>` within the source so the
    /// per-frame whole-document recompiler (`build_parameterized_source`) can
    /// splice the wrapped body back in. This keeps hand-built scenes on the same
    /// single whole-document render path as parsed `.tyx` files — they can drive
    /// transform body swaps, reveals, and `ecval` counters through `sys.inputs`
    /// exactly like real documents.
    pub(crate) fn synthesize_handbuilt_source(
        scene: &Scene,
    ) -> (String, HashMap<Label, (usize, usize)>) {
        let v = crate::CANDY_VERSION;
        let mut src = format!("#import \"@preview/candy:{v}\": *\n\n");
        // Wrap mobjects in a `#scene(...)` so the page size matches what the
        // renderer expects (margin: 0pt, the scene's declared width/height or
        // the 16:9 default). Without this, Typst's default page (A4 with
        // margins) would be used and the introspector positions would not
        // match the renderer's canvas.
        let (pw_cm, ph_cm) = scene
            .page_size
            .map(|(w, h)| (w / PT_PER_CM, h / PT_PER_CM))
            .unwrap_or((16.0, 9.0));
        src.push_str(&format!("#scene(width: {pw_cm}cm, height: {ph_cm}cm)[\n"));
        // Emit mobjects in declaration order (from the first scene's
        // `owns_labels`) so the flow layout matches the intended top-to-bottom
        // stacking. `scene.items` is a HashMap with non-deterministic iteration
        // order, so we must NOT iterate it directly.
        let ordered_labels: Vec<Label> = scene
            .scenes
            .first()
            .map(|s| s.owns_labels.clone())
            .unwrap_or_else(|| scene.items.keys().cloned().collect());
        let mut mobject_body = HashMap::new();
        for label in &ordered_labels {
            let Some(body) = scene.items.get(label) else {
                continue;
            };
            // Build the `  #mobject("label", <body>)` line piece by piece so we
            // can record the exact byte range of `<body>` for the per-frame
            // recompiler to splice the wrapped body back in.
            src.push_str("  #mobject(\"");
            src.push_str(&label.0);
            src.push_str("\", ");
            let body_start = src.len();
            src.push_str(body);
            let body_end = src.len();
            src.push_str(")\n");
            mobject_body.insert(label.clone(), (body_start, body_end));
        }
        src.push_str("]\n");
        (src, mobject_body)
    }
}
