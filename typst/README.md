# Candy

## Code-oriented Animation eNgine Designed for tYpst

This document is the user-facing reference for **Candy's Typst DSL**. Candy turns
`.tyx` X-sheets (standard Typst extended with candy directives) into self-contained
videos (MP4 / MKV / WebM) or SVG drafts.

> A `.tyx` file is **valid, standard Typst**. Compiling it with `typst compile`
> renders the *first frame* of the animation (every object at its natural placement,
> every `play` block visible, and `animate` / `pause` / `audio` inert). The Candy
> Rust pipeline reads the **same directives from the source AST** and produces the
> full clip. So a single `.tyx` is simultaneously a normal Typst document *and* a
> Candy animation script.

```typst
#import "@preview/candy:0.1.0": *

#mobject("dot", circle(radius: 1cm, fill: blue))
#animate("dot", to: (4cm, 0pt), duration: 1000, easing: "linear")
#pause(duration: 500)
```

> **Full documentation.** This README is the quick reference shown on Typst Universe. The
> complete, restructured docs — a learn-by-doing [Tutorial](https://github.com/ViCo-Lab/candy/blob/main/docs/tutorial/README.md)
> and a lookup [Reference](https://github.com/ViCo-Lab/candy/blob/main/docs/reference/README.md)
> (directives, easing, counters, CLI, codecs, errors) — live in the repository's
> [`docs/`](https://github.com/ViCo-Lab/candy/blob/main/docs/README.md).

---

## Table of contents

- [Getting started](#getting-started)
- [Core concepts](#core-concepts)
  - [Timing model](#timing-model)
  - [Scene / canvas](#scene--canvas)
  - [Mobjects & actions](#mobjects--actions)
- [Directives reference](#directives-reference)
  - [Timing & sequencing](#timing--sequencing)
  - [Scene & camera](#scene--camera)
  - [Mobjects & definitions](#mobjects--definitions)
  - [Object animations](#object-animations)
  - [Content blocks](#content-blocks)
  - [Subtitles (masks)](#subtitles-masks)
  - [Easing counters](#easing-counters)
  - [Helpers & constants](#helpers--constants)
- [Easing reference](#easing-reference)
- [Counters & `ecval`](#counters--ecval)
- [Worked examples](#worked-examples)

---

## Getting started

```sh
# Default: H.264 in an MP4 container → dist/<stem>.mp4
candy build examples/dot_move.tyx

# AV1 in WebM
candy build examples/dot_move.tyx --format webm --codec av1

# SVG draft (one file per frame under .candy/<stem>/)
candy build examples/dot_move.tyx --format svg

# Animated GIF of every frame (looping) — no codec needed
candy build examples/dot_move.tyx --format gif

# Static PNG poster of the final frame
candy build examples/dot_move.tyx --format png
```

Every directive is *inert under standard Typst* (it either returns `body` or `none`),
so you can iterate on layout with `typst compile` and render the animation with
`candy build`.

---

## Core concepts

### Timing model

- Candy uses a **millisecond** timeline internally. The `--fps` CLI flag only sets
  the output frame rate; a 1000 ms slide at 30 fps yields ~30 frames, at 60 fps ~60
  frames — the wall-clock duration is unchanged.
- `#animate` / `#pause` / `#play` / `#transform` / … `duration:` arguments are expressed
  directly in **milliseconds** (default `500`). There is no frame-based timing — the
  scheduler works entirely in ms, and only the final rasterization samples that timeline
  at `--fps`.
- `#subtitle` and `#ecnew` lifetimes are expressed in **milliseconds** directly.
- **Object animations** (see [Object animations](#object-animations)) additionally accept
  `timing:` and `delay:` to control how they are sequenced on the timeline. Scene
  animations and masks do **not** accept these (see [Timing & sequencing](#timing--sequencing)).

### Scene / canvas

```typst
#scene(width: 16cm, height: 9cm, bg: white)[
  // all mobjects & actions here
]
```

`scene` wraps the body in a `page()` call so each scene renders as an independent
page. The renderer uses the scene's page size as the canvas for every frame in that
scene. Without `#scene`, candy defaults to 16 cm × 9 cm.

**Scene semantics** (how candy groups and shows content):

- **Nesting** — a `scene` may appear inside another scene's body, forming a child
  scene. Nesting is resolved through the Typst AST, so import style is irrelevant.
- **Parent auto-hide** — entering a child scene automatically hides its parent (and
  any ancestor) for the child's duration. The renderer always shows the *deepest*
  active scene at each frame time, so the child visually replaces the parent.
- **Typst scope** — a mobject / `play` / `subtitle` belongs to the innermost `scene`
  whose body encloses it (the scope in which it is evaluated).
- **Cross-page scene** — a scene's `width`/`height` set the size of *each* page.
  Content overflowing the page spills onto subsequent pages; the mobjects stay in
  one scene (shared data) but the renderer plays the pages **in sequence** on a
  single-page canvas (the canvas does *not* grow). Each page has its own timeline
  and the other pages stay frozen until the current page finishes and the renderer
  auto-advances, so nothing is clipped or split into sub-scenes.
- **Auto-split** — content spanning multiple pages is automatically split into
  multiple scenes (one per page) when no explicit root `scene` wraps it.
- **Implicit root** — with no `scene` call, the entire document is one implicit root
  scene that still follows the one-page / split rules (default 16 cm × 9 cm). A
  child scene inherits its page size from the nearest ancestor that declares one.

```typst
// nested scenes: "outer" shows first, "inner" replaces it (parent auto-hidden)
#scene(width: 16cm, height: 9cm)[
  #mobject("a", circle(radius: 1cm, fill: blue))
  #animate("a", to: (4cm, 0pt), duration: 1000)
  #scene(width: 10cm, height: 6cm)[
    #mobject("b", square(size: 2cm, fill: red))
    #animate("b", to: (3cm, 2cm), duration: 800)
  ]
]
```

### Mobjects & actions

A **mobject** is an animatable object: a bare Typst block/element (`circle(...)`,
`text(...)`, `[$E = mc^2$]`, an imported `canvas(...)`, etc.). Its *placement is
automatic* — taken from where the body lands in the document flow. You never pass an
`at:` coordinate; instead you animate it *relative to* that home position.

An **action** (`#animate`, `#blink`, `#morph`, …) targets a mobject by its `label`
string and changes a transform (position / scale / rotation / opacity) over `duration`
milliseconds. Multiple actions on different targets run in parallel.

**Layout & hidden mobjects.** A mobject's *natural* placement is where `body` lands in
the document flow; `ensure_natural` measures that box by rendering every object once.
Mobjects that are *temporarily not rendered* at frame 0 — a `#reveal` / `#typewriter`
target before it has typed anything, a `play` block, or a `transform` target whose
content timeline starts as `none` — still **reserve their natural box** in the flow:
candy wraps them in Typst `#hide[…]` so the space is kept (and the hidden object gets a
correct `nat` to be placed at once it appears) while later mobjects do **not** shift up
to fill the gap. Pure containers with no content of their own are the only objects
skipped. This means you can safely stack a `reveal` caption between two always-visible
shapes without the layout jumping when the text types in.

---

## Directives reference

### Timing & sequencing

Object animations (the [Object animations](#object-animations) section below) accept two
extra timing parameters that control how the animation is sequenced relative to the
*previous* animation on the timeline. This mirrors the **Start** options in the
PowerPoint animation pane:

- `timing:` — `"after"` (default) starts this animation once the previous one finishes;
  `"with"` starts it at the same time as the previous one (parallel). Only object
  animations accept `timing`.
- `delay:` — an extra wait in **milliseconds** before this animation begins, on top of
  `timing` (default `0`).

```typst
#animate("a", to: (4cm, 0pt), duration: 1000)            // after the previous one
#animate("b", to: (0cm, 3cm), duration: 1000, timing: "with")  // parallel with "a"
#animate("c", dx: 2cm, duration: 500, timing: "after", delay: 300) // 300ms after "b"
```

Scene animations (`scene` / `scene-switch` / `transition` / `camera` / `zoom-to`) and
the mask (`subtitle`) do **not** accept `timing` / `delay` — they are laid out in
document order.

### Scene & camera

These are **scene animations**: they set up the canvas, switch between slides, or move
the whole view. They have no `timing` / `delay`.

#### `#scene(name: none, width: 16cm, height: 9cm, bg: white, body)` {#scene}

Define a scene (a "slide"). See [Scene / canvas](#scene--canvas) for full semantics.
Under standard Typst this sets the page and renders `body`.

#### `#scene-switch(target, duration: 0, easing: "smooth")` {#scene-switch}

Jump the timeline cursor to a named scene (`target` is the scene's `name:` or its
auto-assigned UUID-like name). `duration: 0` is an instant jump; a positive value
animates the transition. Inert under standard Typst.

#### `#transition(kind: "cut", duration: 100)` {#transition}

Mark a slide transition. `kind`: `"cut"` (instant, default), `"fade"` (crossfade),
`"slide"` (push). Only `"cut"` is fully implemented; the others are recorded for future
versions. Inert under standard Typst.

#### `#camera(x: 0, y: 0, zoom: 1.0, rotate: 0, duration: 1000, easing: "smooth")` {#camera}

A global camera move (pan + zoom + rotate) applied to the whole scene. `x` / `y` are a
pan offset in cm from the page center; `zoom > 1` magnifies; `rotate` tilts clockwise in
degrees. Scene-scoped. Inert under standard Typst.

```typst
#camera(zoom: 2.0, x: -3cm, y: 1.5cm, duration: 1500, easing: "smooth")
#camera(zoom: 1.0, rotate: 12, duration: 1500, easing: "smooth")
```

#### `#zoom-to(rect, duration: 500, easing: "smooth")` {#zoom-to}

Zoom-to-region: enlarge a rectangle of the canvas to fill the frame over `duration`
milliseconds. `rect` is `(x, y, w, h)` in cm, relative to the page origin. Implemented
as a scale + translate on all mobjects. Inert under standard Typst.

```typst
#zoom-to((4, 3, 6, 4), duration: 1000, easing: "smooth")
```

### Mobjects & definitions

These register or group animatable content. They are not animations, so they have no
`timing` / `duration`.

#### `#mobject(label, body)` {#mobject}

Register an animatable object. `label` is a unique string id; `body` is a bare block or
element (never a string). Under standard Typst this simply renders `body` at its natural
position.

```typst
#mobject("dot", circle(radius: 1cm, fill: blue))
```

#### `#group(name, members: ())` {#group}

Group several mobjects under a synthetic parent so they move / scale / rotate together.
Animate the `name` afterwards (e.g. `#animate("g", rotate: 360)`) to transform every
member at once. Groups may be nested. The group's rotation pivots about the figure's
centroid, so a ring of objects placed around a center spins in place.

```typst
#group("wheel", members: ("spoke1", "spoke2", "hub"))
#animate("wheel", rotate: 360, duration: 3000, easing: "linear")
```

#### `#video(path, width: 8cm, height: 5cm)` {#video}

Insert a **video reference** as a placeholder mobject. Typst cannot embed video, so
candy renders a labeled placeholder box (rounded rect + ▶ icon + filename). The
placeholder behaves like any other mobject body (can be animated). To show the real first
frame, extract it with ffmpeg and use `#mobject("vid", image(...))` instead.

```typst
#mobject("clip", video("intro.mp4", width: 10cm, height: 6cm))
#animate("clip", scale: 1.2, duration: 500, easing: "smooth")
```

### Object animations

These target a mobject (or, for `#audio`, a media track) and **accept `timing:` and
`delay:`** as described in [Timing & sequencing](#timing--sequencing). Under standard
Typst they are inert (return `none`), except where noted.

#### `#animate(target, ..)` {#animate}

Animate `target` over `duration` milliseconds (default `500`). Supports absolute and
relative transforms in any combination; each produces a parallel action.

| Argument | Meaning |
|---|---|
| `to: (x, y)` | absolute target point in lengths, e.g. `(4cm, 0pt)` |
| `dx:` / `dy:` | relative offset in cm (Manim-style `shift`), e.g. `dx: 2cm` |
| `scale:` | absolute scale factor (e.g. `1.5`) |
| `scale-by:` | relative scale multiplier (e.g. `1.5` grows 50%) |
| `rotate:` | absolute clockwise rotation in degrees (e.g. `45`) |
| `rotate-by:` | relative rotation in degrees (e.g. `15` adds 15°) |
| `opacity:` | target opacity in `[0, 1]` |
| `duration:` | length of the animation in **milliseconds** (default `500`) |
| `easing:` | rate curve (default `"smooth"`; see [Easing](#easing-reference)) |
| `timing:` | `"after"` (default) or `"with"` — sequencing vs the previous animation |
| `delay:` | extra wait in **milliseconds** before start (default `0`) |

```typst
#animate("dot", to: (4cm, 0pt), duration: 1000, easing: "linear")
#animate("box", scale: 1.5, duration: 800, easing: "smooth")
#animate("sq", dx: 2cm, rotate-by: 90, opacity: 50%, duration: 600, timing: "with")
```

#### `#appear(target, timing: "after", delay: 0)` / `#disappear(target, timing: "after", delay: 0)` {#appear}

Make a mobject visible instantly (`opacity: 100%`) or invisible instantly (`opacity:
0%`), with no interpolation. Useful for appear/disappear-without-fading effects. Inert
under standard Typst.

#### `#save_state(target, slot: "default", timing: "after", delay: 0)` {#save_state}

Snapshot a mobject's current transform (x / y / scale / rotation / opacity) into a named
save slot. Mirrors `mobject.save_state()`. Inert under standard Typst.

#### `#restore(target, slot: "default", duration: 500, easing: "smooth", timing: "after", delay: 0)` {#restore}

Interpolate a mobject from its current state back to a previously saved state. Mirrors
`Restore(mobject)`. Inert under standard Typst.

```typst
#save_state("dot", slot: "home")
#animate("dot", to: (3cm, 2cm), duration: 800)
#restore("dot", slot: "home", duration: 200, easing: "cubic-in-out")
```

#### `#indicate(target, factor: 1.1, dx: 0.0, dy: 0.0, duration: 300, easing: "smooth", timing: "after", delay: 0)` {#indicate}

Briefly scale + shift a mobject, then return — a transient "look here" effect. Mirrors
`Indicate`. Inert under standard Typst.

#### `#flash(target, factor: 2.0, duration: 200, easing: "smooth", timing: "after", delay: 0)` {#flash}

Briefly scale a mobject up by `factor` and fade it toward transparent, then restore — a
"flash" attention effect. Mirrors `Flash`. Inert under standard Typst.

#### `#wiggle(target, degrees: 15.0, duration: 500, easing: "wiggle", timing: "after", delay: 0)` {#wiggle}

Oscillate a mobject's rotation by ±`degrees` a few times, then return. Mirrors `Wiggle`.
Inert under standard Typst.

#### `#set_color(target, color: black, duration: 1, easing: "linear", timing: "after", delay: 0)` {#set_color}

Record a color change for a mobject. The color is tracked in the timeline, but the
current renderer treats it as a no-op (Typst bodies are opaque strings). Future versions
with structured mobjects will apply it. Mirrors `set_color`. Inert under standard Typst.

```typst
#set_color("dot", color: red, duration: 300, easing: "smooth")
```

#### `#blink(target, blinks: 3, duration: 500, easing: "smooth", timing: "after", delay: 0)` {#blink}

Alternate opacity 1↔0 `blinks` times. Mirrors `Blink`. Inert under standard Typst.

#### `#spiral-in(target, scale: 3.0, rotate: 360.0, duration: 300, easing: "smooth", timing: "after", delay: 0)` {#spiral-in}

Fly in from a scaled-up, rotated, invisible state to the natural position. Mirrors
`SpiralIn`. Inert under standard Typst.

#### `#focus-on(target, factor: 0.5, duration: 300, easing: "smooth", timing: "after", delay: 0)` {#focus-on}

Shrink a "spotlight" onto the target (scale down + dim). Mirrors `FocusOn`. Inert under
standard Typst.

#### `#fade-transform(from, to, duration: 300, easing: "smooth", timing: "after", delay: 0)` {#fade-transform}

Crossfade two pre-registered mobjects: fade out `from` while fading in `to`. Both must be
registered via `mobject`. Mirrors `FadeTransform` (simple crossfade variant). Inert under
standard Typst.

#### `#move-along-path(target, path, duration: 500, easing: "smooth", mode: "polyline", orient: false, timing: "after", delay: 0)` {#move-along-path}

Move `target` along a polyline through `path` (array of `(x, y)` points in cm, absolute).
The scheduler generates a keyframe at each point, distributed across `duration`. Mirrors
`MoveAlongPath` (linear paths; arcs/beziers approximated as polylines). Inert under
standard Typst.

```typst
#move-along-path("ball", ((2, 2), (6, 5), (10, 2), (14, 4)), duration: 2000, easing: "smooth")
```

#### `#morph(from, to, duration: 500, easing: "smooth", timing: "after", delay: 0)` {#morph}

Morph one mobject into another by crossfading + scaling. Both must be registered via
`mobject`. This is the **simplified** Morph — true point-by-point morphing (Manim's
`Transform`) requires structured mobjects, which candy's opaque-content model does not
support; the crossfade + scale variant is a reasonable approximation. Inert under
standard Typst.

#### `#transform(target, to: none, duration: 500, easing: "smooth", timing: "after", delay: 0)` {#transform}

Morph a **single** mobject's content into new inline content — candy's Manim-style
`Transform` / `ReplacementTransform`. `target`'s current body is smoothly replaced by
`to` (a Typst body — a shape or a formula such as `[$a + b + d = c$]`), and the
**original `target` label keeps the new content**, so you can keep animating it
afterwards.

For **inline content** (formulas and text) the transform is **glyph-by-glyph**, not a
whole-block dissolve. Candy renders the old and new bodies with Typst's own SVG layout
and extracts every glyph and decoration (fraction bars, roots, …) as a positioned
fragment, then matches old↔new fragments by their outline signature (longest common
subsequence). During the window:

- **matched** fragments *glide* from their old slot to their new slot,
- **removed** fragments *fade and slide out* toward the next kept glyph,
- **inserted** fragments *fade and slide in* from the previous kept glyph,

so the old equation visibly disassembles and reassembles into the new one. Fractions
(`a/b`, `\frac{a}{b}`) are kept intact as a single token so the fraction bar renders
correctly (stacked numerator/denominator, not a bare slash). For **shapes** (non-inline
content) the transform falls back to a crossfade + scale morph.

```typst
#mobject("eq", [$a + b = c$])
#transform("eq", to: [$a + b + d = c$], duration: 1000, easing: "smooth")
```

#### `#reveal(target, by: "word", duration: 1000, easing: "smooth", timing: "after", delay: 0)` {#reveal}

Progressively reveal a *string* mobject by swapping its body to longer and longer
prefixes over `duration`. `by: "word"` reveals word-by-word; `by: "char"` reveals
character-by-character. Non-string bodies fall back to a plain `FadeIn` with a warning.
The body must be a string literal (`#mobject("cap", "Hello")`), not a content block. At
frame 0 the target is `none`, but candy keeps its layout box reserved (see [Mobjects &
actions](#mobjects--actions)), so later content does not jump as it types in.

#### `#typewriter(target, duration: 1000, easing: "smooth", timing: "after", delay: 0)` {#typewriter}

Convenience alias for `#reveal(.., by: "char")` — a classic typewriter reveal.

```typst
#mobject("cap", "Step 1: divide by a.")
#typewriter("cap", duration: 1500, easing: "linear")
```

#### `#track(target, keys: (), duration: 1000, easing: "smooth", timing: "after", delay: 0)` {#track}

Drive a single target through several keyframes, each controlling a subset of its
properties — a timeline track that removes the need for many sequential `#animate`s.
Mirrors a Manim `ValueTracker`-driven animation. `keys` is an array of
`(t, (x, y, scale, opacity, rotation))` tuples, where `t` is the time offset (ms) from
the slide start and each inner value is *optional* (omitted properties carry their
previous value forward); `x`/`y` are in cm, `scale`/`opacity`/`rotation` unitless. A
keyframe may also be written flat as `(t, x, y, scale, opacity, rotation)`.

```typst
#track("p",
  keys: (
    (0,    (0cm, 0cm, 1, 1, 0)),
    (1000, (3cm, 2cm, 1.5, 1, 90)),
    (2000, (4cm, 0cm, 1, 0, 0)),
  ),
  duration: 2000, easing: "smooth")
```

#### `#audio(path, blocking: false, loop: false, volume: 1.0, slice: none, timing: "after", delay: 0)` {#audio}

Insert a voice / audio track. Audio is an **object animation**, so it accepts `timing`
and `delay`. Inert under standard Typst (does nothing).

| Argument | Meaning |
|---|---|
| `path` | audio file (`.opus`/`.ogg` for WebM/MKV, `.aac` for MP4) |
| `blocking:` | if `true`, the timeline waits for the clip to finish |
| `loop:` | repeat the clip |
| `volume:` | gain in `[0, 1]` |
| `slice:` | optional `(start, end)` seconds sub-range of the clip |
| `timing:` | `"after"` (default) or `"with"` — sequencing vs the previous animation |
| `delay:` | extra wait in **milliseconds** before start (default `0`) |

```typst
#audio("voice.opus", blocking: false, loop: false, volume: 0.9, slice: none)
```

### Content blocks

#### `#play(body, duration: 500)` {#play}

Show `body` for `duration` milliseconds (default `500`) as its own animation unit (a
block-level object, precisely controllable like a mobject). `play` is a self-contained
content block and does **not** accept `timing` / `delay`. Under standard Typst the body
is shown in the first frame.

```typst
#play([#text(28pt, weight: "bold")[Step 1 of 3]], duration: 1000)
#play([#text(28pt, weight: "bold")[Step 2 of 3]], duration: 1000)
```

#### `#pause(duration: 500)` {#pause}

Hold the current frame for `duration` milliseconds. Inert under standard Typst.

### Subtitles (masks)

#### `#subtitle(body, duration: none, position: "bottom", easing: "linear")` {#subtitle}

Overlay `body` (any Typst block content) on top of the animation. A subtitle is a
**mask/overlay** and does **not** accept `timing` / `delay`.

- `duration:` lifetime in **milliseconds**. `none` (default) means *persist* — the
  caption stays until replaced by another `subtitle` in the same Typst scope, or until
  that scope exits (auto-destroy). A positive number gives an explicit lifetime.
- `position:` anchor — `"bottom"` (default), `"top"`, `"center"`, `"bottom-left"`,
  `"bottom-right"`, `"top-left"`, `"top-right"`, or a tuple `(x, y)` in cm for an
  absolute position.
- `easing:` rate curve for the caption's own fade (default `"linear"`). Custom modes
  `"bezier:x1,y1,x2,y2"` and `"expr:<math>"` are accepted.

Only one subtitle may be visible per Typst scope at a time; a later one replaces an
earlier one. A subtitle in a parent scope is temporarily hidden while a child scope
shows its own (shadowing).

```typst
#subtitle([Long-lived caption], position: "bottom")
#[
  #subtitle([Child scope caption], position: "top", duration: 800)
  #pause(duration: 800)
]
```

### Easing counters

A key-value store of animatable integers, referenced from mobject / subtitle bodies via
`ecval(name)`. Standard Typst sees the integer seed; the candy pipeline steps the value
over time, shaped by the counter's easing.

#### `#ecnew(name, seed: 0, step: 1, duration: none, easing: "linear")` {#ecnew}

Register an integer counter. Returns `seed` under standard Typst (so binding it captures
the initial value). With no `duration`, the counter steps once per millisecond; a
positive `duration` ramps `seed → seed + step·duration` over that window, shaped by
`easing`.

#### `#ecval(value, default: 0)` {#ecval}

Read the current value of an easing counter. Inside candy's pipeline it is substituted
with the live, eased integer and may be used directly as a Typst parameter
(`rect(width: ecval(n) * 1cm)`). Under standard Typst it returns its argument unchanged
when it is already a number, so bind the `ecnew` result (`#let n = ecnew("n")`) and
pass `n`.

#### `#ecpause(name)` / `#ecresume(name)` / `#ecdestroy(name)` {#counter-control}

Pause / resume / freeze a counter. Inert under standard Typst.

```typst
#let r = ecnew("r", seed: 40, step: 1)
#mobject("dot", circle(radius: ecval(r) * 1pt + 1cm, fill: blue))
#pause(duration: 600)
#ecpause("r")
#pause(duration: 600)
#ecresume("r")
#ecdestroy("r")
```

### Helpers & constants

Direction vectors (cm): `dir-left` `dir-right` `dir-up` `dir-down` `dir-origin`
`dir-up-left` `dir-up-right` `dir-down-left` `dir-down-right`.

Scale factors: `grow` (1.5) `shrink` (0.5) `original` (1.0).

Turns (degrees): `quarter-turn` (90) `half-turn` (180) `full-turn` (360).

Opacity presets: `visible` (1.0) `half-visible` (0.5) `invisible` (0.0).

```typst
#animate("dot", dx: dir-right.at(0) * 1cm, dy: dir-up.at(1) * 1cm, duration: 500)
#animate("dot", scale-by: grow, duration: 500)
```

---

## Easing reference

`easing:` accepts a named curve or a custom spec:

**Named curves** (unknown names fall back to `linear` with a warning):

`"linear"`, `"smooth"`, `"smoothstep"`, `"smootherstep"`,
`"quad-in"` / `"quad-out"` / `"quad-in-out"`,
`"cubic-in"` / `"cubic-out"` / `"cubic-in-out"` (aliases `"ease-in"`, `"ease-out"`,
`"ease-in-out"`),
`"sin"` (sine ease-out), `"there-and-back"`, `"wiggle"`, `"lingering"`.

**Custom specs** (accepted by `#animate`, `#subtitle`, `#ecnew`, …):

- `expr:<math>` — a mathematical expression in `t` ∈ [0, 1], e.g.
  `"expr: 1 - (1 - t)^3"`.
- `bezier:x1,y1,x2,y2` — a cubic Bézier control-point spec (CSS-style easing curve),
  e.g. `"bezier:0.25,0.1,0.25,1.0"`.

```typst
#animate("sq", to: (10cm, 0cm), duration: 2000, easing: "expr: 1 - (1 - t)^3")
#animate("dot", to: (0cm, 5cm), duration: 2000, easing: "bezier:0.25,0.1,0.25,1.0")
```

---

## Counters & `ecval`

The easing-counter module lets you drive Typst parameters (widths, radii, text content)
with live, animatable integers. The substitution happens in the Rust renderer, so the
same `.tyx` compiles under plain `typst compile` with the `seed` value.

```typst
#scene(width: 16cm, height: 9cm)[
  #let r = ecnew("r", seed: 40, step: 1)
  #mobject("dot", circle(radius: ecval(r) * 1pt + 1cm, fill: blue))
  #animate("dot", to: (0cm, 5cm), duration: 2000, easing: "bezier:0.25,0.1,0.25,1.0")
  #subtitle([r = #str(ecval(r))], position: "bottom")
  #ecpause("r")
  #pause(duration: 600)
  #ecresume("r")
]
```

---

## Worked examples

| File | Demonstrates |
|---|---|
| `examples/dot_move.tyx` | `mobject` + `animate` + `pause` (the simplest clip) |
| `examples/box_anim.tyx` | absolute `to:` + absolute `scale:` |
| `examples/rotate_fade.tyx` | `rotate:` + `opacity:` + `there-and-back` easing |
| `examples/transform_demo.tyx` | `#transform` on shapes **and** formulas (keeps the label) |
| `examples/manim_features.tyx` | `save_state`/`restore`, `wiggle`, `flash`, `indicate`, `appear`/`disappear` |
| `examples/composite_demo.tyx` | `blink`, `spiral-in`, `fade-transform`, `focus-on` |
| `examples/full_demo.tyx` | `move-along-path` + `morph` + `indicate` |
| `examples/modules_demo.tyx` | `ecnew`/`ecval`, custom `expr:`/`bezier:` easing, `subtitle`, counter control |
| `examples/preview_demo.tyx` | external `@preview` package (`cetz`) resolved in-process |
| `examples/custom_page.tyx` | custom page size via `#set page` |
| `examples/play_demo.tyx` | `#play` beat-by-beat reveal |
| `examples/audio_demo.tyx` | `#audio` + visual sync |
| `examples/video_placeholder_demo.tyx` | `#video` placeholder box |
| `examples/zoom_transition_demo.tyx` | `#zoom-to` + `#transition` + `#set_color` |
| `examples/easing_showcase.tyx` | all named easings + custom `expr:`/`bezier:` |
| `examples/math_derivation.tyx` | long step-by-step equation derivation via `#transform` + `#subtitle` + `#indicate`/`#flash` + `#typewriter` |
| `examples/constellation.tyx` | `#spiral-in` stars + sequential line fade-in + `#wiggle` sparkle (nested night-sky scene) |
| `examples/geometric_construction.tyx` | step-by-step regular-hexagon construction + `#group` + `#animate(rotate:)` symmetry spin |
| `examples/camera_tour.tyx` | cinematic `#camera` pan/zoom/rotate tour + nested title-card scene + `#transition` |
| `examples/data_viz.tyx` | animated horizontal bar chart "race" via `#transform` (reused labels) + `#indicate` leader + `#typewriter` |
| `examples/orbit_demo.tyx` | `#group` ring spin + `#track` orbiting planet + `#camera` push-in (orrery) |
| `examples/projectile_demo.tyx` | nested title scene + `#track` parabolic flight + `#ecnew`/`#ecval` live timer + `#camera` push-in + `#transform` of the trajectory equation |

See each file in `examples/` for the full, runnable source. Build any of them with:

```sh
candy build examples/<name>.tyx
```

---

## Full documentation

This README is the quick reference. For the complete guide, see the repository docs:

- [Tutorial](https://github.com/ViCo-Lab/candy/blob/main/docs/tutorial/README.md) — install,
  first clip, animation basics, transforms, scenes/camera/groups, subtitles & counters,
  output & codecs.
- [Reference](https://github.com/ViCo-Lab/candy/blob/main/docs/reference/README.md) —
  directives, easing, counters, CLI flags, codec matrix, and the error model.

The Rust backend developer reference is in
[`rust/README.md`](https://github.com/ViCo-Lab/candy/blob/main/rust/README.md).
