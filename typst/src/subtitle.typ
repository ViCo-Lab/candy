// Candy — subtitle module.
//
// A `subtitle` overlays arbitrary Typst block content on top of the animation.

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
/// Only one subtitle may be visible per Typst scope at a time; a later one
/// replaces an earlier one. A subtitle in a parent scope is temporarily hidden
/// while a child scope shows its own (shadowing). Under standard Typst the
/// caption is auto-positioned (via `place`) at the requested anchor so the
/// first frame renders correctly; candy's pipeline reads the same call from
/// the AST and overlays it on every frame with the same anchoring.
#let subtitle(
  body,
  duration: none,
  position: "bottom",
  easing: "linear",
) = {
  let (align, dx, dy) = _subtitle_anchor(position)
  place(align, dx: dx, dy: dy)[#body]
}
