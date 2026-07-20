// Candy — public Typst API (function signatures only).
//
// Every directive defined here is *valid, standard Typst*. Compiling a `.tyx`
// with an ordinary `typst compile` renders the **first frame** of the
// animation: each `mobject` at its flow placement in the document flow,
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
//
// This entrypoint re-exports the directive modules below (each lives in its
// own `.typ` file) so `#import "@preview/candy:0.1.0": *` exposes the whole API.

#import "core.typ": *
#import "manim.typ": *
#import "composite.typ": *
#import "counter.typ": *
#import "constants.typ": *
