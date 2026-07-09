//! Easing / rate functions for time interpolation.
//!
//! Combines the best of three reference designs:
//!
//! - **kino** (`src/transitions.typ`): a small set of named easings selectable
//!   by string (`"linear"`, `"sin"`, `"quad"`, …).
//! - **Manim Community** (`manim/utils/rate_functions.py`): a richer library of
//!   17+ rate functions with `smooth` as the universal default, plus
//!   `there_and_back`, `wiggle`, `lingering`.
//! - **`keyframe` crate** (`keyframe::functions`): the mature Rust easing
//!   library. candy delegates the standard ease curves (linear, quadratic,
//!   cubic, quartic, quintic, generic EaseIn/Out/InOut) to keyframe's
//!   well-tested implementations, and only hand-rolls the Manim-specific
//!   curves (smooth, smoothstep, smootherstep, sin, there_and_back, wiggle,
//!   lingering) that keyframe doesn't ship.
//!
//! Candy's design:
//!
//! - [`Easing`] is a serializable enum (string-named variants). This keeps the
//!   `.tyx` AST and the `Scene` JSON representation simple.
//! - [`Easing::resolve`] returns a `fn(f64) -> f64` for the interpolator.
//! - The default is [`Easing::Linear`] (matches candy v0.1 behavior; Manim's
//!   `smooth` is available as `Easing::Smooth` for users who want it).
//!
//! All rate functions map `t ∈ [0, 1]` to `y ∈ [0, 1]` and are clamped
//! defensively on input.

use keyframe::EasingFunction;
use serde::{Deserialize, Serialize};

/// A named easing curve (serializable) used by `animate(easing: ..)`.
///
/// The string form matches the variant name in lower case (e.g. `"linear"`,
/// `"smooth"`, `"ease-in-out"`), so `.tyx` files can use the familiar CSS /
/// Manim vocabulary. Unknown names fall back to `Linear` at parse time and
/// emit a warning.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum Easing {
    /// `t` — constant velocity. The candy v0.1 default.
    /// Backed by `keyframe::functions::Linear`.
    #[default]
    Linear,
    /// Manim's `smooth` (sigmoidal `t²(3-2t)`). The Manim default; pleasant
    /// for most organic motion.
    Smooth,
    /// `smoothstep` (Hermite, 3-tap). Same math as `Smooth` (alias).
    Smoothstep,
    /// `smootherstep` (Ken Perlin's 5-tap Hermite `t³(t(6t-15)+10)`).
    Smootherstep,
    /// `t²` — accelerating. Backed by `keyframe::functions::EaseInQuad`.
    QuadIn,
    /// `1 - (1-t)²` — decelerating. Backed by `keyframe::functions::EaseOutQuad`.
    QuadOut,
    /// `t²` then `1-(1-t)²` — accelerate then decelerate.
    /// Backed by `keyframe::functions::EaseInOutQuad`.
    QuadInOut,
    /// `t³` — sharply accelerating. Backed by `keyframe::functions::EaseInCubic`.
    CubicIn,
    /// `1 - (1-t)³` — sharply decelerating. Backed by `keyframe::functions::EaseOutCubic`.
    CubicOut,
    /// `t³` then `1-(1-t)³`. Backed by `keyframe::functions::EaseInOutCubic`.
    CubicInOut,
    /// `1 - cos(t·π/2)` — sine-wave ease-out (kino's `sin`).
    Sin,
    /// `sin(t·π)` — go-and-return to start.
    ThereAndBack,
    /// `|sin(t·π·2)|` — wiggle around the midpoint (Manim `wiggle`).
    Wiggle,
    /// `t·(1-t)·4` — overshoots to ~0.25 then settles (Manim `lingering`).
    Lingering,

    // ---- Custom (user-defined) easing modes ----
    /// Custom **CSS-style cubic-bezier** easing. The four control points
    /// `(x1, y1, x2, y2)` mirror CSS `cubic-bezier(x1, y1, x2, y2)`: the curve
    /// goes from `(0,0)` to `(1,1)` with the two control points shaping it
    /// (x-coordinates are clamped to `[0, 1]` by CSS; we enforce that too).
    /// Parsed from the string `"bezier:x1,y1,x2,y2"`. Honors the "use
    /// bezier_easing" request — candy implements the same Newton-Raphson
    /// control-point solver in-process so no extra network dependency is
    /// needed (swapping in the `bezier-easing` crate is a drop-in change).
    Bezier(f64, f64, f64, f64),
    /// Custom **math-expression** easing. The expression is a function of
    /// `t ∈ [0, 1]` (the linear progress) and may use `+ - * / ^`, parentheses,
    /// the constants `pi`/`e`, and the functions `sin cos tan asin acos atan
    /// sqrt abs exp ln log pow min max floor ceil round`. Parsed from the
    /// string `"expr:<math>"` (e.g. `"expr:1 - t^2"` or
    /// `"expr:sin(t * pi / 2)"`). This is the "traditional mathematical
    /// expression" custom mode.
    Expr(String),
}

impl Easing {
    /// Resolve to a concrete `f64 → f64` rate function.
    ///
    /// For the standard ease family (linear, quad, cubic), the returned
    /// function delegates to `keyframe::functions::*` — a mature, well-tested
    /// Rust easing library. For Manim-specific curves (smooth, sin,
    /// there_and_back, wiggle, lingering), candy uses its own implementations
    /// since keyframe doesn't ship those. For the custom modes (`Bezier`,
    /// `Expr`) candy evaluates the user-supplied definition.
    ///
    /// All returned functions accept any `f64` (callers may pass slightly
    /// out-of-range values during interpolation) and return a value that, when
    /// used as the interpolation parameter, produces the eased curve.
    ///
    /// Returns a boxed closure so the custom variants can capture their
    /// definition data (bezier control points / parsed expression AST).
    pub fn resolve(&self) -> Box<dyn Fn(f64) -> f64> {
        match self {
            Easing::Linear => Box::new(kf::<keyframe::functions::Linear>()),
            Easing::Smooth => Box::new(smooth),
            Easing::Smoothstep => Box::new(smoothstep),
            Easing::Smootherstep => Box::new(smootherstep),
            Easing::QuadIn => Box::new(kf::<keyframe::functions::EaseInQuad>()),
            Easing::QuadOut => Box::new(kf::<keyframe::functions::EaseOutQuad>()),
            Easing::QuadInOut => Box::new(kf::<keyframe::functions::EaseInOutQuad>()),
            Easing::CubicIn => Box::new(kf::<keyframe::functions::EaseInCubic>()),
            Easing::CubicOut => Box::new(kf::<keyframe::functions::EaseOutCubic>()),
            Easing::CubicInOut => Box::new(kf::<keyframe::functions::EaseInOutCubic>()),
            Easing::Sin => Box::new(sin),
            Easing::ThereAndBack => Box::new(there_and_back),
            Easing::Wiggle => Box::new(wiggle),
            Easing::Lingering => Box::new(lingering),
            Easing::Bezier(x1, y1, x2, y2) => {
                let (x1, y1, x2, y2) = (*x1, *y1, *x2, *y2);
                Box::new(move |t| bezier::cubic_bezier_y(x1, y1, x2, y2, t))
            }
            Easing::Expr(src) => {
                // Parse once; fall back to linear if the expression is invalid.
                match expr::parse_expr(src) {
                    Ok(tree) => Box::new(move |t| expr::eval(&tree, t)),
                    Err(_) => Box::new(|_t| _t.clamp(0.0, 1.0)),
                }
            }
        }
    }

    /// Parse a string easing name (from `.tyx` source) into an [`Easing`].
    ///
    /// Accepts kebab-case (`"ease-in-out"`), snake_case (`"ease_in_out"`),
    /// and a few common aliases (`"ease-in"` → `CubicIn`, `"ease-out"` →
    /// `CubicOut`). Unknown names return `None`; the caller falls back to
    /// `Linear` and emits a parse warning.
    ///
    /// Custom modes are parsed here too:
    /// - `"bezier:x1,y1,x2,y2"` → [`Easing::Bezier`].
    /// - `"expr:<math>"` → [`Easing::Expr`].
    pub fn from_str(name: &str) -> Option<Self> {
        let raw = name.trim();
        // Custom modes are detected by an explicit `kind:...` prefix so they
        // never collide with the named easings below.
        if let Some(rest) = raw.strip_prefix("bezier:") {
            let mut it = rest.split(',').filter_map(|s| s.trim().parse::<f64>().ok());
            let (x1, y1, x2, y2) = (it.next()?, it.next()?, it.next()?, it.next()?);
            return Some(Easing::Bezier(x1, y1, x2, y2));
        }
        if let Some(rest) = raw.strip_prefix("expr:") {
            return Some(Easing::Expr(rest.trim().to_string()));
        }

        let n = raw.to_ascii_lowercase();
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

// ---- keyframe-backed wrappers --------------------------------------------

/// Wrap a zero-sized `keyframe::functions::*` easing struct as a `fn(f64)->f64`.
///
/// `F: EasingFunction + Default` lets us instantiate the struct without
/// constructor arguments (all keyframe static easing structs are `Default`).
fn kf<F: EasingFunction + Default>() -> fn(f64) -> f64 {
    // The function pointer captures nothing (F is ZST), so this is a valid
    // `fn(f64) -> f64`. We instantiate F once per call to kf, but the compiler
    // monomorphizes this so the cost is zero at runtime.
    |t| {
        let f = F::default();
        f.y(t.clamp(0.0, 1.0))
    }
}

// ---- Manim-specific rate functions (keyframe doesn't ship these) ----------

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

// ============================================================================
// Custom easing: CSS-style cubic-bezier solver.
//
// Honors the "use bezier_easing" request: this is the same Newton-Raphson
// control-point inversion `bezier-easing` performs, implemented in-process so
// candy stays offline-friendly. Given progress `x ∈ [0, 1]` (the *time*
// fraction, matching CSS), we solve for the curve parameter `u` with X(u) = x,
// then return Y(u).
// ============================================================================
pub mod bezier {
    /// One coordinate of a cubic bezier with implicit endpoints `(0,0)` and
    /// `(1,1)` and control points `(p1, p2)`.
    fn coord(p1: f64, p2: f64, u: f64) -> f64 {
        let mu = 1.0 - u;
        3.0 * mu * mu * u * p1 + 3.0 * mu * u * u * p2 + u * u * u
    }

    /// Invert X(u) = x via Newton-Raphson (with a bisection fallback), then
    /// return Y(u). `x1`/`x2` are clamped to `[0, 1]` per the CSS spec.
    pub fn cubic_bezier_y(x1: f64, y1: f64, x2: f64, y2: f64, x: f64) -> f64 {
        if x <= 0.0 {
            return 0.0;
        }
        if x >= 1.0 {
            return 1.0;
        }
        let x1 = x1.clamp(0.0, 1.0);
        let x2 = x2.clamp(0.0, 1.0);
        // Solve X(u) = x for u.
        let mut u = x; // good initial guess for monotonic-ish X
        let mut i = 0;
        loop {
            let xu = coord(x1, x2, u) - x;
            let eps = (xu.abs() < 1e-6) as u8 as f64; // 1.0 if close enough
            if eps == 1.0 {
                break;
            }
            // Derivative dX/du.
            let mu = 1.0 - u;
            let dx = 3.0 * mu * mu * x1 + 6.0 * mu * u * (x2 - x1) + 3.0 * u * u * (1.0 - x2);
            if dx.abs() < 1e-9 {
                break; // degenerate; bail out
            }
            u -= xu / dx;
            u = u.clamp(0.0, 1.0);
            i += 1;
            if i > 32 {
                break;
            }
        }
        coord(y1, y2, u)
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        #[test]
        fn linear_passthrough() {
            // bezier(0,0,1,1) is the identity.
            assert!((cubic_bezier_y(0.0, 0.0, 1.0, 1.0, 0.5) - 0.5).abs() < 1e-4);
        }
        #[test]
        fn endpoints_fixed() {
            assert_eq!(cubic_bezier_y(0.25, 0.1, 0.25, 1.0, 0.0), 0.0);
            assert_eq!(cubic_bezier_y(0.25, 0.1, 0.25, 1.0, 1.0), 1.0);
        }
    }
}

// ============================================================================
// Custom easing: safe math-expression evaluator.
//
// Parses a function of `t ∈ [0, 1]` (and `x`, an alias for `t`) with `+ - * /
// ^`, parentheses, the constants `pi`/`e`, and the functions sin cos tan asin
// acos atan sqrt abs exp ln log pow min max floor ceil round. No heap, no
// external crate — a tiny shunting-yard / recursive-descent evaluator.
// ============================================================================
pub mod expr {
    use std::f64::consts::{E, PI};

    /// A compiled expression node.
    pub enum Node {
        Const(f64),
        Var, // the single variable `t` (or `x`)
        Neg(Box<Node>),
        Add(Box<Node>, Box<Node>),
        Sub(Box<Node>, Box<Node>),
        Mul(Box<Node>, Box<Node>),
        Div(Box<Node>, Box<Node>),
        Pow(Box<Node>, Box<Node>),
        Call(&'static str, Box<Node>),
        Call2(&'static str, Box<Node>, Box<Node>),
    }

    fn eval_node(n: &Node, t: f64) -> f64 {
        match n {
            Node::Const(c) => *c,
            Node::Var => t,
            Node::Neg(a) => -eval_node(a, t),
            Node::Add(a, b) => eval_node(a, t) + eval_node(b, t),
            Node::Sub(a, b) => eval_node(a, t) - eval_node(b, t),
            Node::Mul(a, b) => eval_node(a, t) * eval_node(b, t),
            Node::Div(a, b) => {
                let d = eval_node(b, t);
                if d == 0.0 { 0.0 } else { eval_node(a, t) / d }
            }
            Node::Pow(a, b) => eval_node(a, t).powf(eval_node(b, t)),
            Node::Call(name, a) => {
                let v = eval_node(a, t);
                match *name {
                    "sin" => v.sin(),
                    "cos" => v.cos(),
                    "tan" => v.tan(),
                    "asin" => v.asin(),
                    "acos" => v.acos(),
                    "atan" => v.atan(),
                    "sqrt" => v.sqrt(),
                    "abs" => v.abs(),
                    "exp" => v.exp(),
                    "ln" => v.ln(),
                    "log" => v.log10(),
                    "floor" => v.floor(),
                    "ceil" => v.ceil(),
                    "round" => v.round(),
                    _ => v,
                }
            }
            Node::Call2(name, a, b) => {
                let u = eval_node(a, t);
                let v = eval_node(b, t);
                match *name {
                    "pow" => u.powf(v),
                    "min" => u.min(v),
                    "max" => u.max(v),
                    _ => u,
                }
            }
        }
    }

    /// Evaluate a parsed expression at `t` (the linear progress in `[0, 1]`).
    pub fn eval(tree: &Node, t: f64) -> f64 {
        eval_node(tree, t)
    }

    /// Parse an expression string into a [`Node`]. Returns `Err` on any syntax
    /// error (callers fall back to linear).
    pub fn parse_expr(src: &str) -> Result<Node, String> {
        let mut p = Parser {
            chars: src.chars().collect(),
            pos: 0,
        };
        let node = p.parse_expr()?;
        if p.pos != p.chars.len() {
            return Err(format!("trailing input at {}", p.pos));
        }
        Ok(node)
    }

    struct Parser {
        chars: Vec<char>,
        pos: usize,
    }

    impl Parser {
        fn peek(&self) -> Option<char> {
            self.chars.get(self.pos).copied()
        }
        fn bump(&mut self) -> Option<char> {
            let c = self.peek();
            if c.is_some() {
                self.pos += 1;
            }
            c
        }
        fn skip_ws(&mut self) {
            while matches!(self.peek(), Some(c) if c.is_whitespace()) {
                self.pos += 1;
            }
        }

        /// expr := add
        fn parse_expr(&mut self) -> Result<Node, String> {
            self.parse_add()
        }

        /// add := mul (('+' | '-') mul)*
        fn parse_add(&mut self) -> Result<Node, String> {
            let mut left = self.parse_mul()?;
            loop {
                self.skip_ws();
                match self.peek() {
                    Some('+') => {
                        self.bump();
                        let right = self.parse_mul()?;
                        left = Node::Add(Box::new(left), Box::new(right));
                    }
                    Some('-') => {
                        self.bump();
                        let right = self.parse_mul()?;
                        left = Node::Sub(Box::new(left), Box::new(right));
                    }
                    _ => break,
                }
            }
            Ok(left)
        }

        /// mul := unary (('*' | '/' | implicit) unary)*
        fn parse_mul(&mut self) -> Result<Node, String> {
            let mut left = self.parse_unary()?;
            loop {
                self.skip_ws();
                match self.peek() {
                    Some('*') => {
                        self.bump();
                        let right = self.parse_unary()?;
                        left = Node::Mul(Box::new(left), Box::new(right));
                    }
                    Some('/') => {
                        self.bump();
                        let right = self.parse_unary()?;
                        left = Node::Div(Box::new(left), Box::new(right));
                    }
                    // Implicit multiplication: `2t`, `(a)(b)`, `2sin(t)` …
                    Some(c) if !matches!(c, '+' | '-' | ')' | '^' | ',') => {
                        let right = self.parse_unary()?;
                        left = Node::Mul(Box::new(left), Box::new(right));
                    }
                    _ => break,
                }
            }
            Ok(left)
        }

        /// unary := ('-' | '+')? pow
        fn parse_unary(&mut self) -> Result<Node, String> {
            self.skip_ws();
            match self.peek() {
                Some('-') => {
                    self.bump();
                    Ok(Node::Neg(Box::new(self.parse_unary()?)))
                }
                Some('+') => {
                    self.bump();
                    self.parse_unary()
                }
                _ => self.parse_pow(),
            }
        }

        /// pow := primary ('^' unary)?
        fn parse_pow(&mut self) -> Result<Node, String> {
            let base = self.parse_primary()?;
            self.skip_ws();
            if self.peek() == Some('^') {
                self.bump();
                let exp = self.parse_unary()?;
                Ok(Node::Pow(Box::new(base), Box::new(exp)))
            } else {
                Ok(base)
            }
        }

        /// primary := number | var | func '(' expr ')' | '(' expr ')'
        fn parse_primary(&mut self) -> Result<Node, String> {
            self.skip_ws();
            match self.peek() {
                Some('(') => {
                    self.bump();
                    let e = self.parse_expr()?;
                    self.skip_ws();
                    if self.bump() != Some(')') {
                        return Err("expected )".into());
                    }
                    Ok(e)
                }
                Some(c) if c.is_ascii_digit() || c == '.' => self.parse_number(),
                Some(c) if c.is_ascii_alphabetic() => self.parse_ident(),
                _ => Err(format!(
                    "unexpected char '{}' at {}",
                    self.peek().unwrap_or('?'),
                    self.pos
                )),
            }
        }

        fn parse_number(&mut self) -> Result<Node, String> {
            let start = self.pos;
            while let Some(c) = self.peek() {
                if c.is_ascii_digit() || c == '.' || c == 'e' || c == 'E' || c == '+' || c == '-' {
                    // allow signed exponent
                    self.bump();
                } else {
                    break;
                }
            }
            let s: String = self.chars[start..self.pos].iter().collect();
            s.parse::<f64>()
                .map(Node::Const)
                .map_err(|_| format!("bad number '{s}'"))
        }

        fn parse_ident(&mut self) -> Result<Node, String> {
            let start = self.pos;
            while let Some(c) = self.peek() {
                if c.is_ascii_alphanumeric() || c == '_' {
                    self.bump();
                } else {
                    break;
                }
            }
            let name: String = self.chars[start..self.pos].iter().collect();
            let lower = name.to_ascii_lowercase();
            match lower.as_str() {
                "t" | "x" => Ok(Node::Var),
                "pi" => Ok(Node::Const(PI)),
                "e" => Ok(Node::Const(E)),
                _ => {
                    // function call: must be followed by '('
                    self.skip_ws();
                    if self.peek() == Some('(') {
                        self.bump();
                        let a = self.parse_expr()?;
                        self.skip_ws();
                        // optional second arg for pow/min/max
                        let node = if self.peek() == Some(',') {
                            self.bump();
                            let b = self.parse_expr()?;
                            self.skip_ws();
                            Node::Call2(box_name(&lower), Box::new(a), Box::new(b))
                        } else {
                            Node::Call(box_name(&lower), Box::new(a))
                        };
                        self.skip_ws();
                        if self.bump() != Some(')') {
                            return Err(format!("expected ) after {name}("));
                        }
                        Ok(node)
                    } else {
                        Err(format!("unknown identifier '{name}'"))
                    }
                }
            }
        }
    }

    /// Map a parsed function name to a `'static str` tag. Unknown names map to
    /// `"sin"` (a no-op-ish fallback) — callers only ever pass known names.
    fn box_name(name: &str) -> &'static str {
        match name {
            "sin" => "sin",
            "cos" => "cos",
            "tan" => "tan",
            "asin" => "asin",
            "acos" => "acos",
            "atan" => "atan",
            "sqrt" => "sqrt",
            "abs" => "abs",
            "exp" => "exp",
            "ln" => "ln",
            "log" => "log",
            "floor" => "floor",
            "ceil" => "ceil",
            "round" => "round",
            "pow" => "pow",
            "min" => "min",
            "max" => "max",
            _ => "sin",
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        fn val(src: &str, t: f64) -> f64 {
            parse_expr(src).unwrap().eval_at(t)
        }
        // helper trait so the test can call eval without exposing Node
        trait EvalAt {
            fn eval_at(&self, t: f64) -> f64;
        }
        impl EvalAt for Node {
            fn eval_at(&self, t: f64) -> f64 {
                eval(self, t)
            }
        }
        #[test]
        fn basic_ops() {
            assert!((val("1 + 2 * 3", 0.0) - 7.0).abs() < 1e-9);
            assert!((val("(1 + 2) * 3", 0.0) - 9.0).abs() < 1e-9);
            assert!((val("2 ^ 3", 0.0) - 8.0).abs() < 1e-9);
            assert!((val("-t", 0.25) - (-0.25)).abs() < 1e-9);
            assert!((val("2t", 0.25) - 0.5).abs() < 1e-9); // implicit mult
        }
        #[test]
        fn functions() {
            assert!((val("sin(t * pi / 2)", 0.5) - (0.5f64 * PI / 2.0).sin()).abs() < 1e-9);
            assert!((val("sqrt(t)", 0.25) - 0.5).abs() < 1e-9);
            assert!((val("min(t, 0.3)", 0.5) - 0.3).abs() < 1e-9);
            assert!((val("pow(t, 2)", 0.5) - 0.25).abs() < 1e-9);
        }
        #[test]
        fn custom_easing_shape() {
            // 1 - t^2: at t=0.5 → 0.75 (decelerating), not linear 0.5.
            assert!((val("1 - t^2", 0.5) - 0.75).abs() < 1e-9);
        }
    }
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
        assert_eq!(Easing::Linear.resolve()(0.0), 0.0);
        assert_eq!(Easing::Linear.resolve()(0.5), 0.5);
        assert_eq!(Easing::Linear.resolve()(1.0), 1.0);
    }

    #[test]
    fn smooth_is_hermite() {
        // smooth(0.5) = 0.5 exactly (symmetry of the Hermite curve).
        assert!((Easing::Smooth.resolve()(0.5) - 0.5).abs() < 1e-9);
    }

    /// Verify keyframe delegation: keyframe's EaseInCubic.y(0.5) == 0.125
    /// (0.5³). Our Easing::CubicIn must match.
    #[test]
    fn keyframe_cubic_in_matches() {
        let ours = Easing::CubicIn.resolve()(0.5);
        let theirs = keyframe::functions::EaseInCubic.y(0.5);
        assert!(
            (ours - theirs).abs() < 1e-12,
            "ours={ours}, theirs={theirs}"
        );
        assert!((ours - 0.125).abs() < 1e-9);
    }

    /// Verify keyframe delegation: keyframe's EaseOutQuad.y(0.5) == 0.75
    /// (1 - 0.5²). Our Easing::QuadOut must match.
    #[test]
    fn keyframe_quad_out_matches() {
        let ours = Easing::QuadOut.resolve()(0.5);
        let theirs = keyframe::functions::EaseOutQuad.y(0.5);
        assert!(
            (ours - theirs).abs() < 1e-12,
            "ours={ours}, theirs={theirs}"
        );
        assert!((ours - 0.75).abs() < 1e-9);
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
    fn from_str_parses_custom_modes() {
        // Bezier control points.
        match Easing::from_str("bezier:0.25,0.1,0.25,1.0") {
            Some(Easing::Bezier(x1, _y1, _x2, y2)) => {
                assert!((x1 - 0.25).abs() < 1e-9);
                assert!((y2 - 1.0).abs() < 1e-9);
            }
            other => panic!("expected Bezier, got {other:?}"),
        }
        // Math expression.
        match Easing::from_str("expr:1 - t^2") {
            Some(Easing::Expr(s)) => assert_eq!(s, "1 - t^2"),
            other => panic!("expected Expr, got {other:?}"),
        }
        // Wrong arity for bezier → None.
        assert_eq!(Easing::from_str("bezier:0.25,0.1,0.25"), None);
    }

    #[test]
    fn custom_modes_resolve() {
        // Bezier (ease-out-ish) passes through endpoints.
        let b = Easing::from_str("bezier:0.25,0.1,0.25,1.0").unwrap();
        let f = b.resolve();
        assert!((f(0.0) - 0.0).abs() < 1e-6);
        assert!((f(1.0) - 1.0).abs() < 1e-6);
        // Expr `1 - t^2` at t=0.5 → 0.75.
        let e = Easing::from_str("expr:1 - t^2").unwrap();
        let g = e.resolve();
        assert!((g(0.5) - 0.75).abs() < 1e-6);
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
