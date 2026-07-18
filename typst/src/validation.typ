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

// Assert `v` is a length; otherwise panic.
#let _assert_length(v, what) = {
  if type(v) != length {
    panic(what + " must be a length")
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
