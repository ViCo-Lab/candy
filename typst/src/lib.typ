// Candy — public Typst API (function signatures only).
//
// Every directive defined here is *valid, standard Typst*. Compiling a `.tyx`
// with an ordinary `typst compile` renders the **first frame** of the
// animation: each `mobject` at its natural placement in the document flow,
// every `play` block visible, and `animate`/`pause`/`audio` simply inert. The
// animation itself stays hidden. The Candy Rust toolchain reads the same
// directives from the source's **AST** (not the rendered output) and produces
// the full video, so a `.tyx` is simultaneously a valid Typst document and a
// Candy animation script ("code-oriented animation for Typst").
//
// Design notes for the parser (Rust side):
//   * `mobject` takes a bare Typst *block / element* as `body` — never a string.
//     Its position (and any other attributes) are taken automatically from
//     where the content lands in the layout; the user never passes `at`.
//   * `body` is passed by value (a content expression), so it renders with full
//     access to the surrounding scope.
//   * The parser detects these calls through the Typst AST and import analysis,
//     so they work regardless of *how* the module was imported (e.g.
//     `#import "candy": *` lets you call `mobject(...)` directly, while
//     `#import "candy"` + `candy.mobject(...)` also works — the parser resolves
//     the binding, not the literal prefix).

/// Register an animatable object ("mobject").
///
/// - `label`: unique string id, referenced later by `animate` / `play`.
/// - `body`: the object's content — a bare block or element (e.g.
///   `circle(radius: 1cm)`), *not* a string. Its placement is automatic.
///
/// Under standard Typst this simply renders `body` at its natural position.
#let mobject(label, body) = body

/// Animate an object to a new placement / scale / rotation / opacity over
/// `duration` frames.
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
/// - `duration`: number of frames the animation spans (default `30`).
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
  duration: 30,
  easing: "linear",
) = none

/// Hold the current frame for `duration` frames (a manual pause marker).
/// Inert under standard Typst.
#let pause(duration: 15) = none

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

/// Show `body` for `duration` frames as its own animation unit (a block-level
/// object, precisely controllable like a mobject).
///
/// Under standard Typst the body is shown in the first frame.
#let play(body, duration: 30) = body

// ============================================================================
// Manim-inspired directives
//
// The following directives port concepts from Manim Community Edition to
// candy's Typst-based animation model. Each is *inert under standard Typst*
// (compiles as a no-op or returns `none`), so a `.tyx` file using them is
// still a valid Typst document. The Candy Rust parser reads them from the AST.
// ============================================================================

/// Snapshot a mobject's current transform (x/y/scale/rotation/opacity) into a
/// named save slot. The slot can later be restored with `restore`.
///
/// - `target`: the `label` of the object to snapshot.
/// - `slot`: a name for the save slot (default `"default"`). Multiple slots
///   per target are allowed.
///
/// Mirrors Manim's `mobject.save_state()`. Inert under standard Typst.
#let save_state(target, slot: "default") = none

/// Interpolate a mobject from its current state back to a previously saved
/// state (see `save_state`).
///
/// - `target`: the `label` of the object to restore.
/// - `slot`: the save slot to restore from (default `"default"`).
/// - `duration`: number of frames (default `30`).
/// - `easing`: rate curve (default `"linear"`; see `animate` for the list).
///
/// Mirrors Manim's `Restore(mobject)`. Inert under standard Typst.
#let restore(
  target,
  slot: "default",
  duration: 30,
  easing: "linear",
) = none

/// Briefly scale a mobject by `factor` and shift it by `(dx, dy)` cm, then
/// return it to its original state — all within `duration` frames. A transient
/// "look here" effect.
///
/// - `target`: the `label` of the object to indicate.
/// - `factor`: scale multiplier at the peak (default `1.1`).
/// - `dx`, `dy`: offset in cm at the peak (default `0`).
/// - `duration`: number of frames (default `24`).
/// - `easing`: rate curve for the "out" half (default `"smooth"`).
///
/// Mirrors Manim's `Indicate`. Inert under standard Typst.
#let indicate(
  target,
  factor: 1.1,
  dx: 0.0,
  dy: 0.0,
  duration: 24,
  easing: "smooth",
) = none

/// Briefly scale a mobject up by `factor` and fade it toward transparent, then
/// restore it to the original state. A "flash" attention effect.
///
/// - `target`: the `label` of the object to flash.
/// - `factor`: peak scale multiplier (default `2.0`).
/// - `duration`: number of frames (default `18`).
/// - `easing`: rate curve (default `"smooth"`).
///
/// Mirrors Manim's `Flash`. Inert under standard Typst.
#let flash(
  target,
  factor: 2.0,
  duration: 18,
  easing: "smooth",
) = none

/// Oscillate a mobject's rotation by `±degrees` a few times within `duration`
/// frames, then return to the original rotation.
///
/// - `target`: the `label` of the object to wiggle.
/// - `degrees`: peak rotation amplitude (default `15`).
/// - `duration`: number of frames (default `20`).
/// - `easing`: rate curve (default `"wiggle"`).
///
/// Mirrors Manim's `Wiggle`. Inert under standard Typst.
#let wiggle(
  target,
  degrees: 15.0,
  duration: 20,
  easing: "wiggle",
) = none

/// Make a mobject visible instantly (set opacity to 1.0). No interpolation.
///
/// - `target`: the `label` of the object to make visible.
///
/// Useful for "appear without fading" effects. Inert under standard Typst.
#let appear(target) = none

/// Make a mobject invisible instantly (set opacity to 0.0). No interpolation.
///
/// - `target`: the `label` of the object to make invisible.
///
/// Useful for "disappear without fading" effects. Inert under standard Typst.
#let disappear(target) = none

/// Record a color change for a mobject. The color is tracked in the timeline
/// but the current renderer treats it as a no-op (Typst bodies are opaque
/// strings). Future versions with structured mobjects will apply it.
///
/// - `target`: the `label` of the object to recolor.
/// - `color`: a color name or hex string (e.g. `"red"`, `"#ff0000"`).
/// - `duration`: number of frames (default `1`, i.e. instantaneous).
/// - `easing`: rate curve (default `"linear"`).
///
/// Mirrors Manim's `set_color`. Inert under standard Typst.
#let set_color(
  target,
  color: "black",
  duration: 1,
  easing: "linear",
) = none

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

/// Mark a slide transition (a "cut" between scenes). Semantically, this is a
/// boundary marker; candy inserts a brief blank frame or crossfade between
/// the preceding and following content.
///
/// - `kind`: transition style — `"cut"` (instant, default), `"fade"` (crossfade),
///   `"slide"` (push). Only `"cut"` is fully implemented; others are recorded
///   for future versions.
/// - `duration`: number of frames for the transition (default `6`).
///
/// Inert under standard Typst.
#let transition(kind: "cut", duration: 6) = none

/// Zoom-to-region: nest a sub-animation that focuses on a rectangle of the
/// canvas. The `rect` (in cm, relative to the page origin) is enlarged to fill
/// the frame over `duration` frames, producing a "camera zoom" effect.
///
/// - `rect`: `(x, y, w, h)` in cm — the region to zoom into.
/// - `duration`: number of frames (default `30`).
/// - `easing`: rate curve (default `"smooth"`).
///
/// Implemented as a scale + translate on all mobjects; inert under standard
/// Typst.
#let zoom-to(rect, duration: 30, easing: "smooth") = none

#let dir-left = (-2.0, 0.0)
#let dir-right = (2.0, 0.0)
#let dir-up = (0.0, -2.0)
#let dir-down = (0.0, 2.0)
#let dir-origin = (0.0, 0.0)
#let dir-up-left = (dir-left.at(0) + dir-up.at(0), dir-left.at(1) + dir-up.at(1))
#let dir-up-right = (dir-right.at(0) + dir-up.at(0), dir-right.at(1) + dir-up.at(1))
#let dir-down-left = (dir-left.at(0) + dir-down.at(0), dir-left.at(1) + dir-down.at(1))
#let dir-down-right = (dir-right.at(0) + dir-down.at(0), dir-right.at(1) + dir-down.at(1))

#let grow = 1.5
#let shrink = 0.5
#let original = 1.0

#let quarter-turn = 90.0
#let half-turn = 180.0
#let full-turn = 360.0

#let visible = 1.0
#let half-visible = 0.5
#let invisible = 0.0
