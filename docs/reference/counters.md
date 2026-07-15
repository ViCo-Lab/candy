# Counters & `ecval`

The easing-counter module lets you drive Typst parameters (widths, radii, text content)
with live, animatable integers. The substitution happens in the Rust renderer, so the
same `.tyx` compiles under plain `typst compile` with the `seed` value.

A counter is a key-value store of animatable integers, referenced from mobject / subtitle
bodies via `ecval(name)`. Standard Typst sees the integer seed; the Candy pipeline steps
the value over time, shaped by the counter's easing.

## `#ecounter(name, seed: 0, step: 1, duration: none, easing: "linear")`

Register an integer counter. Returns `seed` under standard Typst (so binding it captures
the initial value). With no `duration`, the counter steps once per millisecond; a positive
`duration` ramps `seed → seed + step·duration` over that window, shaped by `easing`.

## `#ecval(value, default: 0)`

Read the current value of an easing counter. Inside Candy's pipeline it is substituted with
the live, eased integer and may be used directly as a Typst parameter
(`rect(width: ecval(n) * 1cm)`). Under standard Typst it returns its argument unchanged when
it is already a number, so bind the `ecounter` result (`#let n = ecounter("n")`) and pass
`n`.

## `#counter_pause(name)` / `#counter_resume(name)` / `#counter_destroy(name)`

Pause / resume / freeze a counter. Inert under standard Typst.

```typst
#let r = ecounter("r", seed: 40, step: 1)
#mobject("dot", circle(radius: ecval(r) * 1pt + 1cm, fill: blue))
#pause(duration: 600)
#counter_pause("r")
#pause(duration: 600)
#counter_resume("r")
#counter_destroy("r")
```

## Full example

```typst
#scene(width: 16cm, height: 9cm)[
  #let r = ecounter("r", seed: 40, step: 1)
  #mobject("dot", circle(radius: ecval(r) * 1pt + 1cm, fill: blue))
  #animate("dot", to: (0cm, 5cm), duration: 2000, easing: "bezier:0.25,0.1,0.25,1.0")
  #subtitle([r = #str(ecval(r))], position: "bottom")
  #counter_pause("r")
  #pause(duration: 600)
  #counter_resume("r")
]
```
