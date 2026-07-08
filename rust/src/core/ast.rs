//! Core data structures for the Candy animation pipeline.
//!
//! These types are the single source of truth shared across `parser`, `core`
//! and `renderer`. They are immutable after creation (the only `mut` is the
//! builder-time mutation inside the parser).

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::core::easing::Easing;
use crate::core::meta::PrivateMeta;

/// Unique identifier for an animatable element.
///
/// Matches an `@label` reference in Typst / the `.tyx` DSL. Serialized
/// transparently as the bare string so it can be used as a JSON/map key.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Label(pub String);

impl Label {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Parse a `@name` reference. Returns `None` for anything that is not a
    /// valid label (`@[A-Za-z0-9_-]+`, without the leading `@`).
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim();
        match s.strip_prefix('@') {
            Some(rest)
                if !rest.is_empty()
                    && rest
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') =>
            {
                Some(Label(rest.to_string()))
            }
            _ => None,
        }
    }
}

/// An animation action applied to a target element within a slide.
///
/// Each action carries its own [`Easing`], so a single slide can mix
/// different rate functions per target (e.g. one object moves `linear` while
/// another fades `smooth`).
///
/// # Manim-inspired actions
///
/// Beyond the core transform actions (MoveTo/Scale/Rotate/FadeTo), candy
/// ports several Manim Community animation concepts:
///
/// - **State management**: [`Action::SaveState`] / [`Action::Restore`]
///   mirror `mobject.save_state()` + `Restore(mobject)`. SaveState captures
///   the current transform; Restore interpolates back to it from the current
///   state — the universal "undo" pattern.
/// - **Indication**: [`Action::Indicate`] briefly scales + color-shifts an
///   object to draw attention, then returns to the original state (Manim's
///   `Indicate`). [`Action::Flash`] briefly enlarges and fades out (Manim's
///   `Flash`). [`Action::Wiggle`] oscillates the rotation (Manim's `Wiggle`).
/// - **Color**: [`Action::SetColor`] is a no-op transform that records a
///   color change for the renderer (Typst bodies are opaque, so candy can't
///   truly recolor arbitrary content, but the action is tracked so future
///   versions with structured mobjects can apply it).
/// - **Visibility**: [`Action::Show`] / [`Action::Hide`] are instantaneous
///   (0-duration) visibility toggles, useful for "appear/disappear without
///   fading" effects.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Action {
    // ---- Core transforms (candy v0.1) ----
    /// Move the target so its origin lands at `(x_cm, y_cm)` (absolute).
    MoveTo { target: Label, to: (f64, f64), easing: Easing },
    /// Move the target by a relative offset `(dx_cm, dy_cm)` from its current
    /// position. Mirrors Manim's `mobject.shift(vector)`. Cumulative: calling
    /// MoveBy twice moves the object by the sum of the offsets.
    MoveBy { target: Label, delta: (f64, f64), easing: Easing },
    /// Scale the target uniformly by `to` (1.0 = original size, absolute).
    Scale { target: Label, to: f64, easing: Easing },
    /// Scale the target by a relative factor (e.g. 1.5 = grow 50%). The final
    /// scale is `current * factor`. Mirrors Manim's `mobject.scale(factor)`.
    ScaleBy { target: Label, factor: f64, easing: Easing },
    /// Rotate the target to `degrees` (absolute, clockwise).
    Rotate { target: Label, degrees: f64, easing: Easing },
    /// Rotate the target by a relative `degrees` from its current rotation.
    /// Mirrors Manim's `mobject.rotate(angle)`.
    RotateBy { target: Label, delta_degrees: f64, easing: Easing },
    /// Fade the target in to full opacity.
    FadeIn { target: Label, easing: Easing },
    /// Fade the target out to zero opacity.
    FadeOut { target: Label, easing: Easing },
    /// Fade the target to an explicit `opacity` in `[0, 1]`.
    /// (FadeIn/FadeOut are conveniences for `FadeTo { opacity: 1.0/0.0 }`.)
    FadeTo { target: Label, opacity: f64, easing: Easing },
    /// Move the target along a polyline through `points` (in cm, absolute).
    /// The scheduler generates a keyframe at each point, distributed evenly
    /// across the slide's duration. Mirrors Manim's `MoveAlongPath` (for
    /// linear paths; arc/bezier paths are approximated as polylines).
    MoveAlongPath {
        target: Label,
        points: Vec<(f64, f64)>,
        easing: Easing,
    },

    // ---- Manim-style state management ----
    /// Snapshot the target's current transform (x/y/scale/rotation/opacity)
    /// into a named save slot. The slot can later be restored with
    /// [`Action::Restore`]. Mirrors Manim's `mobject.save_state()`.
    SaveState { target: Label, slot: String },
    /// Interpolate the target from its current state back to a previously
    /// saved state (see [`Action::SaveState`]). Mirrors Manim's
    /// `Restore(mobject)`.
    Restore { target: Label, slot: String, easing: Easing },

    // ---- Manim-style indication animations ----
    /// Briefly scale the target by `factor` (e.g. 1.1) and shift it by
    /// `(dx, dy)` cm, then return to the original state — all within the
    /// slide's duration. Mirrors Manim's `Indicate`. The "return" half uses
    /// the [`Easing::ThereAndBack`] curve internally regardless of the
    /// action's easing (which shapes the "out" half).
    Indicate { target: Label, factor: f64, dx: f64, dy: f64, easing: Easing },
    /// Briefly scale the target up by `factor` and fade it out, returning
    /// to the original state at the end of the slide. Mirrors Manim's `Flash`.
    Flash { target: Label, factor: f64, easing: Easing },
    /// Oscillate the target's rotation by `±degrees` a few times within the
    /// slide's duration, returning to the original rotation. Mirrors Manim's
    /// `Wiggle`. Uses [`Easing::Wiggle`] internally.
    Wiggle { target: Label, degrees: f64, easing: Easing },

    // ---- Visibility (instantaneous, no interpolation) ----
    /// Make the target visible at the slide start (sets opacity to its
    /// "natural" value, typically 1.0). Instantaneous — the action's easing
    /// and the slide's duration are irrelevant.
    Show { target: Label },
    /// Make the target invisible at the slide start (sets opacity to 0).
    /// Instantaneous. Useful for "appear out of nowhere" effects when
    /// combined with a subsequent `FadeIn`.
    Hide { target: Label },

    // ---- Color (tracked for future structured mobjects) ----
    /// Record a color change for the target. The renderer currently treats
    /// this as a no-op (Typst bodies are opaque strings), but the action is
    /// tracked in the timeline so future versions with structured mobjects
    /// can apply it. Mirrors Manim's `set_color`.
    SetColor { target: Label, color: String, easing: Easing },
}

impl Action {
    pub fn target(&self) -> &Label {
        match self {
            Action::MoveTo { target, .. }
            | Action::MoveBy { target, .. }
            | Action::MoveAlongPath { target, .. }
            | Action::Scale { target, .. }
            | Action::ScaleBy { target, .. }
            | Action::Rotate { target, .. }
            | Action::RotateBy { target, .. }
            | Action::FadeIn { target, .. }
            | Action::FadeOut { target, .. }
            | Action::FadeTo { target, .. }
            | Action::SaveState { target, .. }
            | Action::Restore { target, .. }
            | Action::Indicate { target, .. }
            | Action::Flash { target, .. }
            | Action::Wiggle { target, .. }
            | Action::Show { target }
            | Action::Hide { target }
            | Action::SetColor { target, .. } => target,
        }
    }

    /// The easing curve this action will be interpolated with.
    pub fn easing(&self) -> Easing {
        match self {
            Action::MoveTo { easing, .. }
            | Action::MoveBy { easing, .. }
            | Action::MoveAlongPath { easing, .. }
            | Action::Scale { easing, .. }
            | Action::ScaleBy { easing, .. }
            | Action::Rotate { easing, .. }
            | Action::RotateBy { easing, .. }
            | Action::FadeIn { easing, .. }
            | Action::FadeOut { easing, .. }
            | Action::FadeTo { easing, .. }
            | Action::Restore { easing, .. }
            | Action::Indicate { easing, .. }
            | Action::Flash { easing, .. }
            | Action::Wiggle { easing, .. }
            | Action::SetColor { easing, .. } => *easing,
            Action::SaveState { .. } | Action::Show { .. } | Action::Hide { .. } => Easing::Linear,
        }
    }
}

/// One slide (a "shot") of the animation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Slide {
    /// Duration of this slide in **milliseconds**. Must be ≥ 1.
    ///
    /// Internally candy works in milliseconds everywhere; the `--fps` CLI
    /// flag only affects the final video timebase (how many frames per
    /// second are rasterized and encoded). A 1000ms slide at 30fps produces
    /// 30 frames; at 60fps it produces 60 frames — the wall-clock duration
    /// is the same.
    pub duration_ms: u32,
    /// Actions applied across this slide's duration.
    pub actions: Vec<Action>,
}

/// An audio track attached to the timeline (from `candy.audio`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioTrack {
    /// Path to the audio file (`.opus`/`.ogg` for WebM/MKV, `.aac` for MP4).
    pub path: String,
    /// Frame index at which the clip starts playing.
    pub start_ms: u32,
    /// If `true`, the timeline blocks until the clip finishes.
    pub blocking: bool,
    /// If `true`, the clip loops until the next audio/end.
    pub loop_track: bool,
    /// Gain in `[0, 1]`.
    pub volume: f64,
    /// Optional `(start, end)` seconds sub-range of the clip.
    #[serde(default)]
    pub slice: Option<(f64, f64)>,
}

/// Animation scene parsed from `.tyx` or `@preview/candy` output.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Scene {
    pub slides: Vec<Slide>,
    /// CORRECTION (beyond the original spec): the Typst source body for each
    /// animatable item, keyed by `Label`. The spec's `Scene` carried no
    /// per-target content, but `renderer::typst` needs it to emit a frame.
    /// Without it the pipeline cannot render, so it is added here.
    #[serde(default)]
    pub items: HashMap<Label, String>,
    /// Initial per-object transform (frame 0). Seeded from `candy.mobject`'s
    /// `at`/`scale`/`opacity`. Objects absent here default to origin/scale 1.
    #[serde(default)]
    pub initial: HashMap<Label, FrameData>,
    /// Audio tracks attached via `candy.audio`.
    #[serde(default)]
    pub audio: Vec<AudioTrack>,
    /// Page size in Typst points, if the `.tyx` source sets a page size via
    /// `#set page(width:.., height:..)` or `#scene(width:.., height:..)`.
    /// When `None`, the renderer defaults to 16cm × 9cm (16:9 slide).
    #[serde(default)]
    pub page_size: Option<(f64, f64)>,
    pub private_metadata: PrivateMeta,
}

impl Scene {
    /// Mandatory pipeline assertion: every `duration_ms ≥ 1`.
    pub fn validate(&self) -> Result<(), String> {
        for (i, s) in self.slides.iter().enumerate() {
            if s.duration_ms < 1 {
                return Err(format!("slide {i}: duration_ms must be >= 1"));
            }
        }
        Ok(())
    }

    /// Total duration in milliseconds across all slides.
    pub fn total_ms(&self) -> u32 {
        self.slides.iter().map(|s| s.duration_ms).sum()
    }
}

/// Per-frame rendering parameters passed to the renderer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FrameData {
    /// Time offset in **milliseconds** from the start of the animation.
    pub time_ms: u32,
    pub target: Label,
    pub x: f64, // cm
    pub y: f64, // cm
    pub scale: f64, // Default 1.0
    pub opacity: f64, // 0.0–1.0
    /// Clockwise rotation in degrees around the object's origin.
    #[serde(default)]
    pub rotation: f64,
    /// Easing curve used to interpolate *from the previous keyframe* to this
    /// one. Defaults to [`Easing::Linear`].
    #[serde(default)]
    pub easing: Easing,
}

impl FrameData {
    pub fn new(time_ms: u32, target: Label) -> Self {
        Self {
            time_ms,
            target,
            x: 0.0,
            y: 0.0,
            scale: 1.0,
            opacity: 1.0,
            rotation: 0.0,
            easing: Easing::Linear,
        }
    }

    /// Linear interpolation between two keyframes (clamps `t` to [0, 1]).
    pub fn lerp(a: &FrameData, b: &FrameData, t: f64) -> FrameData {
        let t = t.clamp(0.0, 1.0);
        FrameData {
            time_ms: a.time_ms,
            target: a.target.clone(),
            x: lerp(a.x, b.x, t),
            y: lerp(a.y, b.y, t),
            scale: lerp(a.scale, b.scale, t),
            opacity: lerp(a.opacity, b.opacity, t),
            rotation: lerp(a.rotation, b.rotation, t),
            easing: b.easing,
        }
    }
}

/// Linear interpolation helper.
pub fn lerp(a: f64, b: f64, t: f64) -> f64 {
    a + (b - a) * t.clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scene_two_slides() -> Scene {
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
                    actions: vec![Action::Scale {
                        target: Label("a".into()),
                        to: 2.0,
                        easing: Easing::Smooth,
                    }],
                },
            ],
            items: {
                let mut m = HashMap::new();
                m.insert(Label("a".into()), "circle(radius: 1cm)".into());
                m
            },
            initial: HashMap::new(),
            audio: Vec::new(),
            page_size: None,
            private_metadata: PrivateMeta::default(),
        }
    }

    #[test]
    fn label_parse() {
        assert_eq!(Label::parse("@circle"), Some(Label("circle".into())));
        assert_eq!(Label::parse("circle"), None);
        assert_eq!(Label::parse("@bad name"), None);
    }

    #[test]
    fn scene_validates() {
        assert!(scene_two_slides().validate().is_ok());
        let mut s = scene_two_slides();
        s.slides[0].duration_ms = 0;
        assert!(s.validate().is_err());
    }

    #[test]
    fn total_ms_sums() {
        assert_eq!(scene_two_slides().total_ms(), 15000);
    }
}
