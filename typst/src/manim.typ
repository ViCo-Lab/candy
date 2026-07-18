// Candy — Manim-inspired directives.
//
// These port concepts from Manim Community Edition to candy's Typst-based
// animation model. Each is *inert under standard Typst* (compiles as a no-op
// or returns `none`), so a `.tyx` file using them is still a valid Typst
// document. The Candy Rust parser reads them from the AST.

#import "validation.typ": *

/// Snapshot a mobject's current transform (x/y/scale/rotation/opacity) into a
/// named save slot. The slot can later be restored with `restore`.
///
/// - `target`: the `name` of the object to snapshot.
/// - `slot`: a name for the save slot (default `"default"`). Multiple slots
///   per target are allowed.
/// - `timing`: sequencing relative to the previous animation — `"after"`
///   (default) or `"with"` (parallel). See `animate` for details.
/// - `delay`: extra wait in milliseconds before this animation begins
///   (default `0`).
///
/// Mirrors Manim's `mobject.save_state()`. Inert under standard Typst.
#let save_state(target, slot: "default", timing: "after", delay: 0) = {
  _assert_str(target, "Animation target")
  _assert_str(slot, "save slot")
  _assert_timing(timing)
  _assert_nonneg(delay, "delay")
  none
}

/// Interpolate a mobject from its current state back to a previously saved
/// state (see `save_state`).
///
/// - `target`: the `label` of the object to restore.
/// - `slot`: the save slot to restore from (default `"default"`).
/// - `duration`: number of milliseconds (default `500`).
/// - `easing`: rate curve (default `"smooth"`; see `animate` for the list).
/// - `timing`: sequencing relative to the previous animation — `"after"`
///   (default) or `"with"` (parallel). See `animate` for details.
/// - `delay`: extra wait in milliseconds before this animation begins
///   (default `0`).
///
/// Mirrors Manim's `Restore(mobject)`. Inert under standard Typst.
#let restore(target, slot: "default", duration: 500, easing: "smooth", timing: "after", delay: 0) = {
  _assert_str(target, "Animation target")
  _assert_str(slot, "save slot")
  _assert_nonneg(duration, "duration")
  _assert_easing(easing, "easing")
  _assert_timing(timing)
  _assert_nonneg(delay, "delay")
  none
}

/// Briefly scale a mobject by `factor` and shift it by `(dx, dy)` cm, then
/// return it to its original state — all within `duration` milliseconds. A transient
/// "look here" effect.
///
/// - `target`: the `name` of the object to indicate.
/// - `factor`: scale multiplier at the peak (default `1.1`).
/// - `dx`, `dy`: offset in cm at the peak (default `0`).
/// - `duration`: number of milliseconds (default `300`).
/// - `easing`: rate curve for the "out" half (default `"smooth"`).
/// - `timing`: sequencing relative to the previous animation — `"after"`
///   (default) or `"with"` (parallel). See `animate` for details.
/// - `delay`: extra wait in milliseconds before this animation begins
///   (default `0`).
///
/// Mirrors Manim's `Indicate`. Inert under standard Typst.
#let indicate(target, factor: 1.1, dx: 0.0, dy: 0.0, duration: 300, easing: "smooth", timing: "after", delay: 0) = {
  _assert_str(target, "Animation target")
  _assert_number(factor, "factor")
  _assert_scalar(dx, "indicate dx")
  _assert_scalar(dy, "indicate dy")
  _assert_nonneg(duration, "duration")
  _assert_easing(easing, "easing")
  _assert_timing(timing)
  _assert_nonneg(delay, "delay")
  none
}

/// Briefly scale a mobject up by `factor` and fade it toward transparent, then
/// restore it to the original state. A "flash" attention effect.
///
/// - `target`: the `name` of the object to flash.
/// - `factor`: peak scale multiplier (default `2.0`).
/// - `duration`: number of milliseconds (default `200`).
/// - `easing`: rate curve (default `"smooth"`).
/// - `timing`: sequencing relative to the previous animation — `"after"`
///   (default) or `"with"` (parallel). See `animate` for details.
/// - `delay`: extra wait in milliseconds before this animation begins
///   (default `0`).
///
/// Mirrors Manim's `Flash`. Inert under standard Typst.
#let flash(target, factor: 2.0, duration: 200, easing: "smooth", timing: "after", delay: 0) = {
  _assert_str(target, "Animation target")
  _assert_number(factor, "factor")
  _assert_nonneg(duration, "duration")
  _assert_easing(easing, "easing")
  _assert_timing(timing)
  _assert_nonneg(delay, "delay")
  none
}

/// Oscillate a mobject's rotation by `±degrees` a few times within `duration`
/// milliseconds, then return to the original rotation.
///
/// - `target`: the `name` of the object to wiggle.
/// - `degrees`: peak rotation amplitude (default `15`).
/// - `duration`: number of milliseconds (default `500`).
/// - `easing`: rate curve (default `"wiggle"`).
/// - `timing`: sequencing relative to the previous animation — `"after"`
///   (default) or `"with"` (parallel). See `animate` for details.
/// - `delay`: extra wait in milliseconds before this animation begins
///   (default `0`).
///
/// Mirrors Manim's `Wiggle`. Inert under standard Typst.
#let wiggle(
  target,
  degrees: 15.0,
  duration: 500,
  easing: "wiggle",
  timing: "after",
  delay: 0,
) = {
  _assert_str(target, "Animation target")
  _assert_number(degrees, "degrees")
  _assert_nonneg(duration, "duration")
  _assert_easing(easing, "easing")
  _assert_timing(timing)
  _assert_nonneg(delay, "delay")
  none
}

/// Record a color change for a mobject. The color is tracked in the timeline
/// but the current renderer treats it as a no-op (Typst bodies are opaque
/// strings). Future versions with structured mobjects will apply it.
///
/// - `target`: the `name` of the object to recolor.
/// - `color`: a native Typst color (e.g. `red`, `white`, `rgb(255,0,0)`,
///   `rgb("#ff0000")`, `luma(50)`). A string such as `"red"` is NOT accepted —
///   pass the color value itself.
/// - `duration`: number of milliseconds (default `1`, i.e. instantaneous).
/// - `easing`: rate curve (default `"linear"`).
/// - `timing`: sequencing relative to the previous animation — `"after"`
///   (default) or `"with"` (parallel). See `animate` for details.
/// - `delay`: extra wait in milliseconds before this animation begins
///   (default `0`).
///
/// Mirrors Manim's `set_color`. Inert under standard Typst.
#let set_color(target, color: black, duration: 1, easing: "linear", timing: "after", delay: 0) = {
  _assert_str(target, "Animation target")
  _assert_color(color, "color")
  _assert_nonneg(duration, "duration")
  _assert_easing(easing, "easing")
  _assert_timing(timing)
  _assert_nonneg(delay, "delay")
  none
}

/// Blink: alternate opacity 1↔0 N times. Mirrors Manim's `Blink`.
///
/// - `target`: the `label` of the object to blink.
/// - `blinks`: number of on-off cycles (default `3`, must be an integer).
/// - `duration`: total milliseconds (default `500`, split evenly across blinks).
/// - `easing`: rate curve (default `"smooth"`).
/// - `timing`: sequencing relative to the previous animation — `"after"`
///   (default) or `"with"` (parallel). See `animate` for details.
/// - `delay`: extra wait in milliseconds before this animation begins
///   (default `0`).
#let blink(target, blinks: 3, duration: 500, easing: "smooth", timing: "after", delay: 0) = {
  _assert_str(target, "Animation target")
  _assert_int(blinks, "blinks")
  _assert_nonneg(duration, "duration")
  _assert_easing(easing, "easing")
  _assert_timing(timing)
  _assert_nonneg(delay, "delay")
  none
}

/// Spiral-in: fly in from a scaled-up, rotated, invisible state to the natural
/// position. Mirrors Manim's `SpiralIn`.
///
/// - `target`: the `name` of the object to spiral in.
/// - `scale`: initial scale factor (default `3.0` — starts 3× size).
/// - `rotate`: initial rotation in degrees (default `360` — one full turn).
/// - `duration`: milliseconds for the spiral-in (default `300`).
/// - `easing`: rate curve (default `"smooth"`).
/// - `timing`: sequencing relative to the previous animation — `"after"`
///   (default) or `"with"` (parallel). See `animate` for details.
/// - `delay`: extra wait in milliseconds before this animation begins
///   (default `0`).
#let spiral-in(target, scale: 3.0, rotate: 360.0, duration: 300, easing: "smooth", timing: "after", delay: 0) = {
  _assert_str(target, "Animation target")
  _assert_number(scale, "scale")
  _assert_number(rotate, "rotate")
  _assert_nonneg(duration, "duration")
  _assert_easing(easing, "easing")
  _assert_timing(timing)
  _assert_nonneg(delay, "delay")
  none
}

/// Focus-on: shrink a "spotlight" onto the target (scale down + dim).
/// Mirrors Manim's `FocusOn`.
///
/// - `target`: the `name` of the object to focus on.
/// - `factor`: scale-down factor (default `0.5` — shrinks to half size).
/// - `duration`: milliseconds (default `300`).
/// - `easing`: rate curve (default `"smooth"`).
/// - `timing`: sequencing relative to the previous animation — `"after"`
///   (default) or `"with"` (parallel). See `animate` for details.
/// - `delay`: extra wait in milliseconds before this animation begins
///   (default `0`).
#let focus-on(target, factor: 0.5, duration: 300, easing: "smooth", timing: "after", delay: 0) = {
  _assert_str(target, "Animation target")
  _assert_number(factor, "factor")
  _assert_nonneg(duration, "duration")
  _assert_easing(easing, "easing")
  _assert_timing(timing)
  _assert_nonneg(delay, "delay")
  none
}

/// Fade-transform: crossfade two mobjects — fade out `from` while fading in
/// `to`. Both must be registered via `mobject`. Mirrors Manim's
/// `FadeTransform` (simple crossfade variant; no stretch/alignment).
///
/// - `from`: the `name` of the source object (fades out).
/// - `to`: the `name` of the target object (fades in).
/// - `duration`: milliseconds (default `300`).
/// - `easing`: rate curve (default `"smooth"`).
/// - `timing`: sequencing relative to the previous animation — `"after"`
///   (default) or `"with"` (parallel). See `animate` for details.
/// - `delay`: extra wait in milliseconds before this animation begins
///   (default `0`).
#let fade-transform(from, to, duration: 300, easing: "smooth", timing: "after", delay: 0) = {
  _assert_str(from, "fade-transform `from`")
  _assert_str(to, "fade-transform `to`")
  _assert_nonneg(duration, "duration")
  _assert_easing(easing, "easing")
  _assert_timing(timing)
  _assert_nonneg(delay, "delay")
  none
}

/// Move the target along a polyline through `points` (cm). Like `#animate`'s
/// `to:`, the points are *relative to the object's natural layout position*
/// (the position it has under plain Typst), not absolute page coordinates.
/// The scheduler generates a keyframe at each point, evenly distributed across
/// `duration`. Mirrors Manim's `MoveAlongPath` (linear path; arc/bezier paths
/// are approximated as polylines).
///
/// - `target`: the `label` of the object to move.
/// - `path`: an array of `(x, y)` points in cm, e.g. `((0cm, 0cm), (4cm, 2cm), (8cm, 0cm))`.
/// - `duration`: how long the motion lasts, in milliseconds (default `500`).
/// - `easing`: rate curve (default `"smooth"`).
/// - `mode`: path interpretation — `"polyline"` (default).
/// - `orient`: if `true`, rotate the object to face its direction of travel
///   (default `false`).
/// - `timing`: sequencing relative to the previous animation — `"after"`
///   (default) or `"with"` (parallel). See `animate` for details.
/// - `delay`: extra wait in milliseconds before this animation begins
///   (default `0`).
#let move-along-path(target, path, duration: 500, easing: "smooth", mode: "polyline", orient: false, timing: "after", delay: 0) = {
  _assert_str(target, "Animation target")
  _assert_array(path, "move-along-path `path`")
  _assert_nonneg(duration, "duration")
  _assert_easing(easing, "easing")
  _assert_enum(mode, ("polyline", "bezier"), "move-along-path `mode`")
  _assert_bool(orient, "orient")
  _assert_timing(timing)
  _assert_nonneg(delay, "delay")
  none
}

/// Zoom-to-region: nest a sub-animation that focuses on a rectangle of the
/// canvas. The `rect` (in cm, relative to the page origin) is enlarged to fill
/// the frame over `duration` milliseconds, producing a "camera zoom" effect.
///
/// - `rect`: `(x, y, w, h)` in cm — the region to zoom into.
/// - `duration`: number of milliseconds (default `500`).
/// - `easing`: rate curve (default `"smooth"`).
///
/// Implemented as a scale + translate on all mobjects; inert under standard
/// Typst. This is a scene/camera animation, so it does **not** accept `timing`.
#let zoom-to(rect, duration: 500, easing: "smooth") = {
  _assert_array(rect, "zoom-to `rect`")
  _assert_nonneg(duration, "duration")
  _assert_easing(easing, "easing")
  none
}
