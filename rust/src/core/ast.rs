//! Core data structures for the Candy animation pipeline.
//!
//! These types are the single source of truth shared across `parser`, `core`
//! and `renderer`. They are immutable after creation (the only `mut` is the
//! builder-time mutation inside the parser).

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::core::easing::Easing;
use crate::core::meta::PrivateMeta;

/// Centimeters per Typst point (1pt = 1/72in, 1in = 2.54cm).
pub const PT_PER_CM: f64 = 28.346_456_692_913_385;

/// Default canvas size in Typst points: 16 cm × 9 cm (16:9 slide).
pub const DEFAULT_PAGE_PT: (f64, f64) = (16.0 * PT_PER_CM, 9.0 * PT_PER_CM);

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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum PathMode {
    /// Connect the points with straight segments (default; the v0.1 behavior).
    #[default]
    Polyline,
    /// Treat the points as waypoints of a smooth (Catmull-Rom) spline and
    /// sample a dense polyline through them, so motion is curved. Arc/bezier
    /// paths are approximated by this spline. With `orient: true` the object
    /// is additionally rotated to face its direction of travel.
    Bezier,
}

/// A single keyframe inside a [`Action::Track`]. Every transform field is
/// optional; omitted fields carry their *previous* value forward (the object's
/// current state at the start of the slide is the baseline).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrackKey {
    /// Time offset from the slide start, in ms.
    pub t: u32,
    #[serde(default)]
    pub x: Option<f64>,
    #[serde(default)]
    pub y: Option<f64>,
    #[serde(default)]
    pub scale: Option<f64>,
    #[serde(default)]
    pub opacity: Option<f64>,
    #[serde(default)]
    pub rotation: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Action {
    // ---- Core transforms (candy v0.1) ----
    /// Move the target so its origin lands at `(x_cm, y_cm)` (absolute).
    MoveTo {
        target: Label,
        to: (f64, f64),
        easing: Easing,
    },
    /// Move the target by a relative offset `(dx_cm, dy_cm)` from its current
    /// position. Mirrors Manim's `mobject.shift(vector)`. Cumulative: calling
    /// MoveBy twice moves the object by the sum of the offsets.
    MoveBy {
        target: Label,
        delta: (f64, f64),
        easing: Easing,
    },
    /// Scale the target uniformly by `to` (1.0 = original size, absolute).
    Scale {
        target: Label,
        to: f64,
        easing: Easing,
    },
    /// Scale the target by a relative factor (e.g. 1.5 = grow 50%). The final
    /// scale is `current * factor`. Mirrors Manim's `mobject.scale(factor)`.
    ScaleBy {
        target: Label,
        factor: f64,
        easing: Easing,
    },
    /// Rotate the target to `degrees` (absolute, clockwise).
    Rotate {
        target: Label,
        degrees: f64,
        easing: Easing,
    },
    /// Rotate the target by a relative `degrees` from its current rotation.
    /// Mirrors Manim's `mobject.rotate(angle)`.
    RotateBy {
        target: Label,
        delta_degrees: f64,
        easing: Easing,
    },
    /// Fade the target in to full opacity.
    FadeIn { target: Label, easing: Easing },
    /// Fade the target out to zero opacity.
    FadeOut { target: Label, easing: Easing },
    /// Fade the target to an explicit `opacity` in `[0, 1]`.
    /// (FadeIn/FadeOut are conveniences for `FadeTo { opacity: 1.0/0.0 }`.)
    FadeTo {
        target: Label,
        opacity: f64,
        easing: Easing,
    },
    /// Move the target along a path through `points` (in cm). The scheduler
    /// generates a keyframe at each point, distributed evenly across the
    /// slide's duration. `mode` selects `Polyline` (straight segments) or
    /// `Bezier` (a smooth Catmull-Rom spline sampled into a dense polyline;
    /// arc/bezier paths are approximated this way). With `orient: true` and a
    /// `Bezier` path the object is rotated to face its direction of travel.
    /// Mirrors Manim's `MoveAlongPath`.
    MoveAlongPath {
        target: Label,
        points: Vec<(f64, f64)>,
        #[serde(default)]
        mode: PathMode,
        #[serde(default)]
        orient: bool,
        easing: Easing,
    },
    /// Drive a single target through multiple keyframes, each controlling a
    /// subset of its properties (`x`, `y`, `scale`, `opacity`, `rotation`).
    /// Omitted properties carry their previous value forward. This removes the
    /// need for many sequential `#animate`s and mirrors a timeline track.
    Track {
        target: Label,
        keyframes: Vec<TrackKey>,
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
    Restore {
        target: Label,
        slot: String,
        easing: Easing,
    },

    // ---- Manim-style indication animations ----
    /// Briefly scale the target by `factor` (e.g. 1.1) and shift it by
    /// `(dx, dy)` cm, then return to the original state — all within the
    /// slide's duration. Mirrors Manim's `Indicate`. The "return" half uses
    /// the [`Easing::ThereAndBack`] curve internally regardless of the
    /// action's easing (which shapes the "out" half).
    Indicate {
        target: Label,
        factor: f64,
        dx: f64,
        dy: f64,
        easing: Easing,
    },
    /// Briefly scale the target up by `factor` and fade it out, returning
    /// to the original state at the end of the slide. Mirrors Manim's `Flash`.
    Flash {
        target: Label,
        factor: f64,
        easing: Easing,
    },
    /// Oscillate the target's rotation by `±degrees` a few times within the
    /// slide's duration, returning to the original rotation. Mirrors Manim's
    /// `Wiggle`. Uses [`Easing::Wiggle`] internally.
    Wiggle {
        target: Label,
        degrees: f64,
        easing: Easing,
    },

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
    SetColor {
        target: Label,
        color: String,
        easing: Easing,
    },

    // ---- Manim-style content transform ----
    /// Morph a single mobject's content into a new body. Handled natively by
    /// the scheduler (not via generic `apply`): it crossfades the original
    /// `target` content (parked on `old`) out while the transformed `target`
    /// content (swapped in via `Scene.content_timeline` at the slide start)
    /// fades in, both inheriting `target`'s current transform so there is no
    /// positional jump and no scale accumulation. Mirrors Manim's `Transform`.
    Transform {
        target: Label,
        old: Label,
        easing: Easing,
    },

    /// A global camera transform (pan + zoom + rotate) applied to the whole
    /// scene. Implemented as a synthetic `__camera__` mobject whose `x`/`y` are
    /// pan offsets (cm, from the page center), `scale` is the zoom factor, and
    /// `rotation` is the camera tilt (clockwise degrees). The renderer reads it
    /// once per frame and applies it as a wrapping transform; it is never
    /// rendered as a visible object. Mirrors Manim's camera pan/zoom.
    Camera {
        target: Label,
        x: f64,
        y: f64,
        zoom: f64,
        rotate: f64,
        easing: Easing,
    },
}

/// A real shape-morph pair recorded by `#morph(from, to)` (as opposed to the
/// cruder crossfade used for arbitrary content). The renderer precomputes a
/// `MorphPlan` from the two bodies' outlines and, during `[start_ms, end_ms]`,
/// renders the *target* (`to`) as the interpolated shape so the source shape
/// visibly morphs into the target shape (instead of a plain opacity crossfade).
///
/// The pair window matches the `from`→`to` crossfade window emitted by the
/// parser, so the two effects are composited (shape morph on `to`, fade-out on
/// `from`).
///
/// `to_body`, when set, overrides `items[to]` as the *target outline* source for
/// the plan (used by `#transform`, where `to` keeps its original body in
/// `items` until the content-timeline swap, but the morph must interpolate
/// toward the *new* content). The polygon is still emitted for `to`'s label.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MorphPair {
    pub from: Label,
    pub to: Label,
    #[serde(default)]
    pub to_body: Option<String>,
    pub start_ms: u32,
    pub end_ms: u32,
    pub easing: Easing,
}

/// A single glyph / sub-formula fragment used by the per-character `Transform`
/// morph. The renderer lays out `body` in isolation to recover its absolute
/// position on the page; during the transform each fragment interpolates its
/// `(x, y)` (cm, page origin) from `from_*` (the old content's layout) to
/// `to_*` (the new content's layout), and its `opacity` from `from_opacity` to
/// `to_opacity` (1 → 1 for matched glyphs, 1 → 0 for old-only, 0 → 1 for
/// new-only). `body` is the full Typst content to render the fragment (e.g.
/// `[a]`, `[+]`, `[=]`, or a longer run that could not be split further).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CharFragment {
    pub body: String,
    /// Page-anchored top-left of the fragment in the *old* content (cm).
    pub from_x: f64,
    pub from_y: f64,
    /// Page-anchored top-left of the fragment in the *new* content (cm).
    pub to_x: f64,
    pub to_y: f64,
    /// Opacity at window start (old-only fragments: 1, others: 1).
    pub from_opacity: f64,
    /// Opacity at window end (new-only fragments: 1, others: 1).
    pub to_opacity: f64,
}

/// A Manim-style per-glyph `Transform` plan for one `#transform(target, to: …)`
/// call whose old/new bodies are inline content. `target` is the label whose
/// content is being replaced; `old` is the synthetic parked mobject holding the
/// old content (used only as a fallback / for the crossfade safety net).
/// `old_body` / `new_body` are the raw bodies so the renderer can re-measure
/// and split them into glyph fragments. `fragments` is empty at parse time and
/// filled in by the renderer's `ensure_natural` (which does the splitting +
/// layout). During `[start_ms, end_ms]` the renderer composites the
/// interpolated fragments *over* `target` so the old content visibly
/// disassembles and reassembles into the new content instead of dissolving as
/// one block.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransformPlan {
    pub target: Label,
    pub old: Label,
    pub old_body: String,
    pub new_body: String,
    #[serde(default)]
    pub fragments: Vec<CharFragment>,
    pub start_ms: u32,
    pub end_ms: u32,
    pub easing: Easing,
}

impl Action {
    pub fn target(&self) -> &Label {
        match self {
            Action::MoveTo { target, .. }
            | Action::MoveBy { target, .. }
            |             Action::MoveAlongPath { target, .. }
            | Action::Track { target, .. }
            | Action::Camera { target, .. }
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
            | Action::SetColor { target, .. }
            | Action::Transform { target, .. } => target,
        }
    }

    /// The easing curve this action will be interpolated with.
    pub fn easing(&self) -> Easing {
        match self {
            Action::MoveTo { easing, .. }
            | Action::MoveBy { easing, .. }
            |             Action::MoveAlongPath { easing, .. }
            | Action::Track { easing, .. }
            | Action::Camera { easing, .. }
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
            | Action::SetColor { easing, .. }
            | Action::Transform { easing, .. } => easing.clone(),
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
    /// CORRECTION (beyond the original spec): a per-label **content timeline**
    /// recording when an mobject's body is swapped to a new one (used by
    /// `transform`). Each entry is `(time_ms, new_body)`: for a given frame,
    /// the renderer uses the latest `new_body` whose `time_ms <= frame`, else
    /// falls back to `items[label]`. This lets a single label hold different
    /// content before/after a `transform` without corrupting earlier slides'
    /// rendered content.
    #[serde(default)]
    pub content_timeline: HashMap<Label, Vec<(u32, String)>>,
    /// Real shape-morph pairs recorded by `#morph(from, to)`. The renderer
    /// precomputes an outline interpolator per pair and morphs the `to` body's
    /// shape across each pair's window. Empty unless `#morph` is used.
    #[serde(default)]
    pub morph_pairs: Vec<MorphPair>,
    /// Per-glyph fragment morph plans recorded by `#transform(target, to: …)`
    /// when both the old and new bodies are inline content (e.g. formulas /
    /// text). Each plan drives a Manim-style `Transform`: the old content is
    /// broken into independent glyph fragments that each move / fade toward the
    /// matching fragment of the new content, while unmatched fragments fade out
    /// (old-only) or in (new-only). This replaces the previous single crossfade
    /// (which looked "stiff" because the whole block dissolved at once) and the
    /// single largest-outline polygon blob (which did not resemble the formula).
    /// Empty for shape transforms (which keep the outline morph) or when no
    /// inline `#transform` is used.
    #[serde(default)]
    pub transform_plans: Vec<TransformPlan>,
    /// Initial per-object transform (frame 0). Seeded from `candy.mobject`'s
    /// `at`/`scale`/`opacity`. Objects absent here default to origin/scale 1.
    #[serde(default)]
    pub initial: HashMap<Label, FrameData>,
    /// Audio tracks attached via `candy.audio`.
    #[serde(default)]
    pub audio: Vec<AudioTrack>,
    /// Top-level source lines re-injected into candy's per-object compile
    /// snippets — which are detached Typst modules. This holds:
    /// * `@preview`/package import lines (e.g. `#import "@preview/cetz:0.3.0": *`)
    ///   so mobject bodies can reference external packages, and
    /// * user-defined top-level `#let` helpers (e.g. `#let star(c, s: 0.35cm) = …`)
    ///   so a body like `star(white)` resolves instead of failing with
    ///   "unknown variable: star".
    /// Local relative imports are intentionally excluded (they would not
    /// resolve in a detached module).
    #[serde(default)]
    pub imports: Vec<String>,
    /// Page size in Typst points, if the `.tyx` source sets a page size via
    /// `#set page(width:.., height:..)` or `#scene(width:.., height:..)`.
    /// When `None`, the renderer defaults to 16cm × 9cm (16:9 slide).
    #[serde(default)]
    pub page_size: Option<(f64, f64)>,
    /// Subtitle overlays (the "字幕模块"). Each caption is shown over the
    /// animation at a fixed anchor, persists (by default) until replaced by
    /// another subtitle in the same Typst scope or until its scope exits, and
    /// is subject to parental shadowing.
    #[serde(default)]
    pub subtitles: Vec<Subtitle>,
    /// Named integer counters (the "缓动计数器模块"). Key-value store of
    /// animatable integer values referenced from mobject/subtitle bodies.
    #[serde(default)]
    pub counters: Vec<CounterDef>,
    /// Runtime lifecycle events for counters (`pause` / `resume` / `destroy`).
    #[serde(default)]
    pub counter_events: Vec<CounterEvent>,
    /// Lexical Typst scope intervals on the timeline. Drives auto-destroy on
    /// scope exit and parental shadowing for both subtitles and counters.
    #[serde(default)]
    pub scopes: Vec<ScopeInfo>,
    /// Nested scene tree (see the scene semantics in `docs` / `typst/README`).
    /// The implicit root scene (id `0`) always exists and owns every mobject /
    /// action not declared inside an explicit `#scene(...)`. Each explicit
    /// `#scene(...)` becomes a child scene with its own page size + timeline
    /// interval; entering a child scene hides its parent (auto-hide). When
    /// `scenes` is empty (legacy input) the whole document is treated as a
    /// single implicit scene and behaves exactly as before.
    #[serde(default)]
    pub scenes: Vec<SceneInfo>,
    /// The implicit root scene id (always `Some(0)` once parsed). `None` means
    /// "legacy single-scene document" — behavior is identical to v0.1.
    #[serde(default)]
    pub root_scene: Option<usize>,
    /// Group parent map: child label → parent label. Used by the renderer to
    /// compose group transforms (parent→child). Functional data lives here,
    /// not in `private_metadata`.
    #[serde(default)]
    pub groups: HashMap<Label, Label>,
    pub private_metadata: PrivateMeta,
}

impl Scene {
    /// Depth of a scene in the scene tree (root = 0). Returns `0` for an
    /// unknown scene (treated as a top-level alias).
    pub fn scene_depth(&self, id: usize) -> usize {
        let mut depth = 0;
        let mut cur = self.scenes.iter().find(|s| s.id == id).and_then(|s| s.parent);
        while let Some(p) = cur {
            depth += 1;
            cur = self.scenes.iter().find(|s| s.id == p).and_then(|s| s.parent);
        }
        depth
    }

    /// The active scene at timeline time `time_ms` — the *deepest* scene whose
    /// `[start_ms, end_ms]` interval contains `time_ms`. This is what makes
    /// "entering a child scene hides the parent" work: at any moment exactly
    /// one scene (the innermost enclosing one) is visible.
    pub fn active_scene_at(&self, time_ms: u32) -> usize {
        let mut best: Option<usize> = None;
        let mut best_depth = 0usize;
        for s in &self.scenes {
            if time_ms >= s.start_ms && time_ms <= s.end_ms {
                let depth = self.scene_depth(s.id);
                if best.is_none() || depth > best_depth {
                    best = Some(s.id);
                    best_depth = depth;
                }
            }
        }
        best.or(self.root_scene).unwrap_or(0)
    }

    /// Resolve the effective canvas size (in Typst points) for `scene_id`,
    /// inheriting from the nearest ancestor that declares a page size, then
    /// the root scene, then the 16:9 default. A scene that declares no
    /// `width`/`height` therefore fills its parent's canvas.
    pub fn effective_page_pt(&self, scene_id: usize) -> (f64, f64) {
        let mut cur = Some(scene_id);
        while let Some(id) = cur {
            if let Some(s) = self.scenes.iter().find(|s| s.id == id) {
                if let Some(p) = s.page_size {
                    return p;
                }
                cur = s.parent;
            } else {
                break;
            }
        }
        DEFAULT_PAGE_PT
    }

    /// Map every mobject label to the id of the scene that owns it.
    pub fn label_scene_map(&self) -> HashMap<Label, usize> {
        let mut m = HashMap::new();
        for s in &self.scenes {
            for l in &s.owns_labels {
                m.insert(l.clone(), s.id);
            }
        }
        m
    }
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
    pub x: f64,       // cm
    pub y: f64,       // cm
    pub scale: f64,   // Default 1.0
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
            easing: b.easing.clone(),
        }
    }
}

/// Linear interpolation helper.
pub fn lerp(a: f64, b: f64, t: f64) -> f64 {
    a + (b - a) * t.clamp(0.0, 1.0)
}

// ============================================================================
// Subtitle module
// ============================================================================

/// Anchor position for a subtitle overlay, measured from the page's top-left
/// corner. `Absolute(x, y)` is in centimeters.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SubPos {
    /// Default anchor when `position:` is omitted.
    Bottom,
    Top,
    Center,
    BottomLeft,
    BottomRight,
    TopLeft,
    TopRight,
    /// Absolute position in cm from the top-left of the page.
    Absolute(f64, f64),
}

impl Default for SubPos {
    fn default() -> Self {
        SubPos::Bottom
    }
}

/// A subtitle (caption) overlay rendered over the animation.
///
/// Lifetime rules (Typst-scope aware):
/// - Default: persists until *replaced* by another subtitle in the **same**
///   scope, or until its **scope exits** (auto-destroy).
/// - Within a single Typst scope only **one** subtitle may be visible at a
///   time; a later one replaces an earlier one at its `start_ms`.
/// - A subtitle in a **parent** scope is **temporarily hidden** while a child
///   scope shows its own subtitle (shadowing).
/// - `body` may be any valid Typst block content.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Subtitle {
    /// Unique id assigned by the parser.
    pub id: String,
    /// Lexical Typst scope id (for shadowing / auto-destroy).
    pub scope: String,
    /// Raw Typst body source (any valid Typst block).
    pub body: String,
    /// Start time on the timeline (ms).
    pub start_ms: u32,
    /// Explicit end time (ms). `None` ⇒ persist until replaced or scope exit.
    #[serde(default)]
    pub end_ms: Option<u32>,
    /// Anchor position on the page.
    #[serde(default)]
    pub position: SubPos,
    /// Easing used for the caption's own fade-in / fade-out.
    #[serde(default)]
    pub easing: Easing,
}

impl Subtitle {
    /// Resolve the absolute anchor position in **cm** from the page top-left,
    /// given the page size in cm. `subtitle_margin_cm` is the inset from the
    /// edge for the named anchors.
    pub fn abs_cm(&self, page_w_cm: f64, page_h_cm: f64, margin: f64) -> (f64, f64) {
        match self.position {
            SubPos::Absolute(x, y) => (x, y),
            SubPos::Bottom => (page_w_cm / 2.0, page_h_cm - margin),
            SubPos::Top => (page_w_cm / 2.0, margin),
            SubPos::Center => (page_w_cm / 2.0, page_h_cm / 2.0),
            SubPos::BottomLeft => (margin, page_h_cm - margin),
            SubPos::BottomRight => (page_w_cm - margin, page_h_cm - margin),
            SubPos::TopLeft => (margin, margin),
            SubPos::TopRight => (page_w_cm - margin, margin),
        }
    }
}

// ============================================================================
// Easing-counter module
// ============================================================================

/// A named integer counter ("easing counter").
///
/// Key-value store of animatable integers referenced from mobject / subtitle
/// bodies via `ecval(name)`. The value is:
/// - under **standard Typst**, the integer `seed`;
/// - in **animation** mode, `seed` stepping over time, the ramp shaped by the
///   counter's easing (when a `duration` is given) or stepping once per ms
///   (long-lived, linear) otherwise.
///
/// Scope rules follow Typst: a counter in a child scope **shadows** a parent
/// scope counter of the same name. It can be `pause`d / `resume`d / `destroy`ed,
/// and auto-destroys when its scope exits.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CounterDef {
    /// Counter name (the key).
    pub name: String,
    /// Lexical Typst scope id (for shadowing).
    pub scope: String,
    /// Integer seed (standard-Typst return value, and the value at start).
    pub seed: i64,
    /// Per-step increment (signed integer).
    pub step: i64,
    /// Optional duration (ms). `None` ⇒ long-lived (steps every ms forever).
    #[serde(default)]
    pub duration_ms: Option<u32>,
    /// Easing applied to the ramp (ignored when `duration_ms` is `None`).
    #[serde(default)]
    pub easing: Easing,
    /// Start time on the timeline (ms).
    pub start_ms: u32,
}

/// A runtime lifecycle event mutating a counter.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CounterEventKind {
    Pause,
    Resume,
    Destroy,
}

/// A `pause` / `resume` / `destroy` event on a named counter, anchored on the
/// timeline at `at_ms`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CounterEvent {
    pub name: String,
    pub kind: CounterEventKind,
    pub at_ms: u32,
}

// ============================================================================
// Lexical scope tracking (used by both subtitles and counters)
// ============================================================================

/// A lexical Typst scope interval on the timeline.
///
/// Scopes nest: a block `{ ... }` opens a child scope whose `start_ms` is the
/// cursor when the block is entered and `end_ms` the cursor when it is left.
/// This interval drives auto-destroy on scope exit and parental shadowing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScopeInfo {
    pub id: usize,
    /// Parent scope id (`None` for the root scope).
    #[serde(default)]
    pub parent: Option<usize>,
    pub start_ms: u32,
    pub end_ms: u32,
}

/// A scene in the animation — a nestable, scope-bounded, one-page segment of
/// the timeline.
///
/// Scenes form a tree rooted at the implicit root scene (id `0`). Each
/// explicit `#scene(...)` in the `.tyx` source becomes a child `SceneInfo`
/// with:
/// - its own `page_size` (canvas in Typst points; `None` ⇒ inherit parent),
/// - a `[start_ms, end_ms]` timeline interval (derived from the parse cursor
///   when the scene's body opens / closes),
/// - the set of mobjects (`owns_labels`) declared inside its body.
///
/// Semantics (see `typst/README.md` → *Scene / canvas*):
/// - scenes may be **nested**;
/// - entering a child scene **auto-hides** its parent (the renderer shows
///   only the innermost active scene's content at any frame);
/// - scenes **respect Typst's lexical scope** — a mobject belongs to the
///   innermost scene that encloses it at parse time;
/// - a scene occupies **one page**; content that would overflow is split into
///   multiple scenes (auto-split, enforced by the author / warned by candy);
/// - with **no explicit root scene**, the whole document is one implicit scene
///   (id `0`), following the same split rules.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SceneInfo {
    /// Unique scene id (root = `0`).
    pub id: usize,
    /// Parent scene id (`None` for the root scene).
    #[serde(default)]
    pub parent: Option<usize>,
    /// The lexical Typst scope id this scene occupies (for attribution).
    pub scope: usize,
    /// Canvas size in Typst points `(w, h)`. `None` ⇒ inherit from parent,
    /// then the root, then the 16:9 default.
    #[serde(default)]
    pub page_size: Option<(f64, f64)>,
    /// Background fill for this scene, as the raw Typst color expression
    /// captured from `#scene(bg: …)` (e.g. `white`, `rgb("#05060f")`). `None`
    /// ⇒ inherit from the parent scene, then the root, then opaque white.
    /// The renderer resolves this to an actual color (SVG/video) so the
    /// configured background actually shows up in the output frames.
    #[serde(default)]
    pub bg: Option<String>,
    /// Scene timeline interval (ms). The root spans `[0, total]`.
    pub start_ms: u32,
    pub end_ms: u32,
    /// Mobject labels declared inside this scene's body.
    #[serde(default)]
    pub owns_labels: Vec<Label>,
}

impl Scene {
    /// Depth of a scope in the scope tree (root = 0). Returns `0` for an
    /// unknown scope (treated as a top-level alias).
    fn scope_depth(&self, id: usize) -> usize {
        let mut depth = 0;
        let mut cur = self
            .scopes
            .iter()
            .find(|s| s.id == id)
            .and_then(|s| s.parent);
        while let Some(p) = cur {
            depth += 1;
            cur = self
                .scopes
                .iter()
                .find(|s| s.id == p)
                .and_then(|s| s.parent);
        }
        depth
    }

    /// Is `maybe_child` a descendant scope of `ancestor`?
    fn is_descendant_scope(&self, maybe_child: usize, ancestor: usize) -> bool {
        let mut cur = self
            .scopes
            .iter()
            .find(|s| s.id == maybe_child)
            .and_then(|s| s.parent);
        while let Some(p) = cur {
            if p == ancestor {
                return true;
            }
            cur = self
                .scopes
                .iter()
                .find(|s| s.id == p)
                .and_then(|s| s.parent);
        }
        false
    }

    /// Resolve the integer value of counter `name` at timeline time `time_ms`,
    /// honoring Typst-scope shadowing (innermost active counter wins) and the
    /// `pause` / `resume` / `destroy` lifecycle.
    ///
    /// - Before a counter's `start_ms` (or if undefined) → its `seed`.
    /// - With a `duration`: value ramps `seed → seed + step·duration`, shaped by
    ///   the easing function of the *effective* elapsed time (paused intervals
    ///   are subtracted; `destroy` freezes the value at the destroy time).
    /// - Without a `duration` (long-lived): value = `seed + step · elapsed`
    ///   (one integer step per ms; linear — easing needs a bounded ramp).
    pub fn counter_value_at(&self, name: &str, time_ms: u32) -> i64 {
        // Collect candidate counters named `name` that have started.
        let mut candidates: Vec<&CounterDef> = self
            .counters
            .iter()
            .filter(|c| c.name == name && c.start_ms <= time_ms)
            .collect();
        if candidates.is_empty() {
            // Not started yet (or never): return seed if defined, else 0.
            return self
                .counters
                .iter()
                .find(|c| c.name == name)
                .map(|c| c.seed)
                .unwrap_or(0);
        }
        // Shadowing: innermost (deepest) active scope wins.
        candidates.sort_by_key(|c| {
            std::cmp::Reverse(self.scope_depth(c.scope.parse::<usize>().unwrap_or(0)))
        });
        let c = candidates[0];

        // Determine freeze time (destroy) and paused total.
        let mut freeze_at: Option<u32> = None;
        for ev in &self.counter_events {
            if ev.name == name {
                if let CounterEventKind::Destroy = ev.kind {
                    if ev.at_ms <= time_ms {
                        freeze_at = Some(freeze_at.map_or(ev.at_ms, |f| f.max(ev.at_ms)));
                    }
                }
            }
        }
        let eval_time = freeze_at.unwrap_or(time_ms);
        let elapsed_raw = eval_time.saturating_sub(c.start_ms);

        // Subtract paused intervals (pause..resume) up to eval_time.
        let mut paused: u32 = 0;
        let mut open_pause: Option<u32> = None;
        for ev in &self.counter_events {
            if ev.name != name {
                continue;
            }
            match ev.kind {
                CounterEventKind::Pause => {
                    if ev.at_ms <= eval_time && open_pause.is_none() {
                        open_pause = Some(ev.at_ms);
                    }
                }
                CounterEventKind::Resume => {
                    if let Some(p) = open_pause.take() {
                        if ev.at_ms <= eval_time {
                            paused += ev.at_ms.saturating_sub(p);
                        } else {
                            paused += eval_time.saturating_sub(p);
                        }
                    }
                }
                CounterEventKind::Destroy => {}
            }
        }
        if let Some(p) = open_pause {
            paused += eval_time.saturating_sub(p);
        }

        let elapsed = elapsed_raw.saturating_sub(paused);
        let elapsed_f = elapsed as f64;

        let value = match c.duration_ms {
            Some(d) if d > 0 => {
                let progress = (elapsed_f / d as f64).clamp(0.0, 1.0);
                let eased = c.easing.resolve()(progress);
                (c.seed as f64 + c.step as f64 * d as f64 * eased).round() as i64
            }
            _ => c.seed + (c.step as f64 * elapsed_f).round() as i64,
        };
        value
    }

    /// The set of **visible** subtitles at `time_ms` (after applying one-per-
    /// scope replacement and parental shadowing). Returns the subtitle ids.
    pub fn visible_subtitle_ids_at(&self, time_ms: u32) -> Vec<String> {
        // 1. Per scope, find the active subtitle (last one whose start <= time
        //    and whose end > time). `end` = end_ms, else scope end, else the
        //    next same-scope subtitle's start.
        let mut active: Vec<&Subtitle> = Vec::new();
        let mut by_scope: std::collections::HashMap<String, Vec<&Subtitle>> =
            std::collections::HashMap::new();
        for s in &self.subtitles {
            if s.start_ms > time_ms {
                continue;
            }
            by_scope.entry(s.scope.clone()).or_default().push(s);
        }
        for (scope, mut subs) in by_scope {
            subs.sort_by_key(|s| s.start_ms);
            // Find the latest one active at `time_ms`.
            let mut chosen: Option<&Subtitle> = None;
            for s in &subs {
                let scope_end = self
                    .scopes
                    .iter()
                    .find(|sc| sc.id.to_string() == scope)
                    .map(|sc| sc.end_ms)
                    .unwrap_or(u32::MAX);
                let end = s.end_ms.unwrap_or(scope_end);
                let next_start = subs
                    .iter()
                    .skip_while(|x| x.start_ms <= s.start_ms)
                    .find(|x| x.start_ms > s.start_ms)
                    .map(|x| x.start_ms);
                let effective_end = next_start.map_or(end, |n| end.min(n));
                if time_ms < effective_end {
                    chosen = Some(s);
                }
            }
            if let Some(c) = chosen {
                active.push(c);
            }
        }
        // 2. Shadowing: drop a subtitle if a *descendant* scope has an active
        //    subtitle (parent hidden while child shows its own).
        let visible: Vec<String> = active
            .iter()
            .filter(|s| {
                let sid = s.scope.parse::<usize>().unwrap_or(0);
                !active.iter().any(|o| {
                    let oid = o.scope.parse::<usize>().unwrap_or(0);
                    oid != sid && self.is_descendant_scope(oid, sid)
                })
            })
            .map(|s| s.id.clone())
            .collect();
        visible
    }
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
            content_timeline: HashMap::new(),
            initial: HashMap::new(),
            audio: Vec::new(),
            imports: Vec::new(),
            page_size: None,
            subtitles: Vec::new(),
            counters: Vec::new(),
            counter_events: Vec::new(),
            scopes: Vec::new(),
            scenes: Vec::new(),
            root_scene: None,
            morph_pairs: Vec::new(),
            transform_plans: Vec::new(),
            groups: HashMap::new(),
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
