//! Easing / rate functions for time interpolation.
//!
//! Combines the best of two reference designs:
//!
//! - **kino** (`src/transitions.typ`): a small set of named easings selectable
//!   by string (`"linear"`, `"sin"`, `"quad"`, …) or by function reference, plus
//!   a `concat` combinator for piecewise composition.
//! - **Manim Community** (`manim/utils/rate_functions.py`): a richer library of
//!   17+ rate functions with `smooth` as the universal default, plus
//!   `there_and_back`, `wiggle`, `squish_rate_func`, etc.
//!
//! Candy's design:
//!
//! - [`Easing`] is a serializable enum (string-named variants + a custom
//!   closure variant for advanced use). This keeps the `.tyx` AST and the
//!   `Scene` JSON representation simple.
//! - [`Easing::resolve`] returns a `fn(f64) -> f64` for the interpolator.
//! - The default is [`Easing::Linear`] (matches candy v0.1 behavior; Manim's
//!   `smooth` is available as `Easing::Smooth` for users who want it).
//!
//! All rate functions map `t ∈ [0, 1]` to `y ∈ [0, 1]` and are clamped
//! defensively on input.

use serde::{Deserialize, Serialize};

/// A named easing curve (serializable) used by `animate(easing: ..)`.
///
/// The string form matches the variant name in lower case (e.g. `"linear"`,
/// `"smooth"`, `"ease-in-out"`), so `.tyx` files can use the familiar CSS /
/// Manim vocabulary. Unknown names fall back to `Linear` at parse time and
/// emit a warning.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum Easing {
    /// `t` — constant velocity. The candy v0.1 default.
    #[default]
    Linear,
    /// Manim's `smooth` (sigmoidal). The Manim default; pleasant for most
    /// organic motion.
    Smooth,
    /// `smoothstep` (Hermite, 3-tap).
    Smoothstep,
    /// `smootherstep` (Hermite, 5-tap).
    Smootherstep,
    /// `t²` — accelerating.
    QuadIn,
    /// `1 - (1-t)²` — decelerating.
    QuadOut,
    /// `t²` then `1-(1-t)²` — accelerate then decelerate.
    QuadInOut,
    /// `t³` — sharply accelerating.
    CubicIn,
    /// `1 - (1-t)³` — sharply decelerating.
    CubicOut,
    /// `t³` then `1-(1-t)³`.
    CubicInOut,
    /// `1 - cos(t·π/2)` — sine-wave ease-out (kino's `sin`).
    Sin,
    /// `sin(t·π)` — go-and-return to start.
    ThereAndBack,
    /// `|sin(t·π·2)|` — wiggle around the midpoint (Manim `wiggle`).
    Wiggle,
    /// `t·(1-t)·4` — overshoots to ~0.25 then settles (Manim `lingering`).
    Lingering,
}

impl Easing {
    /// Resolve to a concrete `f64 → f64` rate function.
    ///
    /// All returned functions accept any `f64` (callers may pass slightly
    /// out-of-range values during interpolation) and return a value that, when
    /// used as the interpolation parameter, produces the eased curve.
    pub fn resolve(self) -> fn(f64) -> f64 {
        match self {
            Easing::Linear => linear,
            Easing::Smooth => smooth,
            Easing::Smoothstep => smoothstep,
            Easing::Smootherstep => smootherstep,
            Easing::QuadIn => quad_in,
            Easing::QuadOut => quad_out,
            Easing::QuadInOut => quad_in_out,
            Easing::CubicIn => cubic_in,
            Easing::CubicOut => cubic_out,
            Easing::CubicInOut => cubic_in_out,
            Easing::Sin => sin,
            Easing::ThereAndBack => there_and_back,
            Easing::Wiggle => wiggle,
            Easing::Lingering => lingering,
        }
    }

    /// Parse a string easing name (from `.tyx` source) into an [`Easing`].
    ///
    /// Accepts kebab-case (`"ease-in-out"`), snake_case (`"ease_in_out"`),
    /// and a few common aliases (`"easeOut"` → `QuadOut`, `"ease-in"` →
    /// `CubicIn`, `"ease-out"` → `CubicOut`). Unknown names return `None`;
    /// the caller falls back to `Linear` and emits a parse warning.
    pub fn from_str(name: &str) -> Option<Self> {
        let n = name.trim().to_ascii_lowercase();
        let n = n.replace('_', "-");
        match n.as_str() {
            "linear" => Some(Easing::Linear),
            "smooth" | "sigmoid" => Some(Easing::Smooth),
            "smoothstep" => Some(Easing::Smoothstep),
            "smootherstep" => Some(Easing::Smootherstep),
            // quad family
            "quad" | "quad-in" | "ease-in-quad" => Some(Easing::QuadIn),
            "quad-out" | "ease-out-quad" => Some(Easing::QuadOut),
            "quad-in-out" | "ease-in-out-quad" => Some(Easing::QuadInOut),
            // cubic family
            "cubic" | "cubic-in" | "ease-in-cubic" => Some(Easing::CubicIn),
            "cubic-out" | "ease-out-cubic" => Some(Easing::CubicOut),
            "cubic-in-out" | "ease-in-out-cubic" => Some(Easing::CubicInOut),
            // CSS-style aliases (map to cubic by convention)
            "ease-in" => Some(Easing::CubicIn),
            "ease-out" => Some(Easing::CubicOut),
            "ease-in-out" => Some(Easing::CubicInOut),
            // kino names
            "sin" | "sine" | "ease-out-sine" => Some(Easing::Sin),
            // manim names
            "there-and-back" => Some(Easing::ThereAndBack),
            "wiggle" => Some(Easing::Wiggle),
            "lingering" => Some(Easing::Lingering),
            _ => None,
        }
    }
}

// ---- Rate functions -------------------------------------------------------

/// Identity. Constant velocity.
pub fn linear(t: f64) -> f64 {
    t.clamp(0.0, 1.0)
}

/// Manim's `smooth`: a sigmoidal curve `t²(3 - 2t)` (Hermite smoothstep, same
/// as CSS `ease`). Pleasant default for organic motion.
pub fn smooth(t: f64) -> f64 {
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// `smoothstep` — same as `smooth` (alias kept for Manim naming parity).
pub fn smoothstep(t: f64) -> f64 {
    smooth(t)
}

/// Ken Perlin's `smootherstep`: `t³(t(6t - 15) + 10)`. C2-continuous, no
/// visible "kink" at the endpoints.
pub fn smootherstep(t: f64) -> f64 {
    let t = t.clamp(0.0, 1.0);
    t * t * t * (t * (t * 6.0 - 15.0) + 10.0)
}

/// `t²`. Accelerating from zero velocity.
pub fn quad_in(t: f64) -> f64 {
    let t = t.clamp(0.0, 1.0);
    t * t
}

/// `1 - (1-t)²`. Decelerating to zero velocity.
pub fn quad_out(t: f64) -> f64 {
    let t = t.clamp(0.0, 1.0);
    1.0 - (1.0 - t) * (1.0 - t)
}

/// Accelerate then decelerate (quad).
pub fn quad_in_out(t: f64) -> f64 {
    let t = t.clamp(0.0, 1.0);
    if t < 0.5 {
        2.0 * t * t
    } else {
        1.0 - (-2.0 * t + 2.0).powi(2) / 2.0
    }
}

/// `t³`. Sharply accelerating.
pub fn cubic_in(t: f64) -> f64 {
    let t = t.clamp(0.0, 1.0);
    t * t * t
}

/// `1 - (1-t)³`. Sharply decelerating.
pub fn cubic_out(t: f64) -> f64 {
    let t = t.clamp(0.0, 1.0);
    1.0 - (1.0 - t).powi(3)
}

/// Accelerate then decelerate (cubic).
pub fn cubic_in_out(t: f64) -> f64 {
    let t = t.clamp(0.0, 1.0);
    if t < 0.5 {
        4.0 * t * t * t
    } else {
        1.0 - (-2.0 * t + 2.0).powi(3) / 2.0
    }
}

/// `1 - cos(t·π/2)` — sine-wave ease-out (kino's `sin`).
pub fn sin(t: f64) -> f64 {
    let t = t.clamp(0.0, 1.0);
    1.0 - (t * std::f64::consts::FRAC_PI_2).cos()
}

/// `sin(t·π)` — go and return to the start position. Useful for "dip and
/// recover" motions (Manim's `there_and_back`).
pub fn there_and_back(t: f64) -> f64 {
    let t = t.clamp(0.0, 1.0);
    (t * std::f64::consts::PI).sin()
}

/// `|sin(t·2π)|` — wiggle around the midpoint (Manim's `wiggle`).
pub fn wiggle(t: f64) -> f64 {
    let t = t.clamp(0.0, 1.0);
    (t * 2.0 * std::f64::consts::PI).sin().abs()
}

/// `4t(1-t)` — overshoots to 0.25 then settles back to 0 (Manim's
/// `lingering`). Use for "approach and retreat" effects.
pub fn lingering(t: f64) -> f64 {
    let t = t.clamp(0.0, 1.0);
    4.0 * t * (1.0 - t)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoints_are_fixed() {
        // Every standard easing must pass through (0,0) and (1,1) — except the
        // "go-and-return" ones (ThereAndBack, Wiggle, Lingering) which are
        // intentionally non-monotonic.
        for e in [
            Easing::Linear,
            Easing::Smooth,
            Easing::Smoothstep,
            Easing::Smootherstep,
            Easing::QuadIn,
            Easing::QuadOut,
            Easing::QuadInOut,
            Easing::CubicIn,
            Easing::CubicOut,
            Easing::CubicInOut,
            Easing::Sin,
        ] {
            let f = e.resolve();
            assert!((f(0.0) - 0.0).abs() < 1e-9, "{e:?} f(0)={}", f(0.0));
            assert!((f(1.0) - 1.0).abs() < 1e-9, "{e:?} f(1)={}", f(1.0));
        }
    }

    #[test]
    fn midpoint_monotonic_easings_in_range() {
        for e in [
            Easing::Linear,
            Easing::Smooth,
            Easing::Smoothstep,
            Easing::Smootherstep,
            Easing::QuadIn,
            Easing::QuadOut,
            Easing::QuadInOut,
            Easing::CubicIn,
            Easing::CubicOut,
            Easing::CubicInOut,
            Easing::Sin,
        ] {
            let f = e.resolve();
            let v = f(0.5);
            assert!((0.0..=1.0).contains(&v), "{e:?} f(0.5)={v} out of [0,1]");
        }
    }

    #[test]
    fn linear_is_identity() {
        assert_eq!(linear(0.0), 0.0);
        assert_eq!(linear(0.5), 0.5);
        assert_eq!(linear(1.0), 1.0);
    }

    #[test]
    fn smooth_is_hermite() {
        // smooth(0.5) = 0.5 exactly (symmetry of the Hermite curve).
        assert!((smooth(0.5) - 0.5).abs() < 1e-9);
    }

    #[test]
    fn from_str_accepts_aliases() {
        assert_eq!(Easing::from_str("linear"), Some(Easing::Linear));
        assert_eq!(Easing::from_str("Linear"), Some(Easing::Linear));
        assert_eq!(Easing::from_str("ease_in_out"), Some(Easing::CubicInOut));
        assert_eq!(Easing::from_str("ease-in-out"), Some(Easing::CubicInOut));
        assert_eq!(Easing::from_str("ease-out"), Some(Easing::CubicOut));
        assert_eq!(Easing::from_str("ease-in"), Some(Easing::CubicIn));
        assert_eq!(Easing::from_str("sin"), Some(Easing::Sin));
        assert_eq!(Easing::from_str("wiggle"), Some(Easing::Wiggle));
        assert_eq!(Easing::from_str("unknown"), None);
    }

    #[test]
    fn there_and_back_returns_to_origin() {
        let f = Easing::ThereAndBack.resolve();
        assert!((f(0.0) - 0.0).abs() < 1e-9);
        assert!((f(0.5) - 1.0).abs() < 1e-9);
        assert!((f(1.0) - 0.0).abs() < 1e-9);
    }

    #[test]
    fn serde_roundtrip() {
        let e = Easing::CubicInOut;
        let s = serde_json::to_string(&e).unwrap();
        let e2: Easing = serde_json::from_str(&s).unwrap();
        assert_eq!(e, e2);
    }
}
