// Candy — core directives.
//
// Every directive defined here is *valid, standard Typst*. Under a plain
// `typst compile` it renders the **first frame** of the animation: each
// `mobject` at its natural placement in the document flow, every `play` block
// visible, and `animate` / `pause` / `audio` simply inert. The Candy Rust
// toolchain reads the same directives from the source's **AST** (not the
// rendered output) and produces the full video.

/// Register an animatable object ("mobject").
///
/// - `label`: unique string id, referenced later by `animate` / `play`.
/// - `body`: the object's content — a bare block or element (e.g.
///   `circle(radius: 1cm)`), *not* a string. Its placement is automatic.
///
/// Under standard Typst this simply renders `body` at its natural position.
#let mobject(label, body) = block(body)

// Candy — scene definition.
//
// `scene` sets the canvas size / background for a group of content and is the
// primary way to organize a `.tyx` into slides.

/// Define a scene (a "slide") with a specific page size and background.
///
/// In standard Typst, `scene` sets the page and renders the body. In candy's
/// animation pipeline, `scene` is a semantic marker that groups content into
/// a slide; the page size is also used by the renderer as the canvas size for
/// every frame in this scene.
///
/// - `width`: page width (default `16cm` — standard 16:9 slide width).
/// - `height`: page height (default `9cm`).
/// - `bg`: background fill (default `white`).
/// - `body`: the scene's content.
///
/// *Scene semantics*
///
/// - *Nesting.* Scenes may be nested: a `scene` call *inside* another scene's
///   `body` creates a child scene. Nesting is detected through the Typst AST,
///   so the usual `#import` rules apply.
/// - *Parent auto-hide.* When the timeline enters a child scene, its parent
///   (and any ancestor) is automatically hidden for the duration of the child.
///   The renderer shows only the **deepest** scene that is active at each frame
///   time, so a child scene visually replaces its parent.
/// - *Typst scope.* Scene membership follows Typst's lexical scope: a mobject /
///   `play` / `subtitle` belongs to the innermost `scene` whose `body` encloses
///   it. This is exactly the scope in which the call is evaluated.
/// - *One page per scene.* A scene occupies **one page** (one canvas); its
///   `width`/`height` set the frame size. Content that would overflow the page
///   is warned about and should be split into additional scenes.
/// - *Auto-split.* Content spanning multiple pages is automatically split into
///   multiple scenes (one per page) when no explicit root `scene` wraps it.
/// - *Implicit root.* If you never call `scene`, the whole document is treated
///   as a single implicit root scene following the same one-page / split rules
///   (default canvas 16cm × 9cm). A scene's page size is inherited from the
///   nearest ancestor that declares one.
///
/// Call `scene` at the top of your `.tyx` to set the canvas size. Without it,
/// candy defaults to 16cm × 9cm.
#let scene(width: 16cm, height: 9cm, bg: white, body) = {
  page(width: width, height: height, margin: 0pt, fill: bg, body)
}

/// Animate an object to a new placement / scale / rotation / opacity over
/// `duration` milliseconds.
///
/// Absolute transforms:
/// - `to`: an absolute target point `(x, y)` (lengths, e.g. `(4cm, 0pt)`).
/// - `scale`: an absolute scale factor (e.g. `1.5`).
/// - `rotate`: an absolute clockwise rotation in degrees (e.g. `45`).
/// - `opacity`: a target opacity in `[0, 1]`.
///
/// Relative transforms (Manim-style `shift` / `scale` / `rotate`):
/// - `dx`, `dy`: relative offset in cm (e.g. `dx: 2cm` moves right 2cm from
///   the current position). Either or both may be given.
/// - `scale-by`: relative scale multiplier (e.g. `1.5` grows by 50%).
/// - `rotate-by`: relative rotation in degrees (e.g. `15` adds 15° to the
///   current rotation).
///
/// - `duration`: number of milliseconds the animation spans (default `500`).
/// - `easing`: a string naming the rate curve (default `"linear"`). One of:
///   `"linear"`, `"smooth"`, `"smoothstep"`, `"smootherstep"`,
///   `"quad-in"` / `"quad-out"` / `"quad-in-out"`,
///   `"cubic-in"` / `"cubic-out"` / `"cubic-in-out"` (aliases: `"ease-in"`,
///   `"ease-out"`, `"ease-in-out"`),
///   `"sin"` (sine ease-out), `"there-and-back"`, `"wiggle"`, `"lingering"`.
///   Unknown names fall back to `linear` with a warning.
///
/// Absolute and relative transforms may be combined in one `animate` call:
/// each produces a separate action that animates in parallel over the slide's
/// duration. Inert under standard Typst (returns `none`).
///
/// The trailing `..` argument sink tolerates the intuitive aliases the Candy
/// DSL parser also accepts (e.g. `x:` / `y:` for `dx:` / `dy:`) so a source
/// using them still compiles cleanly through the whole-document render path.
#let animate(
  target,
  to: none,
  dx: none,
  dy: none,
  scale: none,
  scale-by: none,
  rotate: none,
  rotate-by: none,
  opacity: none,
  duration: 500,
  easing: "linear",
  ..,
) = none

/// Hold the current frame for `duration` milliseconds (default `500`, a manual pause marker).
/// Inert under standard Typst.
#let pause(duration: 500) = none

/// Insert a voice / audio track. Inert under standard Typst (does nothing).
///
/// - `path`: audio file (`.opus`/`.ogg` for WebM/MKV, `.aac` for MP4).
/// - `blocking`: if `true`, the timeline waits for the clip to finish.
/// - `loop`: repeat the clip.
/// - `volume`: gain in `[0, 1]`.
/// - `slice`: optional `(start, end)` seconds sub-range of the clip.
#let audio(
  path,
  blocking: false,
  loop: false,
  volume: 1.0,
  slice: none,
) = none

/// Show `body` for `duration` milliseconds (default `500`) as its own animation unit (a block-level
/// object, precisely controllable like a mobject).
///
/// Under standard Typst the body is shown in the first frame.
#let play(body, duration: 500) = block(body)
