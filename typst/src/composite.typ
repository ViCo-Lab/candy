// Candy — composite animations.
//
// These directives compose existing candy primitives into higher-level
// effects. Inert under standard Typst.


/// Drive a single target through several keyframes, each controlling a subset
/// of its properties. Mirrors a timeline track and removes the need for many
/// sequential `#animate`s.
///
/// - `target`: the `label` of the object to animate.
/// - `keys`: an array of `(t, (x, y, scale, opacity, rotation))` tuples, where
///   `t` is the time offset (ms) from the slide start and each inner value is
///   *optional* — omitted properties carry their previous value forward. `x`/
///   `y` are in cm; `scale`/`opacity`/`rotation` are unitless. A keyframe may
///   also be written as `(t, x, y, scale, opacity, rotation)` (flat) — Candy
///   reads the state from the second element when present, else the tail.
/// - `duration`: how long the track lasts, in **milliseconds** (default `1000`).
/// - `easing`: rate curve for every segment (default `"linear"`).
#let track(target, keys: (), duration: 1000, easing: "smooth") = {
  if type(target) != str {
    panic("Animation target must be a string!")
  }
  none
}

/// A global camera pan + zoom + rotate, applied to the whole scene. Mirrors
/// Manim's camera moves (e.g. `self.camera.frame.shift(...)`).
///
/// - `x`, `y`: pan offset in cm from the page center (default `0`).
/// - `zoom`: zoom factor (default `1.0`; `> 1` zooms in).
/// - `rotate`: camera tilt in degrees clockwise (default `0`).
/// - `duration`: milliseconds (default `1000`).
///  `easing`: rate curve (default `"linear"`).
#let camera(x: 0, y: 0, zoom: 1.0, rotate: 0, duration: 1000, easing: "smooth") = none

/// Group several objects under a synthetic parent so they move/scale/rotate
/// together. Animate the `name` afterwards (e.g. `#animate("g", to: (...))`)
/// to transform every member. Groups may be nested.
///
/// - `name`: the label of the group (becomes a synthetic parent mobject).
/// - `members`: an array of member `label` strings.
#let group(name, members: ()) = {
  if type(name) != str {
    panic("Group name must be a string!")
  }
  none
}

/// Progressively reveal a *string* mobject by swapping its body to longer and
/// longer prefixes over `duration`. `by: "char"` reveals per character,
/// `by: "word"` per word. Non-string bodies fall back to a plain fade-in.
///
/// - `target`: the `name` of the (string) object to reveal.
/// - `by`: `"char"` or `"word"` (default).
/// - `duration`, `easing`: as usual.
#let reveal(target, by: "word", duration: 1000, easing: "smooth") = {
  if type(target) != str {
    panic("Animation target must be a string!")
  }
  none
}

/// Typewriter reveal — a convenience alias for `#reveal(.., by: "char")`.
#let typewriter(target, duration: 1000, easing: "smooth") = {
  if type(target) != str {
    panic("Animation target must be a string!")
  }
  none
}

/// Morph one mobject into another by crossfading + scaling. Both mobjects must
/// be registered via `mobject`. The `from` object shrinks and fades out while
/// the `to` object grows and fades in, producing a "transform" effect.
///
/// This is a simplified Morph — true point-by-point morphing (like Manim's
/// `Transform`) requires structured mobjects, which candy's opaque-content
/// model doesn't support. This crossfade+scale variant is a reasonable
/// approximation for most use cases.
///
/// - `from`: the `name` of the source object.
/// - `to`: the `name` of the target object.
/// - `duration`: milliseconds (default `500`).
/// - `easing`: rate curve (default `"smooth"`).
#let morph(from, to, duration: 500, easing: "smooth") = {
  if type(from) != str or type(to) != str {
    panic("Animation target must be a string!")
  }
  none
}

/// Morph a single mobject's content into new inline content. This is candy's
/// Manim-style `Transform` / `ReplacementTransform`: the `target` mobject's
/// current body is smoothly replaced by `to` (a Typst body — e.g. an equation
/// `[$a + b = c$]`), and the **original `target` label keeps the new content**
/// afterwards, so you can keep animating it. Under the hood it is a crossfade +
/// scale (the same mechanism as `morph`), but the old content is parked on a
/// synthetic mobject that ends invisible, so only the transformed target remains.
///
/// - `target`: the `name` of the mobject to transform (must be registered via
///   `mobject`).
/// - `to`: the new content (a bare block / element / equation), e.g.
///   `circle(radius: 2cm)` or `[$a + b + d = c$]`.
/// - `duration`: milliseconds (default `500`).
/// - `easing`: rate curve (default `"smooth"`).
///
/// Inert under standard Typst (returns `none`).
#let transform(target, to: none, duration: 500, easing: "smooth") = {
  if type(target) != str {
    panic("Animation target must be a string!")
  }
  none
}
