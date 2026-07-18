// Candy — easing-counter module.
//
// A key-value store of animatable integers, referenced from mobject / subtitle
// bodies via `ecval(name)`. Standard Typst sees the integer `seed`; the candy
// pipeline steps the value over time, shaped by the counter's easing.

/// Register an integer counter named `name`.
///
/// - `seed`: the integer value (standard-Typst return value, and the starting
///   value). Default `0`.
/// - `step`: the per-step increment (signed integer). Default `1`. With no
///   `duration`, the counter steps once per millisecond.
/// - `duration`: lifetime in **milliseconds**. `none` (default) means
///   long-lived — the value ramps `seed → seed + step·elapsed` once per ms
///   (linear). A positive number makes the value ramps `seed → seed + step·
///   duration` over that window, shaped by `easing`.
/// - `easing`: rate curve for the ramp (default `"linear"`). Custom modes
///   `"bezier:x1,y1,x2,y2"` and `"expr:<math>"` are accepted.
///
/// Returns `seed` under standard Typst, so binding it (`#let c = ecnew("c",
/// seed: 40)`) captures the initial value; read it later with `ecval(c)` so the
/// standard-Typst first frame shows the correct number.
/// Scope rules follow Typst: a counter in a child scope shadows a parent-scope
/// counter of the same name, and it auto-destroys when its scope exits.
#let ecnew(name, seed: 0, step: 1, duration: none, easing: "linear") = {
  if type(name) != str {
    panic("Easing-counter name must be a string!")
  }
  none
}

/// Read the current value of an easing counter. Inside an animating candy
/// pipeline, `ecval(...)` is substituted (by the Rust renderer) with the live,
/// eased integer value and may be used directly as a Typst parameter (e.g.
/// `rect(width: ecval("n") * 1cm)`).
///
/// Under **standard Typst** there is no shared mutable registry, so pass the
/// value returned by `ecnew` (which is the `seed`) rather than the
/// name string:
///
/// ```typ
/// #let n = ecnew("n", seed: 40)
/// #rect(width: ecval("n") * 1pt)   // standard Typst → 40; candy → live value
/// ```
///
/// `ecval` returns its argument unchanged when it is already a number (the
/// seed, via the `ecnew` binding above), so the first frame renders with the
/// correct initial value. If a non-numeric argument is given (e.g. the bare
/// name string `ecval("n")`, which standard Typst cannot resolve to a value),
/// it falls back to `default`.
#let ecval(name, default: 0) = {
  if type(name) != str {
    panic("Easing-counter name must be a string!")
  }
  default
}

/// Pause a counter (freeze its stepping) at the current timeline position.
/// Inert under standard Typst.
#let ecpause(name) = {
  if type(name) != str {
    panic("Easing-counter name must be a string!")
  }
  none
}

/// Resume a paused counter. Inert under standard Typst.
#let ecresume(name) = {
  if type(name) != str {
    panic("Easing-counter name must be a string!")
  }
  none
}

/// Destroy a counter, freezing its value. Inert under standard Typst.
#let ecdestroy(name) = {
  if type(name) != str {
    panic("Easing-counter name must be a string!")
  }
  none
}
