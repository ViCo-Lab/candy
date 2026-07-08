/// Linear interpolation between two numbers.
///
/// - a (float): start value
/// - b (float): end value
/// - t (float): progress in `[0, 1]`
/// -> float: interpolated value
#let lerp(a, b, t) = {
  a + (b - a) * t
}

/// Smoothstep easing in `[0, 1]`.
///
/// - t (float): progress in `[0, 1]`
/// -> float: eased value
#let smooth(t) = {
  t * t * (3 - 2 * t)
}
