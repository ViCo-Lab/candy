//! 2-D affine matrix (`x' = a*x + c*y + e`, `y' = b*x + d*y + f`) used by the
//! camera warp path.

/// A 2-D affine matrix `x' = a*x + c*y + e`, `y' = b*x + d*y + f`.
#[derive(Clone, Copy)]
pub(crate) struct Matrix {
    a: f64,
    b: f64,
    c: f64,
    d: f64,
    e: f64,
    f: f64,
}

impl Matrix {
    /// Translation matrix (cm/pt units, same space as the rest of the pipeline).
    pub(crate) fn translation(x: f64, y: f64) -> Self {
        Matrix {
            a: 1.0,
            b: 0.0,
            c: 0.0,
            d: 1.0,
            e: x,
            f: y,
        }
    }

    /// Uniform scale matrix.
    pub(crate) fn scaling(s: f64) -> Self {
        Matrix {
            a: s,
            b: 0.0,
            c: 0.0,
            d: s,
            e: 0.0,
            f: 0.0,
        }
    }

    /// Rotation matrix, `deg` degrees clockwise (Typst convention; +y down).
    pub(crate) fn rotation(deg: f64) -> Self {
        let r = deg.to_radians();
        let (s, c) = (r.sin(), r.cos());
        Matrix {
            a: c,
            b: s,
            c: -s,
            d: c,
            e: 0.0,
            f: 0.0,
        }
    }

    /// Apply the affine to a point `(x, y)`, returning the mapped point.
    pub(crate) fn apply(&self, x: f64, y: f64) -> (f64, f64) {
        (
            self.a * x + self.c * y + self.e,
            self.b * x + self.d * y + self.f,
        )
    }

    /// Inverse affine. Panics only on a degenerate (zero-determinant) matrix,
    /// which the camera never produces (zoom is clamped to `> 0`).
    pub(crate) fn inverse(&self) -> Matrix {
        let det = self.a * self.d - self.b * self.c;
        let inv = 1.0 / det;
        Matrix {
            a: self.d * inv,
            b: -self.b * inv,
            c: -self.c * inv,
            d: self.a * inv,
            e: (self.c * self.f - self.d * self.e) * inv,
            f: (self.b * self.e - self.a * self.f) * inv,
        }
    }
}

/// Compose `a` after `b` (apply `b` first, then `a`).
pub(crate) fn compose(a: Matrix, b: Matrix) -> Matrix {
    Matrix {
        a: a.a * b.a + a.c * b.b,
        b: a.b * b.a + a.d * b.b,
        c: a.a * b.c + a.c * b.d,
        d: a.b * b.c + a.d * b.d,
        e: a.a * b.e + a.c * b.f + a.e,
        f: a.b * b.e + a.d * b.f + a.f,
    }
}
