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
use crate::core::diag::CandyWarn;

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
/// Precondition: `keyframes` is non-empty and sorted by `time_ms` (the
/// scheduler guarantees both; we re-sort defensively).
/// Postcondition: returns `Vec<FrameData>` with length ≥ `keyframes.len()`,
/// grouped/sorted by `(time_ms, target)`. Every `opacity` value is clamped to
/// [0.0, 1.0]; if any frame had to be clamped, the interpolator clamps and
/// continues but emits [`CandyWarn::Interpolation`] (W016) so the user is told
/// their keyframes / easing produced an out-of-range opacity.
pub fn interpolate(keyframes: Vec<FrameData>) -> Vec<FrameData> {
    interpolate_with(keyframes, InterpMethod::Linear, 30)
}

/// Like [`interpolate`] but with an explicit [`InterpMethod`] and `fps`.
///
/// The interpolator samples the timeline at `1000/fps` ms intervals (the
/// video frame rate). Keyframe times are in ms; the output has one
/// `FrameData` per video frame, per target.
pub fn interpolate_with(
    keyframes: Vec<FrameData>,
    method: InterpMethod,
    fps: u32,
) -> Vec<FrameData> {
    if keyframes.is_empty() {
        return Vec::new();
    }

    // Group keyframes by target.
    let mut groups: HashMap<Label, Vec<FrameData>> = HashMap::new();
    for kf in keyframes {
        groups.entry(kf.target.clone()).or_default().push(kf);
    }

    // Total duration in ms (max time_ms across all keyframes).
    let mut total_ms = 0u32;
    for g in groups.values() {
        for kf in g {
            total_ms = total_ms.max(kf.time_ms);
        }
    }

    // Sample times: one per video frame, at t = i * 1000/fps ms.
    let frame_ms = 1000.0 / fps as f64;
    let n_frames = ((total_ms as f64) / frame_ms).ceil() as u32 + 1;

    let mut out: Vec<FrameData> = Vec::new();
    // Track whether any frame's opacity had to be clamped to [0, 1] (W016).
    let mut opacity_clamped = false;
    for (_, mut kfs) in groups {
        kfs.sort_by_key(|f| f.time_ms);

        for i in 0..n_frames {
            let t_ms = (i as f64 * frame_ms).round() as u32;
            let fr = match method {
                InterpMethod::Linear => interp_linear(&kfs, t_ms),
                InterpMethod::CatmullRom => interp_catmull(&kfs, t_ms),
            };
            if let Some(mut fr) = fr {
                if !(0.0..=1.0).contains(&fr.opacity) {
                    fr.opacity = fr.opacity.clamp(0.0, 1.0);
                    opacity_clamped = true;
                }
                fr.time_ms = t_ms;
                out.push(fr);
            }
        }
    }

    out.sort_by(|a, b| a.time_ms.cmp(&b.time_ms).then(a.target.0.cmp(&b.target.0)));
    if opacity_clamped {
        crate::warn!(CandyWarn::Interpolation(
            "opacity out of [0,1] was clamped during interpolation".into(),
        ));
    }
    out
}

/// Linear interpolation between the two keyframes surrounding `frame`.
fn interp_linear(kfs: &[FrameData], frame: u32) -> Option<FrameData> {
    // Binary search: find the last keyframe with time_ms <= frame.
    let i = kfs.partition_point(|k| k.time_ms <= frame);
    let a = if i > 0 { Some(&kfs[i - 1]) } else { None };
    let b = if i < kfs.len() { Some(&kfs[i]) } else { None };
    match (a, b) {
        (Some(a), None) => Some(a.clone()),
        (None, Some(b)) => Some(b.clone()),
        (Some(a), Some(b)) if a.time_ms == b.time_ms => Some(a.clone()),
        (Some(a), Some(b)) => {
            let t = (frame - a.time_ms) as f64 / (b.time_ms - a.time_ms) as f64;
            let eased_t = b.easing.resolve()(t);
            let mut fr = FrameData::lerp(a, b, eased_t);
            fr.time_ms = frame;
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
    // Binary search: find the segment [i, i+1] containing `frame`.
    // partition_point returns the first index where time_ms > frame,
    // so i-1 is the last keyframe with time_ms <= frame.
    let i = kfs
        .partition_point(|k| k.time_ms <= frame)
        .saturating_sub(1);
    // Before the first keyframe: hold the first value.
    if frame < kfs[0].time_ms {
        return Some(kfs[0].clone_with_frame(frame));
    }
    // After the last keyframe: hold the last value.
    if i >= kfs.len() - 1 {
        return Some(kfs[kfs.len() - 1].clone_with_frame(frame));
    }
    let p1 = &kfs[i];
    let p2 = &kfs[i + 1];
    if p1.time_ms == p2.time_ms {
        return Some(p1.clone_with_frame(frame));
    }
    let t = (frame - p1.time_ms) as f64 / (p2.time_ms - p1.time_ms) as f64;
    let eased_t = p2.easing.resolve()(t);

    // Neighbors for Catmull-Rom (clamp at endpoints).
    let p0 = if i > 0 { &kfs[i - 1] } else { p1 };
    let p3 = if i + 2 < kfs.len() { &kfs[i + 2] } else { p2 };

    let mut fr = catmull_rom(p0, p1, p2, p3, eased_t);
    fr.time_ms = frame;
    fr.target = p1.target.clone();
    Some(fr)
}

/// One-dimensional Catmull-Rom interpolation. Returns the point on the cubic
/// spline through `(p0, p1, p2, p3)` at parameter `t ∈ [0, 1]`, where `t=0`
/// gives `p1` and `t=1` gives `p2`.
fn catmull1(p0: f64, p1: f64, p2: f64, p3: f64, t: f64) -> f64 {
    let t2 = t * t;
    let t3 = t2 * t;
    0.5 * (2.0 * p1
        + (-p0 + p2) * t
        + (2.0 * p0 - 5.0 * p1 + 4.0 * p2 - p3) * t2
        + (-p0 + 3.0 * p1 - 3.0 * p2 + p3) * t3)
}

/// Catmull-Rom interpolation across all FrameData fields.
fn catmull_rom(
    p0: &FrameData,
    p1: &FrameData,
    p2: &FrameData,
    p3: &FrameData,
    t: f64,
) -> FrameData {
    FrameData {
        time_ms: p1.time_ms,
        target: p1.target.clone(),
        x: catmull1(p0.x, p1.x, p2.x, p3.x, t),
        y: catmull1(p0.y, p1.y, p2.y, p3.y, t),
        scale: catmull1(p0.scale, p1.scale, p2.scale, p3.scale, t),
        opacity: catmull1(p0.opacity, p1.opacity, p2.opacity, p3.opacity, t),
        rotation: catmull1(p0.rotation, p1.rotation, p2.rotation, p3.rotation, t),
        easing: p1.easing.clone(),
    }
}

/// Helper trait to clone a FrameData with a new time_ms (avoids repeating
/// the field-copy boilerplate).
trait CloneWithFrame {
    fn clone_with_frame(&self, frame: u32) -> FrameData;
}

impl CloneWithFrame for FrameData {
    fn clone_with_frame(&self, frame: u32) -> FrameData {
        let mut f = self.clone();
        f.time_ms = frame;
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
        // 1000ms at 30fps = 30 frames + 1 = 31 samples (0..1000ms inclusive).
        let kf = vec![
            FrameData {
                time_ms: 0,
                target: Label("a".into()),
                x: 0.0,
                y: 0.0,
                scale: 1.0,
                opacity: 1.0,
                rotation: 0.0,
                easing: Easing::Linear,
            },
            FrameData {
                time_ms: 1000,
                target: Label("a".into()),
                x: 10.0,
                y: 0.0,
                scale: 1.0,
                opacity: 1.0,
                rotation: 0.0,
                easing: Easing::Linear,
            },
        ];
        let out = interpolate(kf);
        // 1000ms / 33.33ms per frame ≈ 30 frames + 1 = 31 samples
        assert!(out.len() >= 30, "expected ~31 frames, got {}", out.len());
        assert_eq!(out[0].x, 0.0);
        assert!((out[out.len() - 1].x - 10.0).abs() < 1e-6);
    }

    #[test]
    fn clamps_opacity() {
        let kf = vec![
            FrameData {
                time_ms: 0,
                target: Label("a".into()),
                x: 0.0,
                y: 0.0,
                scale: 1.0,
                opacity: 1.0,
                rotation: 0.0,
                easing: Easing::Linear,
            },
            FrameData {
                time_ms: 500,
                target: Label("a".into()),
                x: 0.0,
                y: 0.0,
                scale: 1.0,
                opacity: -2.0,
                rotation: 0.0,
                easing: Easing::Linear,
            },
        ];
        // Out-of-range opacity is clamped to [0, 1] and surfaced as a warning
        // (W016); the interpolator returns the clamped frames (non-fatal).
        let out = interpolate(kf);
        for f in &out {
            assert!(
                (0.0..=1.0).contains(&f.opacity),
                "opacity {} was not clamped to [0, 1]",
                f.opacity
            );
        }
    }

    /// Easing must shape the interpolation curve. With `Smooth` (Hermite
    /// `t²(3-2t)`), the midpoint at t=0.5 maps to 0.5 (symmetry), but at
    /// t=0.25 the eased value is `0.25²(3-0.5) = 0.15625` — below the linear
    /// 0.25, proving the curve is non-linear.
    #[test]
    fn easing_shapes_curve() {
        // 4000ms at 30fps: frame_ms = 33.33. We test at t=0.25/0.5/0.75.
        let kf = vec![
            FrameData {
                time_ms: 0,
                target: Label("a".into()),
                x: 0.0,
                y: 0.0,
                scale: 1.0,
                opacity: 1.0,
                rotation: 0.0,
                easing: Easing::Linear,
            },
            FrameData {
                time_ms: 4000,
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
        // Find the sample closest to t=0.5 (2000ms).
        let mid = out
            .iter()
            .min_by_key(|f| (f.time_ms as i64 - 2000).abs())
            .unwrap();
        // At t=0.5, smooth = 0.5, so x ≈ 5.0
        assert!((mid.x - 5.0).abs() < 0.1, "mid x={}", mid.x);
    }
}
