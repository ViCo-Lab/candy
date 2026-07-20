# Subtitles & counters

Overlay captions with `#subtitle`, and drive animatable integers with the easing-counter
module (`#ecnew` / `#ecval`).

## `#subtitle` — overlay captions

`#subtitle(body, duration: none, position: "bottom", easing: "linear")` overlays `body`
(any Typst block content) on top of the animation.

- `duration:` lifetime in **milliseconds**. `none` (default) means *persist* — the
  caption stays until replaced by another `#subtitle` in the same Typst scope, or until
  that scope exits (auto-destroy). A positive number gives an explicit lifetime.
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

## Easing counters — `#ecnew` / `#ecval`

A key-value store of animatable integers, referenced from mobject / subtitle bodies via
`ecval(name)`. Standard Typst sees the integer seed; the Candy pipeline steps the value
over time, shaped by the counter's easing.

`#ecnew(name, seed: 0, step: 1, duration: none, easing: "linear")` registers an
integer counter. It returns `seed` under standard Typst (so binding it captures the
initial value). With no `duration`, the counter steps once per millisecond; a positive
`duration` ramps `seed → seed + step·duration` over that window, shaped by `easing`.

`#ecval(value, default: 0)` reads the current value of an easing counter. Inside Candy's
pipeline it is substituted with the live, eased integer and may be used directly as a
Typst parameter (`rect(width: ecval(n) * 1cm)`). Under standard Typst it returns its
argument unchanged when it is already a number, so bind the `ecnew` result
(`#let n = ecnew("n")`) and pass `n`.

`#ecpause(name)` / `#ecresume(name)` / `#ecdestroy(name)` pause /
resume / freeze a counter. All inert under standard Typst.

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

The substitution happens in the Rust renderer, so the same `.tyx` compiles under plain
`typst compile` with the `seed` value.

Full reference: [Reference · Counters](../reference/counters.md).

Next: [Output & codecs](output.md).
