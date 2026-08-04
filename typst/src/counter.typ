// Candy — easing-counter module.
//
// A key-value store of animatable integers, referenced from mobject / subtitle
// bodies via `ecval(name)`. Standard Typst sees the integer `seed`; the candy
// pipeline steps the value over time, shaped by the counter's easing.

#import "validation.typ": *

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
  _assert_str(name, "Easing-counter name")
  _assert_valid_key_name(name, "Easing-counter name")
  _assert_int(seed, "ecnew seed")
  _assert_int(step, "ecnew step")
  if duration != none {
    _assert_nonneg(duration, "duration")
  }
  _assert_easing(easing, "easing")
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

// ===========================================================================
// Keyframe counter module (`kc*`)
//
// A keyframe counter is driven by discrete keyframes pushed at runtime, rather
// than a single `seed + step` ramp. `kcnew` registers it (with a `seed` held
// before the first keyframe); `kcpush` appends a `(time → value)` keyframe at
// the call site's natural timeline position (no `at` argument — the time is the
// cursor where `kcpush` is called, optionally shifted by `offset`); `kcval`
// reads the live interpolated value; `kcpause` / `kcresume` / `kcdestroy` drive
// its lifecycle. It mirrors the easing-counter module's semantics and exception
// handling (scope shadowing, `E006 UnknownKey` for unknown / out-of-scope names,
// standard-Typst fallback to `none`).
// ===========================================================================

/// Register a keyframe counter named `name`.
///
/// - `seed`: the integer value held before the first keyframe (and when no
///   keyframes are pushed). Default `0`.
/// - `easing`: default easing used by `inherit` on the first keyframe. Default
///   `"linear"`.
///
/// Returns `none` under standard Typst (the live value is supplied by the candy
/// pipeline). Scope rules follow Typst: a counter in a child scope shadows a
/// parent-scope counter of the same name, and it auto-destroys when its scope
/// exits.
#let kcnew(name, seed: 0, easing: "linear") = {
  _assert_str(name, "Keyframe-counter name")
  _assert_valid_key_name(name, "Keyframe-counter name")
  _assert_int(seed, "kcnew seed")
  _assert_easing(easing, "easing")
  none
}

/// Read the current interpolated value of a keyframe counter. Inside an
/// animating candy pipeline, `kcval(...)` is substituted (by the Rust renderer)
/// with the live integer value and may be used directly as a Typst parameter
/// (e.g. `rect(width: kcval("n") * 1cm)`).
///
/// Under **standard Typst** there is no shared mutable registry, so pass the
/// value returned by `kcnew` (which is the `seed`) rather than the name string:
///
/// ```typ
/// #let k = kcnew("k", seed: 40)
/// #rect(width: kcval("k") * 1pt)   // standard Typst → 40; candy → live value
/// ```
///
/// `kcval` returns its argument unchanged when it is already a number (the
/// seed, via the `kcnew` binding above), so the first frame renders with the
/// correct initial value. If a non-numeric argument is given (e.g. the bare
/// name string `kcval("k")`, which standard Typst cannot resolve to a value),
/// it falls back to `default`.
#let kcval(name, default: 0) = {
  if type(name) != str {
    panic("Keyframe-counter name must be a string!")
  }
  default
}

/// Push a keyframe onto a keyframe counter at the call site's natural timeline
/// position. There is **no `at` argument** — the keyframe's time is the timeline
/// cursor where `kcpush` is called, optionally shifted by `offset` ms.
///
/// - `value`: the integer value reached at this keyframe.
/// - `offset`: integer ms added to the push time (default `0`); lets a keyframe
///   land earlier or later on the timeline. An `offset` that would make this
///   keyframe pierce a neighbouring one is clamped into the valid interval
///   (with a warning) rather than errored.
/// - `easing`: the easing for the segment that *starts* at this keyframe.
///   Default `"inherit"` — take the previous keyframe's easing, else the
///   counter's default from `kcnew`. The easing set here controls this node →
///   next node; ignored if there is no following keyframe.
///
/// Inert under standard Typst.
#let kcpush(name, value, offset: 0, easing: "inherit") = {
  _assert_str(name, "Keyframe-counter name")
  _assert_int(value, "kcpush value")
  _assert_int(offset, "kcpush offset")
  _assert_easing_or_inherit(easing, "easing")
  none
}

/// Pause a keyframe counter (freeze its stepping) at the current timeline
/// position. Inert under standard Typst.
#let kcpause(name) = {
  if type(name) != str {
    panic("Keyframe-counter name must be a string!")
  }
  none
}

/// Resume a paused keyframe counter. Inert under standard Typst.
#let kcresume(name) = {
  if type(name) != str {
    panic("Keyframe-counter name must be a string!")
  }
  none
}

/// Destroy a keyframe counter, freezing its value. Inert under standard Typst.
#let kcdestroy(name) = {
  if type(name) != str {
    panic("Keyframe-counter name must be a string!")
  }
  none
}
