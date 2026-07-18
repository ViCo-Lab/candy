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
    panic(what + " must be a ratio (e.g. `50%`), not a number")
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

// Assert `v` is a native Typst color (e.g. `red`, `white`, `rgb(255,0,0)`,
// `rgb("#ff0000")`, `luma(50)`); otherwise panic. A string such as `"red"` or
// `"#ff0000"` is NOT a color and is rejected — callers must pass a real color
// value, not a string that merely names one.
#let _assert_color(v, what) = {
  if type(v) != color {
    panic(what + " must be a native Typst color (e.g. `red`, `rgb(255,0,0)`), not a string")
  }
}
