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

/// Animate an object to a new placement / scale / opacity over `duration`
/// frames.
///
/// - `target`: the `label` of the object to animate.
/// - `to`: an absolute target point `(x, y)` (lengths, e.g. `(4cm, 0pt)`).
/// - `scale`: a uniform scale factor (e.g. `1.5`).
/// - `opacity`: a target opacity in `[0, 1]` (`0` fades out, `1` fades in).
///
/// Any of `to` / `scale` / `opacity` may be omitted to keep the current value.
/// Inert under standard Typst (returns `none`), so the animation stays hidden
/// and only the first frame is shown.
#let animate(
  target,
  to: none,
  scale: none,
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
