// Candy — Manim-inspired directives.
//
// These port concepts from Manim Community Edition to candy's Typst-based
// animation model. Each is *inert under standard Typst* (compiles as a no-op
// or returns `none`), so a `.tyx` file using them is still a valid Typst
// document. The Candy Rust parser reads them from the AST.

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
/// - `duration`: number of milliseconds (default `500`).
/// - `easing`: rate curve (default `"smooth"`; see `animate` for the list).
///
/// Mirrors Manim's `Restore(mobject)`. Inert under standard Typst.
#let restore(
  target,
  slot: "default",
  duration: 500,
  easing: "smooth",
) = none

/// Briefly scale a mobject by `factor` and shift it by `(dx, dy)` cm, then
/// return it to its original state — all within `duration` milliseconds. A transient
/// "look here" effect.
///
/// - `target`: the `label` of the object to indicate.
/// - `factor`: scale multiplier at the peak (default `1.1`).
/// - `dx`, `dy`: offset in cm at the peak (default `0`).
/// - `duration`: number of milliseconds (default `300`).
/// - `easing`: rate curve for the "out" half (default `"smooth"`).
///
/// Mirrors Manim's `Indicate`. Inert under standard Typst.
#let indicate(
  target,
  factor: 1.1,
  dx: 0.0,
  dy: 0.0,
  duration: 300,
  easing: "smooth",
) = none

/// Briefly scale a mobject up by `factor` and fade it toward transparent, then
/// restore it to the original state. A "flash" attention effect.
///
/// - `target`: the `label` of the object to flash.
/// - `factor`: peak scale multiplier (default `2.0`).
/// - `duration`: number of milliseconds (default `200`).
/// - `easing`: rate curve (default `"smooth"`).
///
/// Mirrors Manim's `Flash`. Inert under standard Typst.
#let flash(
  target,
  factor: 2.0,
  duration: 200,
  easing: "smooth",
) = none

/// Oscillate a mobject's rotation by `±degrees` a few times within `duration`
/// milliseconds, then return to the original rotation.
///
/// - `target`: the `label` of the object to wiggle.
/// - `degrees`: peak rotation amplitude (default `15`).
/// - `duration`: number of milliseconds (default `500`).
/// - `easing`: rate curve (default `"wiggle"`).
///
/// Mirrors Manim's `Wiggle`. Inert under standard Typst.
#let wiggle(
  target,
  degrees: 15.0,
  duration: 500,
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
/// - `duration`: number of milliseconds (default `1`, i.e. instantaneous).
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
/// - `duration`: number of milliseconds for the transition (default `100`).
///
/// Inert under standard Typst.
#let transition(kind: "cut", duration: 100) = none

/// Zoom-to-region: nest a sub-animation that focuses on a rectangle of the
/// canvas. The `rect` (in cm, relative to the page origin) is enlarged to fill
/// the frame over `duration` milliseconds, producing a "camera zoom" effect.
///
/// - `rect`: `(x, y, w, h)` in cm — the region to zoom into.
/// - `duration`: number of milliseconds (default `500`).
/// - `easing`: rate curve (default `"smooth"`).
///
/// Implemented as a scale + translate on all mobjects; inert under standard
/// Typst.
#let zoom-to(rect, duration: 500, easing: "smooth") = none
