use super::*;

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
}
