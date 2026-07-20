# Your first clip

Let's render a moving dot. Save this as `dot_move.tyx`:

```typst
#import "@preview/candy:0.1.0": *

#mobject("dot", circle(radius: 1cm, fill: blue))
#animate("dot", to: (4cm, 0pt), duration: 1000, easing: "linear")
#pause(duration: 500)
```

Then build it:

```sh
candy build dot_move.tyx
```

Candy writes `dist/dot_move.mp4` (H.264 in an MP4 container by default). Open it and
you'll see the blue dot glide 4 cm to the right over 1000 ms (1 second), then hold
for 500 ms.

## What each line does

- `#import "@preview/candy:0.1.0": *` — pulls in Candy's DSL. Every directive is *valid,
  standard Typst*, so `typst compile dot_move.tyx` still renders the first frame.
- `#mobject("dot", circle(radius: 1cm, fill: blue))` — registers an animatable object
  named `dot`. Its *placement is automatic*: it lands wherever `body` naturally falls in
  the document flow. You never pass an `at:` coordinate.
- `#animate("dot", to: (4cm, 0pt), …)` — animates `dot` to the absolute point
  `(4cm, 0pt)` over `duration: 1000` milliseconds, using the `"linear"` easing curve.
- `#pause(duration: 500)` — holds the current frame for 500 milliseconds.

## A few things to try

```sh
# Animated GIF (looping) — no codec needed, great for quick previews
candy build dot_move.tyx --format gif

# Static PNG poster of the final frame
candy build dot_move.tyx --format png

# AV1 in WebM
candy build dot_move.tyx --format webm --codec av1

# SVG draft: one file per frame under .candy/dot_move/
candy build dot_move.tyx --format svg
```

> **Debugging tip.** Use `cargo run -- <args>` instead of `candy <args>` while working
> on the Rust backend.

## Mobjects & actions — the core idea

A **mobject** is an animatable object: a bare Typst block or element (`circle(...)`,
`text(...)`, `[$E = mc^2$]`, an imported `canvas(...)`, …). Its *home* position is where
`body` lands in the document flow; you animate it *relative to* that home.

An **action** (`#animate`, `#blink`, `#morph`, …) targets a mobject by its `label`
string and changes a transform (position / scale / rotation / opacity) over `duration`
milliseconds. Multiple actions on different targets run in **parallel**.

**Layout & hidden mobjects.** A mobject's *flow* placement is where `body` lands in
the flow; Candy measures that box once (`ensure_flow`). Mobjects that are *temporarily
not rendered* at frame 0 — a `#reveal` / `#typewriter` target before it has typed
anything, a `play` block, or a `transform` target whose content timeline starts as
`none` — still **reserve their flow box** (wrapped in Typst `#hide[…]`) so later
mobjects do **not** shift up to fill the gap. Pure containers with no content of their
own are the only objects skipped. This means you can safely stack a `reveal` caption
between two always-visible shapes without the layout jumping when the text types in.

Next: [Animation basics](animation-basics.md).
