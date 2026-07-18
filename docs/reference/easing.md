# Easing reference

`easing:` accepts a named curve or a custom spec. It is accepted by `#animate`,
`#subtitle`, `#ecnew`, `#restore`, and the other timed directives.

## Named curves

Unknown names fall back to `linear` with a warning (W009).

`"linear"`, `"smooth"`, `"smoothstep"`, `"smootherstep"`,
`"quad-in"` / `"quad-out"` / `"quad-in-out"`,
`"cubic-in"` / `"cubic-out"` / `"cubic-in-out"` (aliases `"ease-in"`, `"ease-out"`,
`"ease-in-out"`),
`"sin"` (sine ease-out), `"there-and-back"`, `"wiggle"`, `"lingering"`.

## Custom specs

- `expr:<math>` — a mathematical expression in `t` ∈ [0, 1], e.g.
  `"expr: 1 - (1 - t)^3"`.
- `bezier:x1,y1,x2,y2` — a cubic Bézier control-point spec (CSS-style easing curve),
  e.g. `"bezier:0.25,0.1,0.25,1.0"`.

```typst
#animate("sq", to: (10cm, 0cm), duration: 2000, easing: "expr: 1 - (1 - t)^3")
#animate("dot", to: (0cm, 5cm), duration: 2000, easing: "bezier:0.25,0.1,0.25,1.0")
```
