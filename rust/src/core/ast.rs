//! Core data structures for the Candy animation pipeline.
//!
//! These types are the single source of truth shared across `parser`, `core`
//! and `renderer`. They are immutable after creation (the only `mut` is the
//! builder-time mutation inside the parser).

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Action {
    /// Move the target so its origin lands at `(x_cm, y_cm)`.
    MoveTo { target: Label, to: (f64, f64) },
    /// Scale the target uniformly by `to` (1.0 = original size).
    Scale { target: Label, to: f64 },
    /// Fade the target in to full opacity.
    FadeIn { target: Label },
    /// Fade the target out to zero opacity.
    FadeOut { target: Label },
}

impl Action {
    pub fn target(&self) -> &Label {
        match self {
            Action::MoveTo { target, .. }
            | Action::Scale { target, .. }
            | Action::FadeIn { target }
            | Action::FadeOut { target } => target,
        }
    }
}

/// One slide (a "shot") of the animation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Slide {
    /// Number of frames this slide lasts. Must be ≥ 1.
    pub duration_frames: u32,
    /// Actions applied across this slide's duration.
    pub actions: Vec<Action>,
}

/// An audio track attached to the timeline (from `candy.audio`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioTrack {
    /// Path to the audio file (`.opus`/`.ogg` for WebM/MKV, `.aac` for MP4).
    pub path: String,
    /// Frame index at which the clip starts playing.
    pub start_frame: u32,
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
    pub private_metadata: PrivateMeta,
}

impl Scene {
    /// Mandatory pipeline assertion: every `duration_frames ≥ 1`.
    ///
    /// NOTE: an empty `slides` list is now allowed — it produces a single static
    /// first-frame (e.g. a `.tyx` that only declares `mobject`s with no
    /// `animate`). Such a file is still valid standard Typst.
    pub fn validate(&self) -> Result<(), String> {
        for (i, s) in self.slides.iter().enumerate() {
            if s.duration_frames < 1 {
                return Err(format!("slide {i}: duration_frames must be >= 1"));
            }
        }
        Ok(())
    }

    /// Total frame count across all slides (0 when there are no slides).
    pub fn total_frames(&self) -> u32 {
        self.slides.iter().map(|s| s.duration_frames).sum()
    }
}

/// Per-frame rendering parameters passed to the renderer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FrameData {
    pub frame_idx: u32,
    pub target: Label,
    pub x: f64, // cm
    pub y: f64, // cm
    pub scale: f64, // Default 1.0
    pub opacity: f64, // 0.0–1.0
}

impl FrameData {
    pub fn new(frame_idx: u32, target: Label) -> Self {
        Self {
            frame_idx,
            target,
            x: 0.0,
            y: 0.0,
            scale: 1.0,
            opacity: 1.0,
        }
    }

    /// Linear interpolation between two keyframes (clamps `t` to [0, 1]).
    pub fn lerp(a: &FrameData, b: &FrameData, t: f64) -> FrameData {
        let t = t.clamp(0.0, 1.0);
        FrameData {
            frame_idx: a.frame_idx,
            target: a.target.clone(),
            x: lerp(a.x, b.x, t),
            y: lerp(a.y, b.y, t),
            scale: lerp(a.scale, b.scale, t),
            opacity: lerp(a.opacity, b.opacity, t),
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
                    duration_frames: 10,
                    actions: vec![Action::MoveTo {
                        target: Label("a".into()),
                        to: (3.0, 0.0),
                    }],
                },
                Slide {
                    duration_frames: 5,
                    actions: vec![Action::Scale {
                        target: Label("a".into()),
                        to: 2.0,
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
        s.slides[0].duration_frames = 0;
        assert!(s.validate().is_err());
    }

    #[test]
    fn total_frames_sums() {
        assert_eq!(scene_two_slides().total_frames(), 15);
    }
}
