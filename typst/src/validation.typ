// Candy — argument validation helpers for the Typst pseudo-interfaces.
//
// Every directive is inert under standard Typst, but we still validate
// argument types and enum values so misuse fails loudly at compile time
// (via `panic`) instead of producing an AST the Rust parser cannot interpret.
// Helpers are private (leading underscore) and are not part of the public API.

// Assert `v` is a string; otherwise panic naming `what`.
#let _assert_str(v, what) = {
  if type(v) != str {
    panic(what + " must be a string")
  }
}

// Assert `v` is one of `allowed` (an array of strings); otherwise panic.
#let _assert_enum(v, allowed, what) = {
  if type(v) != str or allowed.contains(v) == false {
    panic(what + " must be one of " + repr(allowed))
  }
}

// Assert `v` is a non-negative number (int or float); otherwise panic.
#let _assert_nonneg(v, what) = {
  if (type(v) != int and type(v) != float) or v < 0 {
    panic(what + " must be a non-negative number")
  }
}

// Assert `v` is a number (int or float, may be negative); otherwise panic.
#let _assert_number(v, what) = {
  if type(v) != int and type(v) != float {
    panic(what + " must be a number")
  }
}

// Assert `v` is a number or a length (e.g. `2cm`); otherwise panic. Used for
// geometry offsets that accept either a bare number or a length.
#let _assert_scalar(v, what) = {
  if type(v) != int and type(v) != float and type(v) != length {
    panic(what + " must be a number or a length")
  }
}

// Assert `v` is an integer; otherwise panic.
#let _assert_int(v, what) = {
  if type(v) != int {
    panic(what + " must be an integer")
  }
}

// Assert `v` is a boolean; otherwise panic.
#let _assert_bool(v, what) = {
  if type(v) != bool {
    panic(what + " must be a boolean")
  }
}

// Assert `v` is an array; otherwise panic.
#let _assert_array(v, what) = {
  if type(v) != array {
    panic(what + " must be an array")
  }
}

// Assert `v` is a length or a relative length; otherwise panic. A plain
// `length` is absolute (e.g. `2cm`, `10pt`); a `relative` length is a mix of an
// absolute part and a ratio part (e.g. `1cm + 10%`, `50% + 2pt`). Both are
// accepted so callers may use mixed lengths.
#let _assert_length(v, what) = {
  if type(v) != length and type(v) != relative {
    panic(what + " must be a length or a relative length (e.g. `2cm`, `1cm + 10%`)")
  }
}

// Assert `v` is a Typst ratio (e.g. `50%`, `100%`); otherwise panic. A bare
// number such as `0.5` is NOT a ratio and is rejected — callers must pass a
// percentage value, not a fraction. Used for ratio-style parameters such as
// `opacity`.
#let _assert_ratio(v, what) = {
  if type(v) != ratio {
    panic(what + " must be a ratio (e.g. `50%`)")
  }
}


// Assert `v` is a Typst angle (e.g. `90deg`, `1.5rad`); otherwise panic. A
// bare number such as `90` is NOT an angle and is rejected — callers must pass
// a real angle value with a unit. Used for angle-style parameters such as the
// camera `rotate`.
#let _assert_angle(v, what) = {
  if type(v) != angle {
    panic(what + " must be an angle (e.g. `90deg`)")
  }
}

// Assert `v` lies in the closed interval [lo, hi]; otherwise panic.
#let _assert_range(v, lo, hi, what) = {
  if (type(v) != int and type(v) != float) or v < lo or v > hi {
    panic(what + " must be in [" + str(lo) + ", " + str(hi) + "]")
  }
}

// Assert `v` is a valid timing enum ("after" or "with").
#let _assert_timing(v) = {
  _assert_enum(v, ("after", "with"), "timing")
}

// The named easing curves candy understands, in kebab-case. This mirrors the
// Rust `Easing::from_str` vocabulary (including its aliases) exactly, so a name
// accepted here is accepted by the renderer and vice versa. Comparison is done
// after lower-casing and turning `_` into `-`, so `"Ease_In_Out"` is accepted.
#let _EASING_NAMES = (
  "linear",
  "smooth", "sigmoid",
  "smoothstep", "smootherstep",
  "quad", "quad-in", "ease-in-quad",
  "quad-out", "ease-out-quad",
  "quad-in-out", "ease-in-out-quad",
  "cubic", "cubic-in", "ease-in-cubic",
  "cubic-out", "ease-out-cubic",
  "cubic-in-out", "ease-in-out-cubic",
  "ease-in", "ease-out", "ease-in-out",
  "sin", "sine", "ease-out-sine",
  "there-and-back", "wiggle", "lingering",
)

// Cheap numeric-string test used to validate custom `bezier:` control points
// without letting `float(...)` raise its own (less specific) error. Accepts an
// optional sign, digits, a single decimal point, and an exponent — enough to
// reject obvious garbage like `bezier:a,b,c,d`.
#let _is_number(s) = {
  let t = s.trim()
  if t == "" { return false }
  let allowed = "0123456789.eE+-"
  for c in t {
    if not allowed.contains(c) { return false }
  }
  true
}

// Assert `v` is a valid easing-curve string — one of the "special format"
// strings candy accepts. Three forms are allowed:
//   1. a named curve from `_EASING_NAMES` (case-insensitive; `_`/`-`
//      interchangeable), e.g. `"smooth"`, `"ease-in-out"`;
//   2. a custom CSS-style cubic bezier `"bezier:x1,y1,x2,y2"` — exactly four
//      numbers;
//   3. a custom math expression `"expr:<math>"` — a non-empty function of `t`.
// Anything else panics so easing typos are caught at compile time rather than
// silently falling back to linear.
#let _assert_easing(v, what) = {
  if type(v) != str {
    panic(what + " must be a string naming an easing curve")
  }
  let raw = v.trim()
  if raw.starts-with("bezier:") {
    let parts = raw.slice("bezier:".len()).split(",")
    if parts.len() != 4 {
      panic(what + " must be `bezier:x1,y1,x2,y2` with exactly four numbers")
    }
    for p in parts {
      if not _is_number(p) {
        panic(what + " bezier control points must be numbers: `bezier:x1,y1,x2,y2`")
      }
    }
    return
  }
  if raw.starts-with("expr:") {
    if raw.slice("expr:".len()).trim() == "" {
      panic(what + " math expression (`expr:<math>`) must not be empty")
    }
    return
  }
  let norm = lower(raw).replace("_", "-")
  if not _EASING_NAMES.contains(norm) {
    panic(what + " is not a known easing curve; use a named curve, `bezier:x1,y1,x2,y2`, or `expr:<math>` (got " + repr(v) + ")")
  }
}

// Assert `v` is a native Typst color (e.g. `red`, `white`, `rgb(255,0,0)`,
// `rgb("#ff0000")`, `luma(50)`); otherwise panic. A string such as `"red"` or
// `"#ff0000"` is NOT a color and is rejected — callers must pass a real color
// value, not a string that merely names one.
#let _assert_color(v, what) = {
  if type(v) != color {
    panic(what + " must be a native Typst color (e.g. `red`, `rgb(255,0,0)`), not a string")
  }
}
