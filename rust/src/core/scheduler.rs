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
    rotation: f64,
}

impl Default for State {
    fn default() -> Self {
        Self {
            x: 0.0,
            y: 0.0,
            scale: 1.0,
            opacity: 1.0,
            rotation: 0.0,
        }
    }
}

/// Generate keyframe `FrameData` from the `Scene` AST.
///
/// Precondition: every `slide.duration_ms ≥ 1` (enforced by
/// `Scene::validate`).
/// Postcondition: returns `Vec<FrameData>`; for every `Action::MoveTo` the
/// `time_ms` increments monotonically within a single target's keyframe list
/// (validated below). Every animatable item also gets a frame-0 default
/// keyframe (seeded from `scene.initial`) and a final keyframe at the last
/// frame.
///
/// Errors: returns `CandyError::Parse` (E002) if a non-monotonic `time_ms`
/// is detected for a target — previously this panicked, violating spec §6
/// ("production code must not panic").
pub fn schedule(scene: &Scene) -> Result<Vec<FrameData>, CandyError> {
    // Seed each item's starting state from `scene.initial` (the `candy.mobject`
    // `at`/`scale`/`opacity`/`rotation`), falling back to the origin/scale-1 default.
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
                    rotation: f.rotation,
                })
                .unwrap_or_default();
            (l.clone(), s)
        })
        .collect();

    // Named save slots for SaveState/Restore (Manim's save_state/Restore).
    // Keyed by (target, slot_name).
    let mut saved: HashMap<(Label, String), State> = HashMap::new();

    let mut per_item: HashMap<Label, Vec<FrameData>> = state
        .keys()
        .map(|l| (l.clone(), vec![FrameData::new(0, l.clone())]))
        .collect();

    let mut ptr: u32 = 0;
    for slide in &scene.slides {
        let start = ptr;
        let end = ptr + slide.duration_ms;

        for action in &slide.actions {
            let t = action.target().clone();
            let s = *state.get(&t).unwrap_or(&State::default());
            let easing = action.easing();

            match action {
                // ---- Instantaneous actions: no keyframes, just state change ----
                Action::SaveState { slot, .. } => {
                    saved.insert((t.clone(), slot.clone()), s);
                    // No keyframes — SaveState is a snapshot, not an animation.
                    continue;
                }
                Action::Show { .. } => {
                    let ns = State { opacity: 1.0, ..s };
                    state.insert(t.clone(), ns);
                    // Single keyframe at slide start with full opacity.
                    per_item.entry(t.clone()).or_default().push(FrameData {
                        time_ms: start,
                        target: t.clone(),
                        x: ns.x,
                        y: ns.y,
                        scale: ns.scale,
                        opacity: ns.opacity,
                        rotation: ns.rotation,
                        easing: Easing::Linear,
                    });
                    continue;
                }
                Action::Hide { .. } => {
                    let ns = State { opacity: 0.0, ..s };
                    state.insert(t.clone(), ns);
                    per_item.entry(t.clone()).or_default().push(FrameData {
                        time_ms: start,
                        target: t.clone(),
                        x: ns.x,
                        y: ns.y,
                        scale: ns.scale,
                        opacity: ns.opacity,
                        rotation: ns.rotation,
                        easing: Easing::Linear,
                    });
                    continue;
                }
                Action::SetColor { .. } => {
                    // Color is tracked but doesn't affect transform state.
                    // No keyframes; the renderer will handle color in a future
                    // version with structured mobjects.
                    continue;
                }

                // ---- Indication animations: out-and-back within the slide ----
                Action::Indicate { factor, dx, dy, .. } => {
                    // Start keyframe = current state.
                    per_item.entry(t.clone()).or_default().push(FrameData {
                        time_ms: start,
                        target: t.clone(),
                        x: s.x, y: s.y, scale: s.scale, opacity: s.opacity, rotation: s.rotation,
                        easing,
                    });
                    // Mid keyframe (halfway) = scaled + shifted.
                    let mid = ptr + slide.duration_ms / 2;
                    let peak = State {
                        scale: s.scale * factor,
                        x: s.x + dx,
                        y: s.y + dy,
                        ..s
                    };
                    per_item.entry(t.clone()).or_default().push(FrameData {
                        time_ms: mid,
                        target: t.clone(),
                        x: peak.x, y: peak.y, scale: peak.scale, opacity: peak.opacity, rotation: peak.rotation,
                        easing: Easing::ThereAndBack,
                    });
                    // End keyframe = back to original (state unchanged).
                    per_item.entry(t.clone()).or_default().push(FrameData {
                        time_ms: end,
                        target: t.clone(),
                        x: s.x, y: s.y, scale: s.scale, opacity: s.opacity, rotation: s.rotation,
                        easing,
                    });
                    // State unchanged — Indicate returns to origin.
                    continue;
                }
                Action::Flash { factor, .. } => {
                    // Start = current, mid = scaled up + fading, end = invisible.
                    per_item.entry(t.clone()).or_default().push(FrameData {
                        time_ms: start,
                        target: t.clone(),
                        x: s.x, y: s.y, scale: s.scale, opacity: s.opacity, rotation: s.rotation,
                        easing,
                    });
                    let mid = ptr + slide.duration_ms / 2;
                    let peak = State {
                        scale: s.scale * factor,
                        opacity: s.opacity * 0.5,
                        ..s
                    };
                    per_item.entry(t.clone()).or_default().push(FrameData {
                        time_ms: mid,
                        target: t.clone(),
                        x: peak.x, y: peak.y, scale: peak.scale, opacity: peak.opacity, rotation: peak.rotation,
                        easing: Easing::ThereAndBack,
                    });
                    // End: restored to original (Flash is a transient effect).
                    per_item.entry(t.clone()).or_default().push(FrameData {
                        time_ms: end,
                        target: t.clone(),
                        x: s.x, y: s.y, scale: s.scale, opacity: s.opacity, rotation: s.rotation,
                        easing,
                    });
                    continue;
                }
                Action::Wiggle { degrees, .. } => {
                    // Oscillate rotation. Start at 0, peak at ±degrees at mid,
                    // return to 0 at end. The interpolator's Wiggle easing
                    // handles the oscillation shape.
                    per_item.entry(t.clone()).or_default().push(FrameData {
                        time_ms: start,
                        target: t.clone(),
                        x: s.x, y: s.y, scale: s.scale, opacity: s.opacity, rotation: s.rotation,
                        easing: Easing::Wiggle,
                    });
                    let mid = ptr + slide.duration_ms / 2;
                    let peak = State { rotation: s.rotation + degrees, ..s };
                    per_item.entry(t.clone()).or_default().push(FrameData {
                        time_ms: mid,
                        target: t.clone(),
                        x: peak.x, y: peak.y, scale: peak.scale, opacity: peak.opacity, rotation: peak.rotation,
                        easing: Easing::Wiggle,
                    });
                    per_item.entry(t.clone()).or_default().push(FrameData {
                        time_ms: end,
                        target: t.clone(),
                        x: s.x, y: s.y, scale: s.scale, opacity: s.opacity, rotation: s.rotation,
                        easing: Easing::Wiggle,
                    });
                    continue;
                }

                // ---- Restore: interpolate from current → saved ----
                Action::Restore { slot, .. } => {
                    let saved_state = saved.get(&(t.clone(), slot.clone())).copied().unwrap_or(s);
                    per_item.entry(t.clone()).or_default().push(FrameData {
                        time_ms: start,
                        target: t.clone(),
                        x: s.x, y: s.y, scale: s.scale, opacity: s.opacity, rotation: s.rotation,
                        easing,
                    });
                    per_item.entry(t.clone()).or_default().push(FrameData {
                        time_ms: end,
                        target: t.clone(),
                        x: saved_state.x, y: saved_state.y, scale: saved_state.scale, opacity: saved_state.opacity, rotation: saved_state.rotation,
                        easing,
                    });
                    state.insert(t.clone(), saved_state);
                    continue;
                }

                // ---- MoveAlongPath: keyframe at each point ----
                Action::MoveAlongPath { points, .. } => {
                    if points.is_empty() {
                        continue;
                    }
                    let n = points.len() as u32;
                    let seg = slide.duration_ms / n.max(1);
                    // Start keyframe = current state.
                    per_item.entry(t.clone()).or_default().push(FrameData {
                        time_ms: start,
                        target: t.clone(),
                        x: s.x, y: s.y, scale: s.scale, opacity: s.opacity, rotation: s.rotation,
                        easing,
                    });
                    // One keyframe per path point, evenly distributed.
                    for (i, &(px, py)) in points.iter().enumerate() {
                        let kf_frame = start + (i as u32 + 1) * seg;
                        let kf_frame = kf_frame.min(end);
                        per_item.entry(t.clone()).or_default().push(FrameData {
                            time_ms: kf_frame,
                            target: t.clone(),
                            x: px, y: py, scale: s.scale, opacity: s.opacity, rotation: s.rotation,
                            easing,
                        });
                    }
                    // Update state to the last point.
                    let last = *points.last().unwrap();
                    state.insert(t.clone(), State { x: last.0, y: last.1, ..s });
                    continue;
                }

                // ---- Core transforms: start keyframe + end keyframe ----
                _ => {
                    // Keyframe at the slide start = current state.
                    per_item.entry(t.clone()).or_default().push(FrameData {
                        time_ms: start,
                        target: t.clone(),
                        x: s.x,
                        y: s.y,
                        scale: s.scale,
                        opacity: s.opacity,
                        rotation: s.rotation,
                        easing,
                    });

                    apply(&mut state, &t, action);

                    let s = state[&t];
                    // Keyframe at the slide end = new state, carrying the action's
                    // easing so the interpolator knows how to shape the curve from
                    // `start` to `end`.
                    per_item.entry(t.clone()).or_default().push(FrameData {
                        time_ms: end,
                        target: t.clone(),
                        x: s.x,
                        y: s.y,
                        scale: s.scale,
                        opacity: s.opacity,
                        rotation: s.rotation,
                        easing,
                    });
                }
            }
        }

        ptr = end;
    }

    // Ensure every item has a keyframe at the final frame.
    let last = ptr.saturating_sub(1);
    for (l, st) in &state {
        per_item.entry(l.clone()).or_default().push(FrameData {
            time_ms: last,
            target: l.clone(),
            x: st.x,
            y: st.y,
            scale: st.scale,
            opacity: st.opacity,
            rotation: st.rotation,
            easing: Easing::Linear,
        });
    }

    let mut all: Vec<FrameData> = per_item.into_values().flatten().collect();
    all.sort_by(|a, b| a.time_ms.cmp(&b.time_ms).then(a.target.0.cmp(&b.target.0)));

    // Mandatory validation: monotonic time_ms per target. Returns E002
    // (Parse) instead of panicking, honoring spec §6.
    validate_monotonic(&all)?;
    Ok(all)
}

/// Apply a single action to a target's state.
///
/// Only called for core transform actions (MoveTo/Scale/Rotate/FadeIn/
/// FadeOut/FadeTo). Indication animations, SaveState/Restore, Show/Hide, and
/// SetColor are handled directly in `schedule` because they need custom
/// keyframe logic.
fn apply(state: &mut HashMap<Label, State>, t: &Label, action: &Action) {
    let s = *state.get(t).unwrap_or(&State::default());
    let ns = match action {
        Action::MoveTo { to, .. } => State {
            x: to.0,
            y: to.1,
            ..s
        },
        Action::MoveBy { delta, .. } => State {
            x: s.x + delta.0,
            y: s.y + delta.1,
            ..s
        },
        Action::Scale { to, .. } => State { scale: *to, ..s },
        Action::ScaleBy { factor, .. } => State {
            scale: s.scale * factor,
            ..s
        },
        Action::Rotate { degrees, .. } => State {
            rotation: *degrees,
            ..s
        },
        Action::RotateBy { delta_degrees, .. } => State {
            rotation: s.rotation + delta_degrees,
            ..s
        },
        Action::FadeIn { .. } => State {
            opacity: 1.0,
            ..s
        },
        Action::FadeOut { .. } => State {
            opacity: 0.0,
            ..s
        },
        Action::FadeTo { opacity, .. } => State {
            opacity: *opacity,
            ..s
        },
        // The following actions are handled directly in `schedule` and never
        // reach apply(). Listed here so the match is exhaustive.
        Action::SaveState { .. }
        | Action::Restore { .. }
        | Action::MoveAlongPath { .. }
        | Action::Indicate { .. }
        | Action::Flash { .. }
        | Action::Wiggle { .. }
        | Action::Show { .. }
        | Action::Hide { .. }
        | Action::SetColor { .. } => s,
    };
    state.insert(t.clone(), ns);
}

/// Validation helper: within each target's keyframe list, `time_ms` must be
/// non-decreasing. Returns `CandyError::Parse` (E002) on violation.
fn validate_monotonic(frames: &[FrameData]) -> Result<(), CandyError> {
    let mut last: Option<(Label, u32)> = None;
    for f in frames {
        if let Some((ref lbl, idx)) = last {
            if lbl == &f.target && f.time_ms < idx {
                return Err(CandyError::Parse(format!(
                    "scheduler: non-monotonic time_ms for @{} ({} < {})",
                    f.target.0, f.time_ms, idx
                )));
            }
        }
        last = Some((f.target.clone(), f.time_ms));
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
                    duration_ms: 10000,
                    actions: vec![Action::MoveTo {
                        target: Label("a".into()),
                        to: (3.0, 0.0),
                        easing: Easing::Linear,
                    }],
                },
                Slide {
                    duration_ms: 5000,
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
            imports: Vec::new(),
            page_size: None,
            private_metadata: PrivateMeta::default(),
        }
    }

    #[test]
    fn keyframes_cover_bounds() {
        let kf = schedule(&scene()).unwrap();
        // first keyframe at frame 0, last at 15000ms (10000+5000)
        assert_eq!(kf.iter().map(|f| f.time_ms).min(), Some(0));
        assert_eq!(kf.iter().map(|f| f.time_ms).max(), Some(15000));
        // frame 0 default + start/end for slide0 + start/end for slide1 + final
        assert!(kf.len() >= 5);
    }

    /// The action's easing must propagate to the keyframes so the
    /// interpolator can shape the curve.
    #[test]
    fn easing_propagates_to_keyframes() {
        let kf = schedule(&scene()).unwrap();
        // slide 1 (FadeOut, Smooth) keyframes: find the end keyframe at 15000ms.
        let end = kf.iter().find(|f| f.target.0 == "a" && f.time_ms == 15000);
        let end = end.expect("end keyframe at 15000ms must exist");
        assert_eq!(end.easing, Easing::Smooth);
    }

    /// Regression: scheduler must NOT panic on non-monotonic input. Previously
    /// `assert_monotonic` called `panic!`; now it returns `CandyError::Parse`.
    #[test]
    fn non_monotonic_returns_err_not_panic() {
        let frames = vec![
            FrameData { time_ms: 5000, target: Label("x".into()), x: 0.0, y: 0.0, scale: 1.0, opacity: 1.0, rotation: 0.0, easing: Easing::Linear },
            FrameData { time_ms: 3000, target: Label("x".into()), x: 1.0, y: 0.0, scale: 1.0, opacity: 1.0, rotation: 0.0, easing: Easing::Linear },
        ];
        let err = validate_monotonic(&frames).unwrap_err();
        assert_eq!(err.code(), "E002");
        assert!(err.to_string().contains("non-monotonic"));
    }
}
