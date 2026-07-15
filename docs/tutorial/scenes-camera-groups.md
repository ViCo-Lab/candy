# Scenes, camera & groups

Candy groups content into **scenes** (independent animation segments), supports a global
**camera** move, and lets you **group** mobjects so they move together.

## `#scene` — independent segments

```typst
#scene(width: 16cm, height: 9cm, bg: white)[
  // all mobjects & actions here
]
```

`#scene` wraps the body in a `page()` call so each scene renders as an independent page.
The renderer uses the scene's page size as the canvas for every frame in that scene.
Without `#scene`, Candy defaults to 16 cm × 9 cm.

**Scene semantics**

- **Nesting** — a `scene` may appear inside another scene's body, forming a child scene.
  Nesting is resolved through the Typst AST, so import style is irrelevant.
- **Parent auto-hide** — entering a child scene automatically hides its parent (and any
  ancestor) for the child's duration. The renderer always shows the *deepest* active
  scene at each frame time, so the child visually replaces the parent.
- **Typst scope** — a mobject / `play` / `subtitle` belongs to the innermost `scene`
  whose body encloses it (the scope in which it is evaluated).
- **Cross-page scene** — a scene's `width`/`height` set the size of *each* page. Content
  overflowing the page spills onto subsequent pages; the mobjects stay in one scene
  (shared data) but the renderer plays the pages **in sequence** on a single-page canvas
  (the canvas does *not* grow). Each page has its own timeline; the other pages stay
  frozen until the current page finishes and the renderer auto-advances.
- **Auto-split** — content spanning multiple pages is automatically split into multiple
  scenes (one per page) when no explicit root `scene` wraps it.
- **Implicit root** — with no `scene` call, the entire document is one implicit root
  scene that still follows the one-page / split rules (default 16 cm × 9 cm). A child
  scene inherits its page size from the nearest ancestor that declares one.

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

## `#group` — move mobjects together

`#group(name, members: ())` groups several mobjects under a synthetic parent so they
move / scale / rotate together. Animate the `name` afterwards (e.g.
`#animate("g", rotate: 360)`) to transform every member at once. Groups may be nested.
The group's rotation pivots about the figure's centroid, so a ring of objects placed
around a center spins in place.

```typst
#group("wheel", members: ("spoke1", "spoke2", "hub"))
#animate("wheel", rotate: 360, duration: 3000, easing: "linear")
```

## `#camera` — a global move

`#camera(x: 0, y: 0, zoom: 1.0, rotate: 0, duration: 1000, easing: "linear")` applies a
global camera move to the whole scene (pan + zoom + rotate), mirroring Manim's camera
frame transforms. `x` / `y` are a pan offset in cm from the page center; `zoom > 1`
magnifies; `rotate` tilts clockwise in degrees. The camera is scene-scoped: it only
transforms the scene active when the `#camera` directive runs.

```typst
#camera(zoom: 2.0, x: -3cm, y: 1.5cm, duration: 1500, easing: "smooth")
#camera(zoom: 1.0, rotate: 12, duration: 1500, easing: "smooth")
```

## `#track` — a keyframe timeline

`#track(target, keys: (), duration: 1000, easing: "linear")` drives a single target
through several keyframes, each controlling a subset of its properties — a timeline track
that removes the need for many sequential `#animate`s. `keys` is an array of
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

## `#zoom-to` / `#transition`

`#zoom-to(rect, duration: 30, easing: "smooth")` zooms a rectangle of the canvas
(`(x, y, w, h)` in cm, relative to the page origin) to fill the frame over `duration`
frames — a "camera zoom" implemented as a scale + translate on all mobjects.

`#transition(kind: "cut", duration: 6)` marks a slide transition (`"cut"` between
scenes). `kind`: `"cut"` (instant, default), `"fade"` (crossfade), `"slide"` (push). Only
`"cut"` is fully implemented; the others are recorded for future versions.

```typst
#zoom-to((4, 3, 6, 4), duration: 1000, easing: "smooth")
```

Next: [Subtitles & counters](subtitles-counters.md).
