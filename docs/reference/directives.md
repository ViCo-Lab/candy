# Directives reference

Every directive is *valid, standard Typst* — under `typst compile` it either returns
`body` or `none`, so a `.tyx` is simultaneously a normal Typst document and a Candy
animation script. Argument tables list the directive's parameters and meaning.

All directives validate their argument types and enum values at compile time (via
`panic`); misuse fails loudly instead of producing an AST the Rust parser cannot
interpret.

## Timing & sequencing

Object animations accept two extra timing parameters that control how the animation is
sequenced relative to the *previous* animation on the timeline (mirroring the **Start**
options in the PowerPoint animation pane):

- `timing:` — `"after"` (default) starts this animation once the previous one finishes;
  `"with"` starts it at the same time as the previous one (parallel).
- `delay:` — an extra wait in **milliseconds** before this animation begins, on top of
  `timing` (default `0`).

Scene animations (`scene` / `scene-switch` / `transition` / `camera` / `zoom-to`) and the
mask (`subtitle`) do **not** accept `timing` / `delay`.

## Scene & camera

These are scene animations: no `timing` / `delay`.

### `#scene(name: none, width: 16cm, height: 9cm, bg: white, body)`

Define a scene (a "slide"). See the Tutorial for full semantics. Under standard Typst
this sets the page and renders `body`.

### `#scene-switch(target, duration: 0, easing: "smooth")`

Jump the timeline cursor to a named scene (`target` is the scene's `name:` or its
auto-assigned UUID-like name). `duration: 0` is an instant jump. Inert under standard
Typst.

### `#transition(kind: "cut", duration: 100)`

Mark a slide transition. `kind`: `"cut"` (instant, default), `"fade"` (crossfade),
`"slide"` (push). Only `"cut"` is fully implemented. Inert under standard Typst.

### `#camera(x: 0, y: 0, zoom: 1.0, rotate: 0deg, duration: 1000, easing: "smooth")`

A global camera move (pan + zoom + rotate) applied to the whole scene. `x` / `y` are a
pan offset in cm from the page center; `zoom > 1` magnifies; `rotate` tilts clockwise in
degrees. Scene-scoped. Inert under standard Typst.

```typst
#camera(zoom: 2.0, x: -3cm, y: 1.5cm, duration: 1500, easing: "smooth")
#camera(zoom: 1.0, rotate: 12deg, duration: 1500, easing: "smooth")
```

### `#zoom-to(rect, duration: 500, easing: "smooth")`

Zoom-to-region: enlarge a rectangle of the canvas to fill the frame over `duration`
milliseconds. `rect` is `(x, y, w, h)` in cm, relative to the page origin. Implemented as
a scale + translate on all mobjects. Inert under standard Typst.

```typst
#zoom-to((4, 3, 6, 4), duration: 1000, easing: "smooth")
```

## Mobjects & definitions

These register or group animatable content; they are not animations.

### `#mobject(label, body)`

Register an animatable object. `label` is a unique string id; `body` is a bare block or
element (never a string). Under standard Typst this simply renders `body` at its natural
position.

```typst
#mobject("dot", circle(radius: 1cm, fill: blue))
```

### `#group(name, members: ())`

Group several mobjects under a synthetic parent so they move / scale / rotate together.
Animate the `name` afterwards (e.g. `#animate("g", rotate: 360deg)`) to transform every
member at once. Groups may be nested.

```typst
#group("wheel", members: ("spoke1", "spoke2", "hub"))
#animate("wheel", rotate: 360deg, duration: 3000, easing: "linear")
```

### `#video(path, width: 8cm, height: 5cm)`

Insert a **video reference** as a placeholder mobject. Typst cannot embed video, so
Candy renders a labeled placeholder box (rounded rect + ▶ icon + filename). The
placeholder behaves like any other mobject body (can be animated). To show the real first
frame, extract it with ffmpeg and use `#mobject("vid", image(...))` instead.

```typst
#mobject("clip", video("intro.mp4", width: 10cm, height: 6cm))
#animate("clip", scale: 1.2, duration: 500, easing: "smooth")
```

## Object animations

These target a mobject (or, for `#audio`, a media track) and **accept `timing:` and
`delay:`** (see [Timing & sequencing](#timing--sequencing)). Under standard Typst they
are inert (return `none`), except where noted.

### `#animate(target, ..)`

Animate `target` over `duration` milliseconds (default `500`). Supports absolute and relative
transforms in any combination; each produces a parallel action.

| Argument | Meaning |
|---|---|
| `to: (x, y)` | absolute target point in lengths, e.g. `(4cm, 0pt)` |
| `dx:` / `dy:` | relative offset in cm (Manim-style `shift`), e.g. `dx: 2cm` |
| `scale:` | absolute scale factor (e.g. `1.5`) |
| `scale-by:` | relative scale multiplier (e.g. `1.5` grows 50%) |
| `rotate:` | absolute clockwise rotation in degrees (e.g. `45deg`) |
| `rotate-by:` | relative rotation in degrees (e.g. `15deg` adds 15°) |
| `opacity:` | target opacity as a ratio in `[0%, 100%]` (e.g. `50%`) |
| `duration:` | length of the animation in **milliseconds** (default `500`) |
| `easing:` | rate curve (default `"smooth"`; see [Easing](easing.md)) |
| `timing:` | `"after"` (default) or `"with"` — sequencing vs the previous animation |
| `delay:` | extra wait in **milliseconds** before start (default `0`) |

```typst
#animate("dot", to: (4cm, 0pt), duration: 1000, easing: "linear")
#animate("box", scale: 1.5, duration: 800, easing: "smooth")
#animate("sq", dx: 2cm, rotate-by: 90deg, opacity: 50%, duration: 600, timing: "with")
```

### `#appear(target, timing: "after", delay: 0)` / `#disappear(target, timing: "after", delay: 0)`

Make a mobject visible instantly (`opacity: 100%`) or invisible instantly (`opacity:
0%`), with no interpolation. Useful for appear/disappear-without-fading effects. Inert
under standard Typst.

### `#save-state(target, slot: "default", timing: "after", delay: 0)`

Snapshot a mobject's current transform (x / y / scale / rotation / opacity) into a named
slot. Mirrors `mobject.save-state()`.

### `#restore(target, slot: "default", duration: 500, easing: "smooth", timing: "after", delay: 0)`

Interpolate back to a previously saved state. Mirrors `Restore(mobject)`.

```typst
#save-state("dot", slot: "home")
#animate("dot", to: (3cm, 2cm), duration: 800)
#restore("dot", slot: "home", duration: 200, easing: "cubic-in-out")
```

### `#indicate(target, factor: 1.1, dx: 0.0, dy: 0.0, duration: 300, easing: "smooth", timing: "after", delay: 0)`

Briefly scale + shift a mobject, then return — a transient "look here" effect. Mirrors
`Indicate`.

### `#flash(target, factor: 2.0, duration: 200, easing: "smooth", timing: "after", delay: 0)`

Briefly scale up and fade toward transparent, then restore — a "flash" attention effect.
Mirrors `Flash`.

### `#wiggle(target, degrees: 15deg, duration: 500, easing: "wiggle", timing: "after", delay: 0)`

Oscillate rotation by ±`degrees` a few times, then return. Mirrors `Wiggle`.

### `#set-color(target, color: black, duration: 1, easing: "linear", timing: "after", delay: 0)`

Record a color change for a mobject. The color is tracked in the timeline, but the current
renderer treats it as a no-op (Typst bodies are opaque strings). Future versions with
structured mobjects will apply it. Mirrors `set-color`.

```typst
#set-color("dot", color: red, duration: 300, easing: "smooth")
```

### `#blink(target, blinks: 3, duration: 500, easing: "smooth", timing: "after", delay: 0)`

Alternate opacity 1↔0 `blinks` times. Mirrors `Blink`.

### `#spiral-in(target, scale: 3.0, rotate: 360deg, duration: 300, easing: "smooth", timing: "after", delay: 0)`

Fly in from a scaled-up, rotated, invisible state to the natural position. Mirrors
`SpiralIn`.

### `#focus-on(target, factor: 0.5, duration: 300, easing: "smooth", timing: "after", delay: 0)`

Shrink a "spotlight" onto the target (scale down + dim). Mirrors `FocusOn`.

### `#fade-transform(from, to, duration: 300, easing: "smooth", timing: "after", delay: 0)`

Crossfade two pre-registered mobjects: fade out `from` while fading in `to`. Mirrors
`FadeTransform` (simple crossfade variant).

### `#move-along-path(target, path, duration: 500, easing: "smooth", mode: "polyline", orient: false, timing: "after", delay: 0)`

Move `target` along a polyline through `path` (array of `(x, y)` points in cm, absolute).
The scheduler generates a keyframe at each point, distributed across `duration`. Mirrors
`MoveAlongPath` (linear paths; arcs/beziers approximated as polylines).

```typst
#move-along-path("ball", ((2, 2), (6, 5), (10, 2), (14, 4)), duration: 2000, easing: "smooth")
```

### `#morph(from, to, duration: 500, easing: "smooth", timing: "after", delay: 0)`

Morph one mobject into another by crossfading + scaling. Both must be registered via
`mobject`. This is the **simplified** Morph — true point-by-point morphing (Manim's
`Transform`) requires structured mobjects, which Candy's opaque-content model does not
support; the crossfade + scale variant is a reasonable approximation.

### `#transform(target, to: none, duration: 500, easing: "smooth", timing: "after", delay: 0)`

Morph a **single** mobject's content into new inline content — Candy's Manim-style
`Transform` / `ReplacementTransform`. `target`'s current body is smoothly replaced by
`to` (a Typst body — a shape or a formula such as `[$a + b + d = c$]`), and the **original
`target` label keeps the new content**, so you can keep animating it afterwards.

For **inline content** (formulas and text) the transform is **glyph-by-glyph**, not a
whole-block dissolve. Candy renders the old and new bodies with Typst's own SVG layout and
extracts every glyph and decoration (fraction bars, roots, …) as a positioned fragment,
then matches old↔new fragments by their outline signature (longest common subsequence).
During the window:

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

### `#reveal(target, by: "word", duration: 1000, easing: "smooth", timing: "after", delay: 0)`

Progressively reveal a *string* mobject by swapping its body to longer and longer prefixes
over `duration`. `by: "word"` reveals word-by-word; `by: "char"` reveals
character-by-character. Non-string bodies fall back to a plain `FadeIn` with a warning. The
body must be a string literal (`#mobject("cap", "Hello")`), not a content block. At frame
0 the target is `none`, but Candy keeps its layout box reserved (see
[Tutorial · first clip](../tutorial/first-clip.md#mobjects--actions--the-core-idea)), so
later content does not jump as it types in.

### `#typewriter(target, duration: 1000, easing: "smooth", timing: "after", delay: 0)`

Convenience alias for `#reveal(.., by: "char")` — a classic typewriter reveal.

```typst
#mobject("cap", "Step 1: divide by a.")
#typewriter("cap", duration: 1500, easing: "linear")
```

### `#track(target, keys: (), duration: 1000, easing: "smooth", timing: "after", delay: 0)`

Drive a single target through several keyframes, each controlling a subset of its
properties — a timeline track that removes the need for many sequential `#animate`s.
Mirrors a Manim `ValueTracker`-driven animation. `keys` is an array of
`(t, (x, y, scale, opacity, rotation))` tuples, where `t` is the time offset (ms) from the
slide start and each inner value is *optional* (omitted properties carry their previous
value forward); `x`/`y` are in cm, `scale`/`opacity`/`rotation` unitless. A keyframe may
also be written flat as `(t, x, y, scale, opacity, rotation)`.

```typst
#track("p",
  keys: (
    (0,    (0cm, 0cm, 1, 1, 0)),
    (1000, (3cm, 2cm, 1.5, 1, 90)),
    (2000, (4cm, 0cm, 1, 0, 0)),
  ),
  duration: 2000, easing: "smooth")
```

### `#audio(path, blocking: false, loop: false, volume: 1.0, slice: none, timing: "after", delay: 0)`

Insert a voice / audio track. Audio is an **object animation**, so it accepts `timing`
and `delay`. Inert under standard Typst.

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

## Content blocks

### `#play(body, duration: 500)`

Show `body` for `duration` milliseconds as its own animation unit (block-level, controllable
like a mobject). `play` is a self-contained content block and does **not** accept `timing`
/ `delay`. Under standard Typst the body is shown in the first frame.

```typst
#play([#text(28pt, weight: "bold")[Step 1 of 3]], duration: 1000)
#play([#text(28pt, weight: "bold")[Step 2 of 3]], duration: 1000)
```

### `#pause(duration: 500)`

Hold the current frame for `duration` milliseconds. Inert under standard Typst.

## Subtitles (masks)

### `#subtitle(body, duration: none, position: "bottom", easing: "linear")`

Overlay `body` (any Typst block content) on top of the animation. A subtitle is a
**mask/overlay** and does **not** accept `timing` / `delay`.

- `duration:` lifetime in **milliseconds**. `none` (default) means *persist* — the caption
  stays until replaced by another `#subtitle` in the same Typst scope, or until that scope
  exits (auto-destroy). A positive number gives an explicit lifetime.
- `position:` anchor — `"bottom"` (default), `"top"`, `"center"`, `"bottom-left"`,
  `"bottom-right"`, `"top-left"`, `"top-right"`, or a tuple `(x, y)` in cm for an absolute
  position.
- `easing:` rate curve for the caption's own fade (default `"linear"`). Custom modes
  `"bezier:x1,y1,x2,y2"` and `"expr:<math>"` are accepted.

Only one subtitle may be visible per Typst scope at a time; a later one replaces an
earlier one. A subtitle in a parent scope is temporarily hidden while a child scope shows
its own (shadowing).

```typst
#subtitle([Long-lived caption], position: "bottom")
#[
  #subtitle([Child scope caption], position: "top", duration: 800)
  #pause(duration: 800)
]
```

## Easing counters

### `#ecnew(name, seed: 0, step: 1, duration: none, easing: "linear")`

Register an integer counter. Returns `seed` under standard Typst (so binding it captures
the initial value). With no `duration`, the counter steps once per millisecond; a positive
`duration` ramps `seed → seed + step·duration` over that window, shaped by `easing`.

### `#ecval(value, default: 0)`

Read the current value of an easing counter. Inside candy's pipeline it is substituted with
the live, eased integer and may be used directly as a Typst parameter
(`rect(width: ecval(n) * 1cm)`). Under standard Typst it returns its argument unchanged
when it is already a number, so bind the `ecnew` result (`#let n = ecnew("n")`) and pass
`n`.

### `#ecpause(name)` / `#ecresume(name)` / `#ecdestroy(name)`

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

## Helpers & constants

Direction vectors (cm): `dir-left` `dir-right` `dir-up` `dir-down` `dir-origin`
`dir-up-left` `dir-up-right` `dir-down-left` `dir-down-right`.

Scale factors: `grow` (1.5) `shrink` (0.5) `original` (1.0).

Turns (degrees): `quarter-turn` (90) `half-turn` (180) `full-turn` (360).

Opacity presets: `visible` (1.0) `half-visible` (0.5) `invisible` (0.0).

```typst
#animate("dot", dx: dir-right.at(0) * 1cm, dy: dir-up.at(1) * 1cm, duration: 500)
#animate("dot", scale-by: grow, duration: 500)
```
