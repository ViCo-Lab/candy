//! X-axis (time) interpolation: expand keyframes into every frame.
//!
//! Two interpolation methods are supported:
//!
//! - **Linear** (default): straight-line lerp between each pair of keyframes,
//!   shaped by the target keyframe's easing function. Fast and predictable.
//! - **Catmull-Rom spline**: smooth cubic interpolation through the keyframe
//!   points, producing C1-continuous motion (no velocity jumps at keyframes).
//!   Ideal for multi-keyframe paths where linear interpolation looks "robotic".
//!   The easing function still applies, shaping the parametric `t` before the
//!   spline is evaluated.

use std::collections::HashMap;

use crate::core::ast::{FrameData, Label, lerp};

/// Interpolation method for expanding keyframes into per-frame data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InterpMethod {
    /// Linear lerp between keyframe pairs, shaped by the easing function.
    /// Default; backward-compatible with candy v0.1.
    #[default]
    Linear,
    /// Catmull-Rom spline through the keyframe points (C1-continuous). The
    /// easing function shapes the parametric `t` before spline evaluation.
    CatmullRom,
}

/// Interpolate between keyframes to generate all frames (linear method).
///
/// Precondition: `keyframes` is non-empty and sorted by `frame_idx` (the
/// scheduler guarantees both; we re-sort defensively).
/// Postcondition: returns `Vec<FrameData>` with length ≥ `keyframes.len()`,
/// grouped/sorted by `(frame_idx, target)`. Every `opacity` value is clamped to
/// [0.0, 1.0] (spec E005 handling).
pub fn interpolate(keyframes: Vec<FrameData>) -> Vec<FrameData> {
    interpolate_with(keyframes, InterpMethod::Linear)
}

/// Like [`interpolate`] but with an explicit [`InterpMethod`].
pub fn interpolate_with(keyframes: Vec<FrameData>, method: InterpMethod) -> Vec<FrameData> {
    if keyframes.is_empty() {
        return Vec::new();
    }

    // Group keyframes by target.
    let mut groups: HashMap<Label, Vec<FrameData>> = HashMap::new();
    for kf in keyframes {
        groups.entry(kf.target.clone()).or_default().push(kf);
    }

    let mut last_frame = 0u32;
    for g in groups.values() {
        for kf in g {
            last_frame = last_frame.max(kf.frame_idx);
        }
    }

    let mut out: Vec<FrameData> = Vec::new();
    for (_, mut kfs) in groups {
        kfs.sort_by_key(|f| f.frame_idx);

        for frame in 0..=last_frame {
            let fr = match method {
                InterpMethod::Linear => interp_linear(&kfs, frame),
                InterpMethod::CatmullRom => interp_catmull(&kfs, frame),
            };
            if let Some(mut fr) = fr {
                if !(0.0..=1.0).contains(&fr.opacity) {
                    fr.opacity = fr.opacity.clamp(0.0, 1.0);
                }
                out.push(fr);
            }
        }
    }

    out.sort_by(|a, b| a.frame_idx.cmp(&b.frame_idx).then(a.target.0.cmp(&b.target.0)));
    out
}

/// Linear interpolation between the two keyframes surrounding `frame`.
fn interp_linear(kfs: &[FrameData], frame: u32) -> Option<FrameData> {
    let mut a: Option<&FrameData> = None;
    let mut b: Option<&FrameData> = None;
    for k in kfs {
        if k.frame_idx <= frame {
            a = Some(k);
        } else {
            b = Some(k);
            break;
        }
    }
    match (a, b) {
        (Some(a), None) => Some(a.clone()),
        (None, Some(b)) => Some(b.clone()),
        (Some(a), Some(b)) if a.frame_idx == b.frame_idx => Some(a.clone()),
        (Some(a), Some(b)) => {
            let t = (frame - a.frame_idx) as f64 / (b.frame_idx - a.frame_idx) as f64;
            let eased_t = b.easing.resolve()(t);
            let mut fr = FrameData::lerp(a, b, eased_t);
            fr.frame_idx = frame;
            fr.target = a.target.clone();
            Some(fr)
        }
        (None, None) => None,
    }
}

/// Catmull-Rom spline interpolation through the keyframe points.
///
/// For each segment between keyframes `p1` and `p2`, uses the neighbors `p0`
/// and `p3` (clamped to the endpoints if out of range) to compute a cubic
/// spline that passes through all four points with C1 continuity. The easing
/// function shapes the parametric `t` before spline evaluation.
fn interp_catmull(kfs: &[FrameData], frame: u32) -> Option<FrameData> {
    if kfs.is_empty() {
        return None;
    }
    // Find the segment [i, i+1] containing `frame`.
    let mut i = 0;
    for (idx, k) in kfs.iter().enumerate() {
        if k.frame_idx <= frame {
            i = idx;
        } else {
            break;
        }
    }
    // Before the first keyframe: hold the first value.
    if frame < kfs[0].frame_idx {
        return Some(kfs[0].clone_with_frame(frame));
    }
    // After the last keyframe: hold the last value.
    if i >= kfs.len() - 1 {
        return Some(kfs[kfs.len() - 1].clone_with_frame(frame));
    }
    let p1 = &kfs[i];
    let p2 = &kfs[i + 1];
    if p1.frame_idx == p2.frame_idx {
        return Some(p1.clone_with_frame(frame));
    }
    let t = (frame - p1.frame_idx) as f64 / (p2.frame_idx - p1.frame_idx) as f64;
    let eased_t = p2.easing.resolve()(t);

    // Neighbors for Catmull-Rom (clamp at endpoints).
    let p0 = if i > 0 { &kfs[i - 1] } else { p1 };
    let p3 = if i + 2 < kfs.len() { &kfs[i + 2] } else { p2 };

    let mut fr = catmull_rom(p0, p1, p2, p3, eased_t);
    fr.frame_idx = frame;
    fr.target = p1.target.clone();
    Some(fr)
}

/// One-dimensional Catmull-Rom interpolation. Returns the point on the cubic
/// spline through `(p0, p1, p2, p3)` at parameter `t ∈ [0, 1]`, where `t=0`
/// gives `p1` and `t=1` gives `p2`.
fn catmull1(p0: f64, p1: f64, p2: f64, p3: f64, t: f64) -> f64 {
    let t2 = t * t;
    let t3 = t2 * t;
    0.5 * (
        2.0 * p1
            + (-p0 + p2) * t
            + (2.0 * p0 - 5.0 * p1 + 4.0 * p2 - p3) * t2
            + (-p0 + 3.0 * p1 - 3.0 * p2 + p3) * t3
    )
}

/// Catmull-Rom interpolation across all FrameData fields.
fn catmull_rom(p0: &FrameData, p1: &FrameData, p2: &FrameData, p3: &FrameData, t: f64) -> FrameData {
    FrameData {
        frame_idx: p1.frame_idx,
        target: p1.target.clone(),
        x: catmull1(p0.x, p1.x, p2.x, p3.x, t),
        y: catmull1(p0.y, p1.y, p2.y, p3.y, t),
        scale: catmull1(p0.scale, p1.scale, p2.scale, p3.scale, t),
        opacity: catmull1(p0.opacity, p1.opacity, p2.opacity, p3.opacity, t),
        rotation: catmull1(p0.rotation, p1.rotation, p2.rotation, p3.rotation, t),
        easing: p1.easing,
    }
}

/// Helper trait to clone a FrameData with a new frame_idx (avoids repeating
/// the field-copy boilerplate).
trait CloneWithFrame {
    fn clone_with_frame(&self, frame: u32) -> FrameData;
}

impl CloneWithFrame for FrameData {
    fn clone_with_frame(&self, frame: u32) -> FrameData {
        let mut f = self.clone();
        f.frame_idx = frame;
        f
    }
}

/// Convenience: interpolate a single scalar track (used by tests/utils).
pub fn interp_scalar(track: &[(u32, f64)], frame: u32) -> f64 {
    if track.is_empty() {
        return 0.0;
    }
    let mut a = track[0];
    let mut b = track[track.len() - 1];
    for &(idx, v) in track {
        if idx <= frame {
            a = (idx, v);
        } else {
            b = (idx, v);
            break;
        }
    }
    if a.0 == b.0 {
        return a.1;
    }
    lerp(a.1, b.1, (frame - a.0) as f64 / (b.0 - a.0) as f64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::ast::Label;
    use crate::core::easing::Easing;

    #[test]
    fn fills_all_frames() {
        let kf = vec![
            FrameData {
                frame_idx: 0,
                target: Label("a".into()),
                x: 0.0,
                y: 0.0,
                scale: 1.0,
                opacity: 1.0,
                rotation: 0.0,
                easing: Easing::Linear,
            },
            FrameData {
                frame_idx: 9,
                target: Label("a".into()),
                x: 9.0,
                y: 0.0,
                scale: 1.0,
                opacity: 1.0,
                rotation: 0.0,
                easing: Easing::Linear,
            },
        ];
        let out = interpolate(kf);
        assert_eq!(out.len(), 10);
        assert_eq!(out[0].x, 0.0);
        assert_eq!(out[9].x, 9.0);
        assert!((out[5].x - 5.0).abs() < 1e-9);
    }

    #[test]
    fn clamps_opacity() {
        let kf = vec![
            FrameData {
                frame_idx: 0,
                target: Label("a".into()),
                x: 0.0,
                y: 0.0,
                scale: 1.0,
                opacity: 1.0,
                rotation: 0.0,
                easing: Easing::Linear,
            },
            FrameData {
                frame_idx: 4,
                target: Label("a".into()),
                x: 0.0,
                y: 0.0,
                scale: 1.0,
                opacity: -2.0,
                rotation: 0.0,
                easing: Easing::Linear,
            },
        ];
        let out = interpolate(kf);
        for f in &out {
            assert!((0.0..=1.0).contains(&f.opacity));
        }
    }

    /// Easing must shape the interpolation curve. With `Smooth` (Hermite
    /// `t²(3-2t)`), the midpoint at t=0.5 maps to 0.5 (symmetry), but at
    /// t=0.25 the eased value is `0.25²(3-0.5) = 0.15625` — below the linear
    /// 0.25, proving the curve is non-linear.
    #[test]
    fn easing_shapes_curve() {
        let kf = vec![
            FrameData {
                frame_idx: 0,
                target: Label("a".into()),
                x: 0.0,
                y: 0.0,
                scale: 1.0,
                opacity: 1.0,
                rotation: 0.0,
                easing: Easing::Linear,
            },
            FrameData {
                frame_idx: 4,
                target: Label("a".into()),
                x: 10.0,
                y: 0.0,
                scale: 1.0,
                opacity: 1.0,
                rotation: 0.0,
                easing: Easing::Smooth,
            },
        ];
        let out = interpolate(kf);
        // frame 1 → t=0.25, eased = 0.25²(3-0.5) = 0.15625, x = 1.5625
        assert!((out[1].x - 1.5625).abs() < 1e-9, "frame 1 x={}", out[1].x);
        // frame 2 → t=0.5, eased = 0.5, x = 5.0 (smooth is symmetric)
        assert!((out[2].x - 5.0).abs() < 1e-9, "frame 2 x={}", out[2].x);
        // frame 3 → t=0.75, eased = 0.75²(3-1.5) = 0.84375, x = 8.4375
        assert!((out[3].x - 8.4375).abs() < 1e-9, "frame 3 x={}", out[3].x);
    }
}
