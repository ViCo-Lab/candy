//! X-axis (time) interpolation: expand keyframes into every frame.

use std::collections::HashMap;

use crate::core::ast::{FrameData, Label, lerp};

/// Interpolate between keyframes to generate all frames.
///
/// Precondition: `keyframes` is non-empty and sorted by `frame_idx` (the
/// scheduler guarantees both; we re-sort defensively).
/// Postcondition: returns `Vec<FrameData>` with length ≥ `keyframes.len()`,
/// grouped/sorted by `(frame_idx, target)`. Every `opacity` value is clamped to
/// [0.0, 1.0] (spec E005 handling).
pub fn interpolate(keyframes: Vec<FrameData>) -> Vec<FrameData> {
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
            // Find the surrounding keyframes a ≤ frame ≤ b.
            let mut a: Option<&FrameData> = None;
            let mut b: Option<&FrameData> = None;
            for k in &kfs {
                if k.frame_idx <= frame {
                    a = Some(k);
                } else {
                    b = Some(k);
                    break;
                }
            }

            let mut fr = match (a, b) {
                (Some(a), None) => a.clone(),
                (None, Some(b)) => b.clone(),
                (Some(a), Some(b)) if a.frame_idx == b.frame_idx => a.clone(),
                (Some(a), Some(b)) => {
                    let t = if b.frame_idx == a.frame_idx {
                        0.0
                    } else {
                        (frame - a.frame_idx) as f64 / (b.frame_idx - a.frame_idx) as f64
                    };
                    let mut fr = FrameData::lerp(a, b, t);
                    fr.frame_idx = frame;
                    fr.target = a.target.clone();
                    fr
                }
                (None, None) => continue,
            };

            // Mandatory assertion (E005): opacity ∈ [0, 1], clamp otherwise.
            if !(0.0..=1.0).contains(&fr.opacity) {
                fr.opacity = fr.opacity.clamp(0.0, 1.0);
            }
            out.push(fr);
        }
    }

    out.sort_by(|a, b| a.frame_idx.cmp(&b.frame_idx).then(a.target.0.cmp(&b.target.0)));
    out
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
            },
            FrameData {
                frame_idx: 9,
                target: Label("a".into()),
                x: 9.0,
                y: 0.0,
                scale: 1.0,
                opacity: 1.0,
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
            },
            FrameData {
                frame_idx: 4,
                target: Label("a".into()),
                x: 0.0,
                y: 0.0,
                scale: 1.0,
                opacity: -2.0,
            },
        ];
        let out = interpolate(kf);
        for f in &out {
            assert!((0.0..=1.0).contains(&f.opacity));
        }
    }
}
