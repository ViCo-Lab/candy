// Candy — core directives.
//
// Every directive defined here is *valid, standard Typst*. Under a plain
// `typst compile` it renders the **first frame** of the animation: each
// `mobject` at its natural placement in the document flow, every `play` block
// visible, and `animate` / `pause` / `audio` simply inert. The Candy Rust
// toolchain reads the same directives from the source's **AST** (not the
// rendered output) and produces the full video.

#import "validation.typ": *

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
/// - `name`: optional human-readable name for scene switching (e.g., `"intro"`,
///   `"demo"`). Named scenes can be targeted directly via `#scene-switch(target: "name")`.
///   Anonymous scenes (without a `name:`) are auto-assigned UUID-like names
///   (e.g., `"scene_00000000"`) so they can still be referenced by `#scene-switch`.
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
/// - *Named scenes & switching.* Use `#scene(name: "foo", ...)` to give a scene
///   a human-readable name. Then use `#scene-switch(target: "foo")` to jump the
///   timeline cursor to that scene. Anonymous scenes get auto-assigned UUID
///   names (e.g., `"scene_a1b2c3d4"`) which can also be targeted.
///
/// Call `scene` at the top of your `.tyx` to set the canvas size. Without it,
/// candy defaults to 16cm × 9cm.
#let scene(name: none, width: 16cm, height: 9cm, bg: white, body) = {
  if name != none and type(name) != str {
    panic("scene name must be a string")
  }
  _assert_length(width, "scene width")
  _assert_length(height, "scene height")
  page(width: width, height: height, margin: 0pt, fill: bg, body)
}

/// Switch to a named scene by `target` (the scene's `name:` value or UUID-like
/// auto-assigned name). Optionally animate the transition over `duration` ms
/// with an `easing` curve.
///
/// - `target` / `name`: the scene name to switch to (required).
/// - `duration`: transition duration in ms (default `0`, instant jump).
/// - `easing`: easing curve string (default `"smooth"`).
///
/// Named scenes are defined via `#scene(name: "foo", ...)`. Anonymous scenes
/// receive auto-assigned names like `"scene_00000000"`.
///
/// Under standard Typst this is inert (returns `none`). In candy's animation
/// pipeline it jumps the timeline cursor to the target scene's start time.
#let scene-switch(target, duration: 0, easing: "smooth") = {
  _assert_str(target, "Scene-switch target")
  _assert_nonneg(duration, "duration")
  _assert_easing(easing, "easing")
  none
}

/// Mark a slide transition (a "cut" between scenes). Semantically, this is a
/// boundary marker; candy inserts a brief blank frame or crossfade between
/// the preceding and following content.
///
/// - `kind`: transition style — `"cut"` (instant, default), `"fade"` (crossfade),
///   `"slide"` (push). Only `"cut"` is fully implemented; others are recorded
///   for future versions.
/// - `duration`: number of milliseconds for the transition (default `100`).
///
/// Inert under standard Typst.
#let transition(kind: "cut", duration: 100) = {
  _assert_enum(kind, ("cut", "fade", "slide"), "transition kind")
  _assert_nonneg(duration, "duration")
  none
}

/// Register an animatable object ("mobject").
///
/// - `name`: unique string id, referenced later by `animate` / `play`.
/// - `body`: the object's content — a bare block or element (e.g.
///   `circle(radius: 1cm)`), *not* a string. Its placement is automatic.
///
/// Under standard Typst this simply renders `body` at its natural position.
#let mobject(name, body) = {
  if type(name) != str {
    panic("Mobject name must be a string")
  }
  [#body]
}

/// Animate an object to a new placement / scale / rotation / opacity over
/// `duration` milliseconds.
///
/// Absolute transforms:
/// - `to`: an absolute target point `(x, y)` (lengths, e.g. `(4cm, 0pt)`).
/// - `scale`: an absolute scale factor (e.g. `1.5`).
/// - `rotate`: an absolute clockwise rotation in degrees (e.g. `45deg`).
/// - `opacity`: a target opacity as a ratio in `[0%, 100%]` (e.g. `50%` for
///   half-opaque). Pass `none` to leave opacity unchanged.
///
/// Relative transforms (Manim-style `shift` / `scale` / `rotate`):
/// - `dx`, `dy`: relative offset in cm (e.g. `dx: 2cm` moves right 2cm from
///   the current position). Either or both may be given.
/// - `scale-by`: relative scale multiplier (e.g. `1.5` grows by 50%).
/// - `rotate-by`: relative rotation in degrees (e.g. `15deg` adds 15° to the
///   current rotation).
///
/// - `duration`: number of milliseconds the animation spans (default `500`).
/// - `easing`: a string naming the rate curve (default `"smooth"`). One of:
///   `"linear"`, `"smooth"`, `"smoothstep"`, `"smootherstep"`,
///   `"quad-in"` / `"quad-out"` / `"quad-in-out"`,
///   `"cubic-in"` / `"cubic-out"` / `"cubic-in-out"` (aliases: `"ease-in"`,
///   `"ease-out"`, `"ease-in-out"`),
///   `"sin"` (sine ease-out), `"there-and-back"`, `"wiggle"`, `"lingering"`.
///   Unknown names fall back to `linear` with a warning.
/// - `timing`: sequencing relative to the previous animation on the timeline.
///   `"after"` (default) starts this animation once the previous one finishes;
///   `"with"` starts it at the same time as the previous one (parallel). Only
///   object animations accept `timing`.
/// - `delay`: extra wait in **milliseconds** before this animation begins, on
///   top of `timing` (default `0`).
///
/// Absolute and relative transforms may be combined in one `animate` call:
/// each produces a separate action that animates in parallel over the slide's
/// duration. Inert under standard Typst (returns `none`).
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
  easing: "smooth",
  timing: "after",
  delay: 0,
) = {
  _assert_str(target, "Animation target")
  if to != none and type(to) != array {
    panic("animate `to` must be an (x, y) array or none")
  }
  if opacity != none {
    _assert_ratio(opacity, "opacity")
    if opacity < 0% or opacity > 100% {
      panic("animate `opacity` must be in [0%, 100%]")
    }
  }
  if rotate != none { _assert_angle(rotate, "rotate") }
  if rotate-by != none { _assert_angle(rotate-by, "rotate-by") }
  _assert_nonneg(duration, "duration")
  _assert_easing(easing, "easing")
  _assert_timing(timing)
  _assert_nonneg(delay, "delay")
  none
}

/// Make a mobject visible instantly (set opacity to 1.0). No interpolation.
///
/// - `target`: the `name` of the object to make visible.
/// - `timing`: sequencing relative to the previous animation — `"after"`
///   (default) or `"with"` (parallel). See `animate` for details.
/// - `delay`: extra wait in milliseconds before this animation begins
///   (default `0`).
///
/// Useful for "appear without fading" effects. Inert under standard Typst.
#let appear(target, timing: "after", delay: 0) = {
  _assert_str(target, "Animation target")
  _assert_timing(timing)
  _assert_nonneg(delay, "delay")
  none
}

/// Make a mobject invisible instantly (set opacity to 0.0). No interpolation.
///
/// - `target`: the `label` of the object to make invisible.
/// - `timing`: sequencing relative to the previous animation — `"after"`
///   (default) or `"with"` (parallel). See `animate` for details.
/// - `delay`: extra wait in milliseconds before this animation begins
///   (default `0`).
///
/// Useful for "disappear without fading" effects. Inert under standard Typst.
#let disappear(target, timing: "after", delay: 0) = {
  _assert_str(target, "Animation target")
  _assert_timing(timing)
  _assert_nonneg(delay, "delay")
  none
}

/// Hold the current frame for `duration` milliseconds (default `500`, a manual pause marker).
/// Inert under standard Typst.
#let pause(duration: 500) = {
  _assert_nonneg(duration, "duration")
  none
}

/// Show `body` for `duration` milliseconds (default `500`) as its own animation unit (a block-level
/// object, precisely controllable like a mobject).
///
/// Under standard Typst the body is shown in the first frame.
#let play(body, duration: 500) = [#body]

/// Insert a voice / audio track. Inert under standard Typst (does nothing).
///
/// - `path`: audio file (`.opus`/`.ogg` for WebM/MKV, `.aac` for MP4).
/// - `blocking`: if `true`, the timeline waits for the clip to finish.
/// - `loop`: repeat the clip.
/// - `volume`: gain in `[0, 1]`.
/// - `slice`: optional `(start, end)` seconds sub-range of the clip.
/// - `timing`: sequencing relative to the previous animation — `"after"`
///   (default) or `"with"` (parallel). Audio is an object animation, so it
///   accepts `timing`.
/// - `delay`: extra wait in milliseconds before this track begins (default `0`).
#let audio(
  path,
  blocking: false,
  loop: false,
  volume: 1.0,
  slice: none,
  timing: "after",
  delay: 0,
) = {
  _assert_str(path, "audio path")
  _assert_bool(blocking, "blocking")
  _assert_bool(loop, "loop")
  _assert_range(volume, 0, 1, "audio volume")
  _assert_timing(timing)
  _assert_nonneg(delay, "delay")
  none
}

/// Insert a video reference as a placeholder mobject.
///
/// Since Typst cannot embed video, candy renders a labeled placeholder box
/// (a rounded rect with a ▶ icon and the filename). Under standard Typst this
/// is a visible placeholder; candy's renderer treats it like any other mobject
/// body (it can be animated with `animate`/`indicate`/etc.).
///
/// - `path`: path to the video file (displayed in the placeholder).
/// - `width`: placeholder width (default `8cm`).
/// - `height`: placeholder height (default `5cm`).
///
/// To show the actual first frame, extract it with ffmpeg first:
/// ```sh
/// ffmpeg -i input.mp4 -vframes 1 -q:v 2 first_frame.png
/// ```
/// then use `#mobject("vid", image("first_frame.png", width: 8cm))`.
#let video(path, width: 8cm, height: 5cm) = {
  _assert_str(path, "video path")
  _assert_length(width, "video width")
  _assert_length(height, "video height")
  block(
    width: width,
    height: height,
    radius: 4pt,
    stroke: 1pt + gray,
    fill: luma(240),
    align(center + horizon)[
      #text(28pt, fill: gray)[▶]
      #v(0.5em)
      #text(10pt, fill: gray)[Video: #path]
    ],
  )
}

#let _subtitle_anchor(position) = {
  let m = 1cm
  if type(position) == array {
    // Absolute (x, y) in cm: anchor the box's top-left corner there.
    let (x, y) = (position.at(0), position.at(1))
    (top + left, x * 1cm, y * 1cm)
  } else if position == "top" {
    (top + center, 0cm, m)
  } else if position == "center" or position == "centre" {
    (center + center, 0cm, 0cm)
  } else if position == "bottom-left" {
    (bottom + left, m, -m)
  } else if position == "bottom-right" {
    (bottom + right, -m, -m)
  } else if position == "top-left" {
    (top + left, m, m)
  } else if position == "top-right" {
    (top + right, -m, m)
  } else {
    // default: "bottom"
    (bottom + center, 0cm, -m)
  }
}

/// Show a caption over the animation.
///
/// - `body`: any valid Typst block content (e.g. `[Hello]`, `[$E = mc^2$]`,
///   `align(center)[ ... ]`).
/// - `duration`: how long the caption stays, in **milliseconds**. `none`
///   (default) means "persist" — the caption stays until it is replaced by
///   another `subtitle` in the *same* Typst scope, or until that scope exits
///   (auto-destroy). A positive number gives an explicit lifetime.
/// - `position`: anchor on the page. One of `"bottom"` (default), `"top"`,
///   `"center"`, `"bottom-left"`, `"bottom-right"`, `"top-left"`,
///   `"top-right"`, or a tuple `(x, y)` in cm for an absolute position.
/// - `easing`: rate curve used for the caption's own fade (default
///   `"linear"`). Custom modes `"bezier:x1,y1,x2,y2"` and `"expr:<math>"` are
///   accepted.
///
/// Subtitles use a fixed style: white text with black stroke for maximum
/// readability on any background.
///
/// Only one subtitle may be visible per Typst scope at a time; a later one
/// replaces an earlier one. A subtitle in a parent scope is temporarily hidden
/// while a child scope shows its own (shadowing). Under standard Typst the
/// caption is auto-positioned (via `place`) at the requested anchor so the
/// first frame renders correctly; candy's pipeline reads the same call from
/// the AST and overlays it on every frame with the same anchoring.
///
/// Subtitles are camera-independent: a global `#camera` (pan/zoom/rotate) only
/// transforms the mobjects, never the captions — a subtitle always stays at its
/// fixed page anchor and fixed size, regardless of the current view. This is a
/// mask/overlay, so it does **not** accept `timing`.
#let subtitle(body, duration: none, position: "bottom", easing: "linear") = {
  if duration != none and type(duration) != int and type(duration) != float {
    panic("subtitle duration must be a number or none")
  }
  if type(position) != str and type(position) != array {
    panic("subtitle position must be a string or an (x, y) array")
  }
  _assert_easing(easing, "easing")
  let (align, dx, dy) = _subtitle_anchor(position)

  // Fixed style: white text with black stroke for maximum contrast on any background
  [
    #set text(fill: white, stroke: black + 0.025em)
    #place(align, dx: dx, dy: dy)[#body]
  ]
}
