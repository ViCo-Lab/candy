# Scenes, camera & groups

Candy groups content into **scenes** (independent animation segments), supports a global
**camera** move, and lets you **group** mobjects so they move together.

## `#scene` — independent segments

```typst
#scene(name: "intro")[
  // all mobjects & actions here
]
```

`#scene` marks a segment of the timeline. It does **not** size the canvas and does **not**
paint a background: the viewport comes from the global `#show: candy` configuration (and
any `#set page`), and the background is whatever the page under the scene paints.

**Scene semantics**

- **Flat, never nested** — a `#scene(...)` call inside another scene's body is a *parse
  error*. There is only "switch scene", never "enter a sub-scene". Nesting is detected
  through the Typst AST, so import style is irrelevant.
- **Document structure** — a document must be either a sequence of parallel `#scene(...)`
  calls with **no content at the document root**, or root content with **no** `#scene(...)`
  call at all. Mixing the two is a parse error.
- **Implicit whole-document scene** — with no `#scene` call, the entire document is a
  single scene.
- **Typst scope** — a mobject / `play` / `subtitle` belongs to the `scene` whose body
  encloses it (the scope in which it is evaluated).
- **One page per scene** — a scene occupies one viewport. Content that overflows is
  reported as `W018` and clipped at rasterization; split it into more scenes instead.
- **Background** — set it on the page under the scene, e.g. `#set page(fill: black)` as
  the first line of the scene body.
- **Named scenes & switching** — `#scene(name: "foo")` can be jumped to with
  `#scene-switch(target: "foo")`. Anonymous scenes get auto-assigned names.

Unknown named arguments (including the removed `width` / `height` / `bg`) are a parse
error rather than being silently ignored.

```typst
// flat, sibling scenes: "outer" plays first, then "inner" takes over
#scene(name: "outer")[
  #mobject("a", circle(radius: 1cm, fill: blue))
  #animate("a", to: (4cm, 0pt), duration: 1000)
]
#scene(name: "inner")[
  #set page(fill: black)
  #mobject("b", square(size: 2cm, fill: red))
  #animate("b", to: (3cm, 2cm), duration: 800)
]
```

## `#group` — move mobjects together

`#group(name, members: ())` groups several mobjects under a synthetic parent so they
move / scale / rotate together. Animate the `name` afterwards (e.g.
`#animate("g", rotate: 360deg)`) to transform every member at once. Groups may be nested.
The group's rotation pivots about the figure's centroid, so a ring of objects placed
around a center spins in place.

```typst
#group("wheel", members: ("spoke1", "spoke2", "hub"))
#animate("wheel", rotate: 360deg, duration: 3000, easing: "linear")
```

## `#camera` — a global move

`#camera(x: 0, y: 0, zoom: 100%, rotate: 0deg, duration: 1000, easing: "linear")` applies a
global camera move to the whole scene (pan + zoom + rotate), mirroring Manim's camera
frame transforms. `x` / `y` are a pan offset in cm from the page center; `zoom > 100%`
magnifies; `rotate` tilts clockwise in degrees. The camera is scene-scoped: it only
transforms the scene active when the `#camera` directive runs.

```typst
#camera(zoom: 200%, x: -3cm, y: 1.5cm, duration: 1500, easing: "smooth")
#camera(zoom: 100%, rotate: 12deg, duration: 1500, easing: "smooth")
```

## `#track` — a keyframe timeline

`#track(target, keys: (), duration: 1000, easing: "smooth")` drives a single target
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

`#zoom-to(rect, duration: 500, easing: "smooth")` zooms a rectangle of the canvas
(`(x, y, w, h)` in cm, relative to the page origin) to fill the frame over `duration`
milliseconds — a "camera zoom" implemented as a scale + translate on all mobjects.

`#transition(kind: "cut", duration: 100)` marks a slide transition (`"cut"` between
scenes). `kind`: `"cut"` (instant, default), `"fade"` (crossfade), `"slide"` (push). Only
`"cut"` is fully implemented; the others are recorded for future versions.

```typst
#zoom-to((4, 3, 6, 4), duration: 1000, easing: "smooth")
```

Next: [Subtitles & counters](subtitles-counters.md).
