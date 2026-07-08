//! Y-axis scheduling: turn a `Scene` AST into keyframe `FrameData`.

use std::collections::HashMap;

use crate::core::ast::{Action, FrameData, Label, Scene};
use crate::core::easing::Easing;
use crate::core::error::CandyError;

/// Per-target animation state. Internal to the scheduler.
#[derive(Debug, Clone, Copy)]
struct State {
    x: f64,
    y: f64,
    scale: f64,
    opacity: f64,
}

impl Default for State {
    fn default() -> Self {
        Self {
            x: 0.0,
            y: 0.0,
            scale: 1.0,
            opacity: 1.0,
        }
    }
}

/// Generate keyframe `FrameData` from the `Scene` AST.
///
/// Precondition: every `slide.duration_frames ≥ 1` (enforced by
/// `Scene::validate`).
/// Postcondition: returns `Vec<FrameData>`; for every `Action::MoveTo` the
/// `frame_idx` increments monotonically within a single target's keyframe list
/// (validated below). Every animatable item also gets a frame-0 default
/// keyframe (seeded from `scene.initial`) and a final keyframe at the last
/// frame.
///
/// Errors: returns `CandyError::Parse` (E002) if a non-monotonic `frame_idx`
/// is detected for a target — previously this panicked, violating spec §6
/// ("production code must not panic").
pub fn schedule(scene: &Scene) -> Result<Vec<FrameData>, CandyError> {
    // Seed each item's starting state from `scene.initial` (the `candy.mobject`
    // `at`/`scale`/`opacity`), falling back to the origin/scale-1 default.
    let mut state: HashMap<Label, State> = scene
        .items
        .keys()
        .map(|l| {
            let s = scene
                .initial
                .get(l)
                .map(|f| State {
                    x: f.x,
                    y: f.y,
                    scale: f.scale,
                    opacity: f.opacity,
                })
                .unwrap_or_default();
            (l.clone(), s)
        })
        .collect();

    let mut per_item: HashMap<Label, Vec<FrameData>> = state
        .keys()
        .map(|l| (l.clone(), vec![FrameData::new(0, l.clone())]))
        .collect();

    let mut ptr: u32 = 0;
    for slide in &scene.slides {
        let start = ptr;
        let end = ptr + slide.duration_frames.saturating_sub(1);

        for action in &slide.actions {
            let t = action.target().clone();
            let s = *state.get(&t).unwrap_or(&State::default());
            let easing = action.easing();

            // Keyframe at the slide start = current state.
            per_item.entry(t.clone()).or_default().push(FrameData {
                frame_idx: start,
                target: t.clone(),
                x: s.x,
                y: s.y,
                scale: s.scale,
                opacity: s.opacity,
                easing,
            });

            apply(&mut state, &t, action);

            let s = state[&t];
            // Keyframe at the slide end = new state, carrying the action's
            // easing so the interpolator knows how to shape the curve from
            // `start` to `end`.
            per_item.entry(t.clone()).or_default().push(FrameData {
                frame_idx: end,
                target: t.clone(),
                x: s.x,
                y: s.y,
                scale: s.scale,
                opacity: s.opacity,
                easing,
            });
        }

        ptr = end + 1;
    }

    // Ensure every item has a keyframe at the final frame.
    let last = ptr.saturating_sub(1);
    for (l, st) in &state {
        per_item.entry(l.clone()).or_default().push(FrameData {
            frame_idx: last,
            target: l.clone(),
            x: st.x,
            y: st.y,
            scale: st.scale,
            opacity: st.opacity,
            easing: Easing::Linear,
        });
    }

    let mut all: Vec<FrameData> = per_item.into_values().flatten().collect();
    all.sort_by(|a, b| a.frame_idx.cmp(&b.frame_idx).then(a.target.0.cmp(&b.target.0)));

    // Mandatory validation: monotonic frame_idx per target. Returns E002
    // (Parse) instead of panicking, honoring spec §6.
    validate_monotonic(&all)?;
    Ok(all)
}

/// Apply a single action to a target's state.
fn apply(state: &mut HashMap<Label, State>, t: &Label, action: &Action) {
    let s = *state.get(t).unwrap_or(&State::default());
    let ns = match action {
        Action::MoveTo { to, .. } => State {
            x: to.0,
            y: to.1,
            ..s
        },
        Action::Scale { to, .. } => State { scale: *to, ..s },
        Action::FadeIn { .. } => State {
            opacity: 1.0,
            ..s
        },
        Action::FadeOut { .. } => State {
            opacity: 0.0,
            ..s
        },
    };
    state.insert(t.clone(), ns);
}

/// Validation helper: within each target's keyframe list, `frame_idx` must be
/// non-decreasing. Returns `CandyError::Parse` (E002) on violation.
fn validate_monotonic(frames: &[FrameData]) -> Result<(), CandyError> {
    let mut last: Option<(Label, u32)> = None;
    for f in frames {
        if let Some((ref lbl, idx)) = last {
            if lbl == &f.target && f.frame_idx < idx {
                return Err(CandyError::Parse(format!(
                    "scheduler: non-monotonic frame_idx for @{} ({} < {})",
                    f.target.0, f.frame_idx, idx
                )));
            }
        }
        last = Some((f.target.clone(), f.frame_idx));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::ast::{Action, Label, Scene, Slide};
    use crate::core::easing::Easing;
    use crate::core::meta::PrivateMeta;

    fn scene() -> Scene {
        Scene {
            slides: vec![
                Slide {
                    duration_frames: 10,
                    actions: vec![Action::MoveTo {
                        target: Label("a".into()),
                        to: (3.0, 0.0),
                        easing: Easing::Linear,
                    }],
                },
                Slide {
                    duration_frames: 5,
                    actions: vec![Action::FadeOut {
                        target: Label("a".into()),
                        easing: Easing::Smooth,
                    }],
                },
            ],
            items: {
                let mut m = std::collections::HashMap::new();
                m.insert(Label("a".into()), String::new());
                m
            },
            initial: std::collections::HashMap::new(),
            audio: Vec::new(),
            private_metadata: PrivateMeta::default(),
        }
    }

    #[test]
    fn keyframes_cover_bounds() {
        let kf = schedule(&scene()).unwrap();
        // first keyframe at frame 0, last at frame 14 (10 + 5 - 1)
        assert_eq!(kf.iter().map(|f| f.frame_idx).min(), Some(0));
        assert_eq!(kf.iter().map(|f| f.frame_idx).max(), Some(14));
        // frame 0 default + start/end for slide0 + start/end for slide1 + final
        assert!(kf.len() >= 5);
    }

    /// The action's easing must propagate to the keyframes so the
    /// interpolator can shape the curve.
    #[test]
    fn easing_propagates_to_keyframes() {
        let kf = schedule(&scene()).unwrap();
        // slide 1 (FadeOut, Smooth) keyframes: find the end keyframe at frame 14.
        let end = kf.iter().find(|f| f.target.0 == "a" && f.frame_idx == 14);
        let end = end.expect("end keyframe at frame 14 must exist");
        assert_eq!(end.easing, Easing::Smooth);
    }

    /// Regression: scheduler must NOT panic on non-monotonic input. Previously
    /// `assert_monotonic` called `panic!`; now it returns `CandyError::Parse`.
    #[test]
    fn non_monotonic_returns_err_not_panic() {
        let frames = vec![
            FrameData { frame_idx: 5, target: Label("x".into()), x: 0.0, y: 0.0, scale: 1.0, opacity: 1.0, easing: Easing::Linear },
            FrameData { frame_idx: 3, target: Label("x".into()), x: 1.0, y: 0.0, scale: 1.0, opacity: 1.0, easing: Easing::Linear },
        ];
        let err = validate_monotonic(&frames).unwrap_err();
        assert_eq!(err.code(), "E002");
        assert!(err.to_string().contains("non-monotonic"));
    }
}
