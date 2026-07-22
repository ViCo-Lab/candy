use super::*;
use crate::core::ast::SceneInfo;
use std::collections::HashSet;

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
    /// Build the stable *parameterized* whole-document source (used for the
    /// flow-measurement pass) **and** the per-mobject wrapped-body map.
    ///
    /// The whole-document `String` is still compiled once per scene during
    /// [`Renderer::ensure_flow`] to read each mobject's flow position and which
    /// page it landed on (the "measurement" pass). The wrapped-body `HashMap`
    /// (`label → sys.inputs`-driven body expression) is the building block for
    /// the *per-page* render documents: [`Renderer::assemble_page_doc`] stitches
    /// the wrapped bodies of a single (scene, page) into a standalone Typst
    /// document that is laid out from the top in raw flow ("裸排") and compiled
    /// independently — this is the replacement for the old whole-document
    /// render path.
    pub(crate) fn build_parameterized_source(scene: &Scene) -> (String, HashMap<Label, String>) {
        let src = &scene.artifacts.source;
        let chars: Vec<char> = src.chars().collect();
        // `(char_start, char_end, replacement)` — `start == end` is an insertion.
        let mut edits: Vec<(usize, usize, String)> = Vec::new();
        // Per-mobject wrapped body, collected so the per-page render documents
        // can be assembled without re-parsing the whole source.
        let mut wrapped_bodies: HashMap<Label, String> = HashMap::new();
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
            let wrapped = Self::wrap_mobject_inputs(&label.0, &inner);
            wrapped_bodies.insert(label.clone(), wrapped.clone());
            edits.push((cs, ce, wrapped));
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
        (cur, wrapped_bodies)
    }

    /// Build, for every scene, the **accumulated Typst context** that the
    /// scene's mobjects must see when rendered as a standalone per-page
    /// document — i.e. the full chain of *ancestor* contexts, not just the
    /// immediate parent's page size / background.
    ///
    /// In the old whole-document path the full `.tyx` source was compiled, so a
    /// nested scene's mobjects naturally inherited every `#import`, `#set` /
    /// `#show` rule, helper `#let` definition, and top-level content established
    /// by their ancestor scenes (and the document root). The per-page path
    /// compiles a *standalone* document containing only that page's `#mobject`
    /// calls, so all of that context would otherwise be lost. This function
    /// extracts it by walking the parse tree and keeping, for scene `sid`:
    ///
    /// * the document-root context (everything outside any `#scene` call), minus
    ///   `#mobject` / `#subtitle` calls (those are re-emitted per page) and minus
    ///   `#scene` calls that are *not* ancestors of `sid`;
    /// * for each ancestor scene (root → … → immediate parent), that ancestor's
    ///   body context (its `#set` / `#show` / `#let` / content), minus its own
    ///   `#mobject` calls and minus any nested `#scene` call that is not on the
    ///   ancestor chain.
    ///
    /// The result is keyed by scene id (`0` for the no-scene-tree / hand-built
    /// case). Each value is a flat Typst source fragment that, prepended before
    /// the `#set page(...)` and the `#mobject(...)` calls, reproduces exactly the
    /// environment the mobjects had inside the whole document.
    pub(crate) fn build_scene_contexts(
        scene: &Scene,
        wrapped: &HashMap<Label, String>,
    ) -> HashMap<usize, String> {
        let mut map: HashMap<usize, String> = HashMap::new();
        let src = &scene.artifacts.source;
        if src.is_empty() {
            // Hand-built / programmatic scene: no user source → no context.
            map.insert(0, String::new());
            return map;
        }
        let root = typst_syntax::parse(src);
        let node = LinkedNode::new(&root);
        // `(call_start, call_end) → scene id` for every `#scene(...)` call.
        let call_map: HashMap<(usize, usize), usize> = scene
            .artifacts
            .scene_call
            .iter()
            .map(|(id, (s, e))| ((*s, *e), *id))
            .collect();
        // Rewrite file-style candy imports to the canonical `@preview` form so
        // the World resolves them in-process (mirrors `build_parameterized_source`).
        let v = crate::CANDY_VERSION;
        let rewrite = |mut c: String| -> String {
            c = c.replace(
                "#import \"candy\":",
                &format!("#import \"@preview/candy:{v}\":"),
            );
            c.replace(
                "#import \"candy\"",
                &format!("#import \"@preview/candy:{v}\""),
            )
        };
        if scene.scenes.is_empty() {
            // No scene tree: context for sid 0 = root minus mobjects/subtitles,
            // with `#play(...)` rewritten to its inline controlled mobject.
            let keep: HashSet<usize> = HashSet::new();
            let mut edits: Vec<(usize, usize, Option<String>)> = Vec::new();
            Self::collect_skipped(src, &node, &keep, &call_map, &mut edits, scene, wrapped);
            let ctx = Self::emit_minus_skipped(src, node.range(), &edits);
            map.insert(0, rewrite(ctx));
            return map;
        }
        for s in &scene.scenes {
            let keep: HashSet<usize> = Self::ancestor_chain(scene, s.id).into_iter().collect();
            let mut edits: Vec<(usize, usize, Option<String>)> = Vec::new();
            Self::collect_skipped(src, &node, &keep, &call_map, &mut edits, scene, wrapped);
            let ctx = Self::emit_minus_skipped(src, node.range(), &edits);
            map.insert(s.id, rewrite(ctx));
        }
        map
    }

    /// All ancestor scene ids of `id` (including `id` itself), walking `parent`
    /// links up to the document root. Used to decide which `#scene` calls stay in
    /// a scene's injected context.
    fn ancestor_chain(scene: &Scene, id: usize) -> Vec<usize> {
        let mut chain = Vec::new();
        let mut cur = Some(id);
        while let Some(c) = cur {
            chain.push(c);
            cur = scene
                .scenes
                .iter()
                .find(|s| s.id == c)
                .and_then(|s| s.parent);
        }
        chain
    }

    /// Collect the byte-range *edits* that turn `node`'s source into the injected
    /// context for a scene whose ancestor chain is `keep`.
    ///
    /// * A `#scene(...)` call **on** the ancestor chain is kept, but only its inner
    ///   body — its `#scene(name, w, h, bg, …)` wrapper (everything outside the body)
    ///   is subtracted, and we recurse into the body to drop any `#mobject` /
    ///   `#subtitle` / non-ancestor `#scene` calls inside it.
    /// * A `#scene(...)` call **not** on the chain is subtracted whole.
    /// * `#mobject(...)` and `#subtitle(...)` calls are subtracted whole (they are
    ///   re-emitted per page / drawn as overlays).
    /// * A `#play(...)` call is **replaced** (not dropped) by its inline controlled
    ///   `#mobject("__block_N", …)` at the call site, so the play content is drawn
    ///   by the synthetic block mobject (correctly hidden / faded by its animation)
    ///   exactly where `#play` appeared — see `assemble_page_doc` which then skips
    ///   re-appending `__block_N`. This fixes the two play bugs: the mask was being
    ///   appended at the page end and drifted off the call site ("play 遮罩和内容
    ///   错位"), and the literal `block(body)` emitted by `#play` was always
    ///   rendered, ignoring the block mobject's opacity ("遮罩开始播放时原始内容没有
    ///   隐藏"). Each edit is `(start, end, None)` to remove or
    ///   `(start, end, Some(replacement))` to substitute.
    fn collect_skipped(
        src: &str,
        node: &LinkedNode,
        keep: &HashSet<usize>,
        call_map: &HashMap<(usize, usize), usize>,
        out: &mut Vec<(usize, usize, Option<String>)>,
        scene: &Scene,
        wrapped: &HashMap<Label, String>,
    ) {
        // A Typst code expression `#expr` is parsed so the `FuncCall` (etc.) node
        // range starts at the callee identifier and *excludes* the leading `#`.
        // When we subtract a call's range we must also back up over that `#`,
        // otherwise the dangling `#` leaks into the context and breaks the
        // standalone document (e.g. a `#scene` body left as `#[ … ]`).
        let with_hash = |cs: usize| -> usize {
            if cs > 0 && src.as_bytes().get(cs - 1) == Some(&b'#') {
                cs - 1
            } else {
                cs
            }
        };
        // `#scene(...)` call?
        if let Some(id) = Self::scene_id_of(node, call_map) {
            if keep.contains(&id) {
                // Keep only the inner body; subtract the surrounding wrapper.
                if let Some(body) = Self::scene_body_node(node) {
                    let cs = with_hash(node.range().start);
                    let ce = node.range().end;
                    let (bs, be) = (body.range().start, body.range().end);
                    // Strip the block delimiters so ancestor set rules apply to
                    // the re-emitted mobjects (which live *outside* this block).
                    // A scene body is normally a markup content block `[ … ]`,
                    // but guard for a code block `{ … }` too.
                    let (ibs, ibe) = if src.as_bytes().get(bs) == Some(&b'[')
                        && src.as_bytes().get(be.wrapping_sub(1)) == Some(&b']')
                    {
                        (bs + 1, be - 1)
                    } else {
                        (bs, be)
                    };
                    out.push((cs, ibs, None));
                    out.push((ibe, ce, None));
                    for child in body.children() {
                        Self::collect_skipped(src, &child, keep, call_map, out, scene, wrapped);
                    }
                }
            } else {
                let cs = with_hash(node.range().start);
                out.push((cs, node.range().end, None));
            }
            return;
        }
        // `#play(...)` call? Rewrite it inline to its controlled block mobject at
        // the call site so the content lands where `#play` appeared and obeys the
        // block's animation (hidden until its `FadeIn`, hidden again after).
        if Self::callee_is(node, "play") {
            let cs = with_hash(node.range().start);
            let ce = node.range().end;
            // The block mobject is the `__block_N` whose recorded body range lies
            // inside this `#play` call (either the explicit `#mobject("__block_N", …)`
            // passed to `#play`, or the synthetic one `process_play` created).
            let block_label = scene
                .artifacts
                .mobject_body
                .iter()
                .find(|(l, (bs, be))| l.0.starts_with("__block_") && *bs >= cs && *be <= ce)
                .map(|(l, _)| l.clone());
            if let Some(label) = block_label {
                if let Some(w) = wrapped.get(&label) {
                    out.push((cs, ce, Some(format!("#mobject(\"{}\", {})", label.0, w))));
                    return;
                }
            }
            // No matching synthetic block — drop the call so nothing leaks.
            out.push((cs, ce, None));
            return;
        }
        // `#mobject(...)` / `#subtitle(...)` call?
        if Self::callee_is(node, "mobject") || Self::callee_is(node, "subtitle") {
            let cs = with_hash(node.range().start);
            out.push((cs, node.range().end, None));
            return;
        }
        for child in node.children() {
            Self::collect_skipped(src, &child, keep, call_map, out, scene, wrapped);
        }
    }

    /// Emit `src[range]` with every edit applied: ranges mapped to `None` are
    /// removed, ranges mapped to `Some(replacement)` are substituted. Edits are
    /// applied right-to-left (descending start) so earlier offsets stay valid.
    fn emit_minus_skipped(
        src: &str,
        range: std::ops::Range<usize>,
        edits: &[(usize, usize, Option<String>)],
    ) -> String {
        let mut es: Vec<(usize, usize, Option<String>)> = edits
            .iter()
            .filter(|(s, e, _)| *e > *s && *e > range.start && *s < range.end)
            .map(|(s, e, t)| ((*s).max(range.start), (*e).min(range.end), t.clone()))
            .collect();
        es.sort_by_key(|b| std::cmp::Reverse(b.0));
        let mut out: Vec<u8> = src[range.clone()].as_bytes().to_vec();
        for (s, e, t) in es {
            let (s, e) = (s - range.start, e - range.start);
            let rep = t.unwrap_or_default();
            out.splice(s..e, rep.into_bytes());
        }
        String::from_utf8(out).unwrap_or_default()
    }

    /// If `node` is a `#scene(...)` call, return its registered scene id.
    fn scene_id_of(node: &LinkedNode, call_map: &HashMap<(usize, usize), usize>) -> Option<usize> {
        let call = node.get().cast::<ast::FuncCall>()?;
        if let Expr::Ident(id) = call.callee() {
            if id.as_str() == "scene" {
                let r = node.range();
                return call_map.get(&(r.start, r.end)).copied();
            }
        }
        None
    }

    /// Whether `node` is a `#<name>(...)` call.
    fn callee_is(node: &LinkedNode, name: &str) -> bool {
        if let Some(call) = node.get().cast::<ast::FuncCall>() {
            if let Expr::Ident(id) = call.callee() {
                return id.as_str() == name;
            }
        }
        false
    }

    /// The body expression node of a `#scene(...)` call (its last positional arg).
    fn scene_body_node<'a>(node: &'a LinkedNode) -> Option<LinkedNode<'a>> {
        let args_node = node
            .children()
            .find_map(|c| c.get().cast::<ast::Args>().map(|_| c))?;
        let mut body_arg: Option<LinkedNode<'a>> = None;
        for arg in args_node.children() {
            if let Some(a) = arg.get().cast::<ast::Arg>() {
                if matches!(a, ast::Arg::Pos(_)) {
                    body_arg = Some(arg.clone());
                }
            }
        }
        let arg = body_arg?;
        // `arg` is the `Arg::Pos` wrapping the scene's trailing content block
        // `[ … ]`. Its range covers the *entire* block (including the `[` and
        // `]` delimiters), so callers can strip those delimiters and re-emit the
        // inner markup at top level. Returning `arg.children().next()` would
        // hand back the bare `LeftBracket` token (a 1-byte node), which makes
        // the delimiter-stripping guard fail and leaks a stray `[` into the
        // re-emitted document (often pairing with a neighbour to form `[[`/`]]`
        // garbage).
        Some(arg)
    }

    /// Build the candy **scene runtime context** preamble for scene `sid`: a
    /// standalone Typst document header that injects the scene's page size,
    /// background, and (implicitly, via the global `sys.inputs`) its counters
    /// and `active_scene` — **plus the full chain of ancestor Typst contexts**.
    ///
    /// The preamble is: `[candy import if absent] + [#set page(...)] +
    /// [accumulated ancestor context]`. The `#set page(...)` comes *first* so the
    /// candy canvas is established before the ancestor context is applied (an
    /// ancestor `#set`/`#show`/`#let` rule or piece of content then lands on the
    /// candy page, not on Typst's default A4 page). The accumulated ancestor
    /// context (`self.scene_contexts[sid]`, built in
    /// [`Renderer::build_scene_contexts`]) is the document-root context followed
    /// by every ancestor scene's body context (its `#import` / `#set` / `#show` /
    /// `#let` / content), so a nested sub-scene renders with *all* of its
    /// parents' Typst environment — not just the immediate parent's page size /
    /// background. This is the "sub-scene rendering injects the current scene's
    /// (and every ancestor's) context" requirement.
    pub(crate) fn scene_context_preamble(&self, sid: usize) -> String {
        let v = crate::CANDY_VERSION;
        let (pw_pt, ph_pt) = if self.scene.scenes.is_empty() {
            (self.page_w, self.page_h)
        } else {
            self.scene.effective_page_pt(sid)
        };
        let pw_cm = pw_pt / PT_PER_CM;
        let ph_cm = ph_pt / PT_PER_CM;
        let bg = if self.scene.scenes.is_empty() {
            "white".to_string()
        } else {
            self.scene_bg_hex(sid)
                .unwrap_or_else(|_| "white".to_string())
        };
        // `scene_bg_hex` returns either a named colour (`white`) or a `#rrggbb(aa)`
        // hex. A bare `#…` is invalid in code mode (the `#` is a code escape), so
        // emit a valid Typst paint: a named colour as-is, a hex via `rgb("…")`.
        let bg_expr = if bg.starts_with('#') {
            format!("rgb(\"{bg}\")")
        } else {
            bg.clone()
        };
        let ctx = self.scene_contexts.get(&sid).cloned().unwrap_or_default();
        let mut preamble = String::new();
        // Prepend the candy import only if the accumulated context doesn't
        // already import it (the context built in `build_scene_contexts` is
        // rewritten to the canonical `@preview/candy:{v}` form, so check for
        // that exact string — a bare substring `"candy"` would false-positive on
        // user identifiers / comments and wrongly skip the import, leaving every
        // `#mobject` undefined and the page blank).
        if !ctx.contains(&format!("@preview/candy:{v}")) {
            preamble.push_str(&format!("#import \"@preview/candy:{v}\": *\n"));
        }
        preamble.push_str(&format!(
            "#set page(width: {pw_cm}cm, height: {ph_cm}cm, margin: 0pt, fill: {bg_expr})\n",
            pw_cm = pw_cm,
            ph_cm = ph_cm,
            bg_expr = bg_expr,
        ));
        preamble.push_str(&ctx);
        preamble
    }

    /// Assemble the standalone per-page render document for `(sid, page)`: the
    /// scene's injected context preamble followed by only the mobjects that
    /// belong to `sid` and landed on `page`, each laid out from the top in raw
    /// Typst flow ("裸排"), in declaration order. Mobjects with no recorded page
    /// (absent from the flow layout) are emitted on every page so they keep
    /// rendering exactly as they did under the whole-document path.
    pub(crate) fn assemble_page_doc(&self, sid: usize, page: usize) -> String {
        let mut doc = self.scene_context_preamble(sid);
        if std::env::var("CANDY_DBG_DOC").is_ok() {
            eprintln!("DBG DOC sid={sid} page={page}:\n<<<\n{doc}\n>>>");
        }
        let label_scene = self.scene.label_scene_map();
        let owns: Vec<Label> = if self.scene.scenes.is_empty() {
            self.scene.items.keys().cloned().collect()
        } else {
            self.scene
                .scenes
                .iter()
                .find(|s| s.id == sid)
                .map(|s| s.owns_labels.clone())
                .unwrap_or_default()
        };
        for label in &owns {
            let Some(wrapped) = self.wrapped_bodies.get(label) else {
                continue;
            };
            // `#play` blocks are no longer appended here: they are rewritten
            // inline (at their `#play` call site) in the scene context by
            // `collect_skipped`, so re-emitting them would duplicate the
            // content and drift it back to the page end ("play 遮罩和内容错位").
            if label.0.starts_with("__block_") {
                continue;
            }
            // Page filter: skip mobjects that landed on a different page.
            if let Some(p) = self.pages.page_of(label) {
                if p != page {
                    continue;
                }
            }
            // Scene ownership filter (only for real scene trees; hand-built
            // scenes have no tree and own every mobject).
            if !self.scene.scenes.is_empty() && label_scene.get(label).copied().unwrap_or(0) != sid
            {
                continue;
            }
            doc.push_str(&format!("#mobject(\"{}\", {})\n", label.0, wrapped));
        }
        if std::env::var("CANDY_DBG_DOC").is_ok() {
            eprintln!("DBG FULLDOC sid={sid} page={page}:\n<<<\n{doc}\n>>>");
        }
        doc
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
