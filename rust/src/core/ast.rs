//! Core data structures for the Candy animation pipeline.
//!
//! These types are the single source of truth shared across `parser`, `core`
//! and `renderer`. They are immutable after creation (the only `mut` is the
//! builder-time mutation inside the parser).

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::core::diag::{CandyError, SourceLoc};
use crate::core::easing::Easing;
use crate::core::meta::PrivateMeta;

/// Typst points per centimeter (1cm = 28.346pt = 72/2.54 pt).
pub const PT_PER_CM: f64 = 28.346_456_692_913_385;

/// Default canvas size in Typst points: 16 cm × 9 cm (16:9 slide).
pub const DEFAULT_PAGE_PT: (f64, f64) = (16.0 * PT_PER_CM, 9.0 * PT_PER_CM);

/// Global canvas / export configuration for a Candy animation, declared once
/// per `.tyx` via the `candy` show rule:
///
/// ```typst
/// #show: candy
/// #show: candy.with(width: 13.33in, height: 7.5in, ppi: 144, fps: 30)
/// ```
///
/// `width_pt` / `height_pt` are the *viewport* dimensions in Typst points.
/// The rendering canvas equals `(width_pt, height_pt)` for every scene (the
/// page size is derived from the global config, not per-scene `width` /
/// `height` — those were deprecated in favor of `candy`). `ppi` drives
/// rasterization resolution and `fps` the output frame rate.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct GlobalConfig {
    pub width_pt: f64,
    pub height_pt: f64,
    pub ppi: u32,
    pub fps: u32,
}
impl GlobalConfig {
    /// The default global config, matching `show: candy` with no overrides:
    /// `width: 13.33in`, `height: 7.5in`, `ppi: 144`, `fps: 30`.
    pub const DEFAULT: GlobalConfig = GlobalConfig {
        width_pt: 13.33 * 72.0,
        height_pt: 7.5 * 72.0,
        ppi: 144,
        fps: 30,
    };
}

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
/// - **Color**: [`Action::SetColor`] lerps the mobject's `fill:`/`stroke:`
///   from its current paint to the target color over `duration` milliseconds
///   along `easing`, so the color change is animated by the renderer.
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

/// Sequencing of an object animation relative to the previous one on the
/// timeline. Mirrors the PowerPoint animation-pane "Start" options:
///
/// - [`Timing::After`] (default): begin once the previous animation finishes
///   (PPT "Start: After Previous").
/// - [`Timing::With`]: begin at the same time as the previous animation, i.e.
///   run in parallel with it (PPT "Start: With Previous").
///
/// The Rust parser resolves `timing` + `delay` into each [`Slide`]'s absolute
/// `start_ms`; the scheduler consumes `start_ms` directly, so this enum is not
/// serialized (it lives only in the parse step).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Timing {
    /// Start after the previous animation ends.
    #[default]
    After,
    /// Start together with the previous animation (parallel).
    With,
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
    /// Scale the target by a relative factor (e.g. 130% = grow 30%). The final
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

    // ---- Color (animated by the renderer) ----
    /// Record a color change for the target. The renderer lerps the mobject's
    /// `fill:`/`stroke:` from its current paint to `color` over `duration`
    /// milliseconds along `easing`. Mirrors Manim's `set_color`.
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

    // ---- Named scene switching ----
    /// Switch to a named scene by its `name` (e.g., `"intro"`, `"demo"`).
    /// This is a timeline-jump action: the scheduler records a transition that
    /// makes the target scene active from this point onward. The target scene
    /// must exist in `scenes` with a matching `name` field. If the target is
    /// an anonymous scene, use its auto-assigned UUID name (e.g.,
    /// `"scene_a1b2c3d4"`).
    ///
    /// When a named scene is switched to via this action:
    /// - If the target is a **sibling** scene at the same hierarchy level,
    ///   it replaces the current scene on canvas (mutual exclusion).
    /// - If the target is a **nested** child scene, it enters that nested scope
    ///   (auto-hiding parent per nested scene semantics).
    /// - Mobjects not registered in the target scene's `owns_labels` are hidden
    ///   (only the target scene's mobjects are visible).
    SceneSwitch {
        /// The name of the target scene to switch to.
        target: String,
        /// Duration of the transition effect in ms (0 = instant jump).
        #[serde(default)]
        duration_ms: u32,
        /// Easing for any fade transition during the switch.
        #[serde(default)]
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
/// filled in by the renderer's `ensure_flow` (which does the splitting +
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
    pub fn target(&self) -> Option<&Label> {
        match self {
            Action::MoveTo { target, .. }
            | Action::MoveBy { target, .. }
            | Action::MoveAlongPath { target, .. }
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
            | Action::Transform { target, .. } => Some(target),
            // SceneSwitch doesn't target a mobject label; it targets a scene by name.
            Action::SceneSwitch { .. } => None,
        }
    }

    /// The easing curve this action will be interpolated with.
    pub fn easing(&self) -> Easing {
        match self {
            Action::MoveTo { easing, .. }
            | Action::MoveBy { easing, .. }
            | Action::MoveAlongPath { easing, .. }
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
            Action::SaveState { .. }
            | Action::Show { .. }
            | Action::Hide { .. }
            | Action::SceneSwitch { .. } => Easing::Linear,
        }
    }
}

/// One slide (a "shot") of the animation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Slide {
    /// Absolute start time of this slide on the timeline, in **milliseconds**.
    ///
    /// Resolved by the parser from the directive's `timing` (`after`/`with`)
    /// and `delay` parameters. The scheduler places the slide's keyframes at
    /// `[start_ms, start_ms + duration_ms)`. Defaults to `0` for hand-built
    /// (test) scenes that don't set it.
    #[serde(default)]
    pub start_ms: u32,
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
    /// Source location of the directive that produced this slide (best-effort,
    /// set by the parser's `emit_slide` from `ParseCtx::current_directive_loc`).
    /// Lets structural `E002`/`Parse` errors (e.g. `duration_ms < 1`) point at
    /// the offending directive rather than only at the slide index. `None` for
    /// synthetic slides (the injected empty slide, hand-built test scenes).
    /// Skipped on (de)serialization: a `SourceLoc` is only meaningful within a
    /// single parse / error-reporting run, and `SourceLoc` is not `Serialize`.
    #[serde(skip)]
    pub loc: Option<SourceLoc>,
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
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
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
    ///
    /// Local relative imports are intentionally excluded (they would not
    /// resolve in a detached module).
    #[serde(default)]
    pub imports: Vec<String>,
    /// Page size in Typst points, if the `.tyx` source sets a page size via
    /// `#set page(width:.., height:..)` or `#scene(width:.., height:..)`.
    /// When `None`, the renderer defaults to 16cm × 9cm (16:9 slide).
    /// Page size in Typst points, if the `.tyx` source sets a page size via
    /// `#set page(width:.., height:..)` (or the global `candy` show rule).
    /// When `None`, the renderer defaults to 16cm × 9cm (16:9 slide). The page
    /// (not the scene) owns the size; scenes never configure width/height.
    #[serde(default)]
    pub page_size: Option<(f64, f64)>,
    /// Subtitle overlays (the "subtitle module"). Each caption is shown over the
    /// animation at a fixed anchor, persists (by default) until replaced by
    /// another subtitle in the same Typst scope or until its scope exits, and
    /// is subject to parental shadowing.
    #[serde(default)]
    pub subtitles: Vec<Subtitle>,
    /// Named integer counters (the "easing-counter module"). Key-value store of
    /// animatable integer values referenced from mobject/subtitle bodies.
    #[serde(default)]
    pub counters: Vec<CounterDef>,
    /// Runtime lifecycle events for counters (`pause` / `resume` / `destroy`).
    #[serde(default)]
    pub counter_events: Vec<CounterEvent>,
    /// Keyframe counters (the "keyframe-counter module", `kc*`). Discrete
    /// (time → value) keyframe tracks referenced from mobject / subtitle bodies.
    #[serde(default)]
    pub kcdefs: Vec<KeyframeCounterDef>,
    /// Runtime lifecycle events for keyframe counters (`pause` / `resume` /
    /// `destroy`). Reuses `CounterEvent` but kept separate from `counter_events`
    /// so a `kcpause("x")` never affects a same-named `ecnew("x")`.
    #[serde(default)]
    pub kc_events: Vec<CounterEvent>,
    /// Lexical Typst scope intervals on the timeline. Drives auto-destroy on
    /// scope exit and parental shadowing for both subtitles and counters.
    #[serde(default)]
    pub scopes: Vec<ScopeInfo>,
    /// Scene list (see the scene semantics in `docs` / `typst/README`). Scenes
    /// are flat: a `#scene(...)` may only appear at the document root. The
    /// implicit whole-document scene (id `0`) always exists and owns every
    /// mobject / action not declared inside an explicit `#scene(...)`. When
    /// `scenes` is empty the whole document is the single implicit scene.
    #[serde(default)]
    pub scenes: Vec<SceneInfo>,
    /// Group parent map: child label → parent label. A group is a special kind
    /// of mobject — an mobject may own child mobjects, and animating the parent
    /// transforms all of its children together (parent→child inheritance). The
    /// renderer composes group transforms using this map; the parent label is
    /// itself a normal mobject (registered in `items` / `initial`) but is never
    /// drawn directly. Functional data lives here, not in `private_metadata`.
    #[serde(default)]
    pub groups: HashMap<Label, Label>,
    /// Parse artifacts needed by the **per-frame whole-document recompiler**
    /// (Phase 2): the original `.tyx` source plus the source ranges of every
    /// `#mobject(label, body)` body and every `#scene(...)`. The renderer
    /// reconstructs each frame as a complete Typst document by splicing the
    /// per-frame wrapped mobject bodies back into the original source, so it
    /// keeps the document's flow layout, Z-order and all non-candy Typst
    /// content (prose, equations, `#play` blocks, …) faithful to `typst
    /// compile`. Skipped from serialization (it is a re-derivable cache of the
    /// source) and defaulted so synthetic `Scene`s (tests) stay trivial.
    #[serde(skip, default)]
    pub artifacts: ParseArtifacts,
    pub private_metadata: PrivateMeta,
}

/// Re-derivable parse artifacts for the per-frame whole-document recompiler.
///
/// See `Scene::artifacts`. All fields are default-empty so a `Scene` built
/// without parsing (e.g. unit tests) carries no artifacts and the renderer
/// transparently falls back to its legacy per-object compositing path.
/// A region of the **expanded** source (the single flat document produced by
/// recursively inlining `#include "rel"` statements) that came from an *included*
/// file. When a diagnostic byte offset lands inside `[start, end)`, the deepest
/// trace frame points at the *actual* error inside the included file (via
/// `inc_path`/`inc_raw`), and the includer's `#include "rel"` call-sites are kept
/// in `chain` so the reporter can print them as a layer-by-layer "included from
/// …" trace. This makes candy's source tracking honest for included files: an
/// error inside `b.tyx` (pulled in by `#include "b.tyx"`) points at the real
/// offending line in `b.tyx`, then walks up the include chain.
#[derive(Debug, Clone)]
pub struct IncludeRegion {
    /// Byte range `[start, end)` in the expanded source that belongs to the
    /// inlined content of the referenced file. `expanded[start..end]` equals
    /// `inc_raw` exactly (the spliced content), so an offset inside this range
    /// maps directly to the same offset inside `inc_raw`.
    pub start: usize,
    pub end: usize,
    /// Absolute path of the file that *contains the `#include "..."` call* (the
    /// includer), not the included file itself.
    pub ref_path: std::path::PathBuf,
    /// The includer file's *original* (un-expanded) source text, retained so the
    /// reported `line:col` is computed against the real file the user wrote.
    pub ref_raw: String,
    /// Byte range of the whole `#include "..."` call within `ref_raw`.
    pub ref_range: std::ops::Range<usize>,
    /// Absolute path of the *included* file whose content occupies `[start, end)`
    /// — i.e. where the actual error lives. For a nested include `a → b → c`, the
    /// innermost region's `inc_path` is `c` (the error originates there).
    pub inc_path: std::path::PathBuf,
    /// The included file's inlined content occupying `[start, end)` in the
    /// expanded source (the fully-expanded text, in the same coordinate space as
    /// that range). Used to build the deepest trace frame pointing at the real
    /// error line inside the included file.
    pub inc_raw: String,
    /// Full include chain from the outermost includer (the root document's
    /// direct child) down to — and including — the immediate includer
    /// (`ref_path`/`ref_raw`/`ref_range` == `chain.last()`). For a top-level
    /// include this is exactly one entry; for a nested include `a → b → c`
    /// it is `[a's #include "b", b's #include "c"]`, printed as a layer-by-layer
    /// "included from …" trace behind the deepest (error) frame.
    pub chain: Vec<(std::path::PathBuf, String, std::ops::Range<usize>)>,
}

/// Map of every inlined-include region in the expanded source. Built bottom-up
/// during [`crate::parser::ast_walk::expand_includes`]: each nested region's
/// offsets are shifted into the final expanded-string coordinates as its parent
/// is spliced in.
#[derive(Debug, Clone, Default)]
pub struct SourceMap {
    pub regions: Vec<IncludeRegion>,
}

impl SourceMap {
    /// If `offset` falls inside an inlined include region, return the
    /// *innermost* such region (so the caller reports the actual error inside
    /// the deepest included file). For a nested include `a → b → c`, the erroring
    /// content in `c` is contained by both `c`'s region and `b`'s (outer)
    /// region; we want `c`'s (the smallest one), so the deepest trace frame
    /// points at the real error inside `c` and the chain then walks outward.
    pub(crate) fn region_for(&self, offset: usize) -> Option<&IncludeRegion> {
        self.regions
            .iter()
            .filter(|r| offset >= r.start && offset < r.end)
            .min_by_key(|r| r.end - r.start)
    }
}

#[derive(Debug, Clone, Default)]
pub struct ParseArtifacts {
    /// The original `.tyx` source text (markup, as parsed by `typst_syntax`).
    pub source: String,
    /// Absolute path of the `.tyx` file this scene was parsed from. Empty for
    /// hand-built / programmatic `Scene`s. Threaded into the renderer so an
    /// `E005` Typst failure can point at the real file rather than the synthetic
    /// `main.typ` detached source.
    pub file_path: PathBuf,
    /// Source map for the (expanded) `source`: every inlined `#include "rel"`
    /// region is recorded together with the `#include` call-site that pulled it
    /// in (the "referenced position"). Lets a diagnostic pointing at content
    /// inside an included file trace back to the `#include` line in the includer,
    /// so the user is pointed at the file they wrote rather than at a
    /// meaningless offset inside the concatenated document.
    pub source_map: SourceMap,
    /// Source range `(start, end)` of each `#mobject(label, body)` call's
    /// `body` argument expression, keyed by label. Used to splice the
    /// per-frame wrapped body back into `source`.
    pub mobject_body: HashMap<Label, (usize, usize)>,
    /// Source range `(start, end)` of each explicit `#scene(...)` call — the
    /// *entire* `FuncCall` (not just its body), keyed by scene id. Used to gate
    /// scenes with `sys.inputs.at("candy:active_scene")` so the whole-document
    /// recompile emits exactly one page per frame (the active scene), keeping
    /// memory bounded and the `body_cache` hit rate high.
    pub scene_call: HashMap<usize, (usize, usize)>,
    /// Source range `(start, end)` of each `#subtitle(...)` call — the *entire*
    /// call **including its leading `#`** — keyed by the subtitle's generated
    /// id. The whole-document recompiler blanks each one out (replacing it with
    /// `#none`) so the caption is **not** rendered as part of the base document:
    /// captions are drawn as a separate, camera-independent overlay, and leaving
    /// the `#subtitle[...]` body in the base would double-render it (once warped
    /// by the global camera, once as the fixed overlay) — the "base unfiltered subtitle"
    /// rendering anomaly.
    pub subtitle_call: HashMap<String, (usize, usize)>,
    /// Source location of every label's *declaration* (`#mobject("x", …)` /
    /// `#ecnew("x", …)`), keyed by label. Used to point the user at the
    /// exact code when a label later causes a diagnostic (e.g. `E004`
    /// LabelNotFound). Not serialized (it is a re-derivable cache of the
    /// source), default-empty so synthetic `Scene`s (tests) stay trivial.
    pub label_locs: HashMap<Label, SourceLoc>,
    /// Source location of every *name reference* (a target label or
    /// easing-counter name written inside a directive), keyed by the resolved
    /// name string. Lets name-anomaly errors (`E004` LabelNotFound / `E006`
    /// UnknownKey) point at the *usage* site when the name is never declared.
    /// Default-empty so synthetic `Scene`s (tests) stay trivial.
    pub name_ref_locs: HashMap<String, SourceLoc>,
    /// Global canvas / export config declared via the `candy` show rule
    /// (`show: candy` / `show: candy.with(...)`). Defaults to
    /// [`GlobalConfig::DEFAULT`]. The rendering canvas equals
    /// `(config.width_pt, config.height_pt)` for every scene. Not serialized
    /// as part of the document body; it is a re-derivable cache of the
    /// `candy` config, defaulting for synthetic (hand-built) `Scene`s.
    pub config: GlobalConfig,
}

impl Scene {
    /// The active scene at timeline time `time_ms` — the scene whose
    /// `[start_ms, end_ms]` interval contains `time_ms`. Scenes are flat (no
    /// nesting), so at any moment exactly one scene is visible. If no scene's
    /// interval contains `time_ms`, the first scene in document order is
    /// returned (so the timeline always maps to a real scene).
    pub fn active_scene_at(&self, time_ms: u32) -> usize {
        let mut best: Option<usize> = None;
        for s in &self.scenes {
            if time_ms >= s.start_ms && time_ms <= s.end_ms {
                best = Some(s.id);
            }
        }
        best.or_else(|| self.scenes.first().map(|s| s.id))
            .unwrap_or(0)
    }

    /// Resolve the effective canvas size (in Typst points) for `scene_id`,
    /// falling back to the 16:9 default when the scene declares no measured
    /// page size. After `parse_tyx` each scene's `page_size` is set from the
    /// document's `#set page(...)` or the global `candy` show rule, so size is
    /// owned by the page, never by `#scene` arguments.
    pub fn effective_page_pt(&self, scene_id: usize) -> (f64, f64) {
        if let Some(s) = self.scenes.iter().find(|s| s.id == scene_id) {
            if let Some(p) = s.page_size {
                return p;
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

    /// Look up a scene by its human-readable `name`. Returns the scene id if
    /// found, `None` otherwise. Anonymous scenes (name = `None`) are not
    /// included — use [`Scene::resolve_scene_id`] which also accepts UUID-style
    /// names like `"scene_a1b2c3d4"`.
    pub fn find_scene_by_name(&self, name: &str) -> Option<usize> {
        self.scenes
            .iter()
            .find(|s| s.name.as_deref() == Some(name))
            .map(|s| s.id)
    }

    /// Resolve a scene-switch target string to a scene id. This handles both:
    /// - Named scenes (exact match on `name`)
    /// - Anonymous scenes matched by their auto-assigned UUID-like name
    ///   (e.g., `"scene_a1b2c3d4"` matches an anonymous scene whose internal
    ///   resolved name is that UUID).
    ///
    /// Returns `None` if no matching scene is found.
    pub fn resolve_scene_id(&self, target: &str) -> Option<usize> {
        // Direct name match (named scene or resolved UUID name).
        if let Some(id) = self.find_scene_by_name(target) {
            return Some(id);
        }
        // Also check by scene id as string (for numeric references).
        if let Ok(id) = target.parse::<usize>() {
            if self.scenes.iter().any(|s| s.id == id) {
                return Some(id);
            }
        }
        None
    }

    /// Assign deterministic UUID-like names to all anonymous scenes (those with
    /// `name = None`). Uses `scene_<hex_of_id>_<index>` format so names are
    /// stable across runs. This is called during/after parsing so that anonymous
    /// scenes can still be referenced via `#switch(target: "scene_<hex>")`.
    pub fn assign_anonymous_names(scenes: &mut [SceneInfo]) {
        use std::collections::HashSet;

        // Collect already-used explicit names.
        let mut used_names: HashSet<String> =
            scenes.iter().filter_map(|s| s.name.clone()).collect();

        // Track how many anonymous scenes we've assigned per scene id
        // (for handling multiple anonymous scenes with same id edge case).
        let mut anon_counter: usize = 0;

        for scene in scenes.iter_mut() {
            if scene.name.is_none() {
                // Generate a deterministic name based on scene id + counter.
                // Format: "scene_<8-char-hex>" where hex is derived from id.
                let hex_id = format!("{:08x}", scene.id);
                let base_name = format!("scene_{}", &hex_id[..8.min(hex_id.len())]);

                let name = if used_names.contains(&base_name) {
                    // Collision - append counter.
                    let candidate = format!("{}_{:04x}", base_name, anon_counter);
                    anon_counter += 1;
                    candidate
                } else {
                    base_name
                };

                used_names.insert(name.clone());
                scene.name = Some(name);
            }
        }
    }
}

impl Scene {
    /// Mandatory pipeline assertion. Returns the precise [`CandyError`] so callers
    /// can propagate it directly (no `map_err` that would flatten everything to
    /// `Parse`/E002): a bad `duration_ms` is `Parse` (E002), while an undeclared
    /// `mobject`/`counter` reference is `UnknownKey` (E006) — matching the error
    /// code the message always advertised.
    pub fn validate(&self) -> Result<(), CandyError> {
        for (i, s) in self.slides.iter().enumerate() {
            if s.duration_ms < 1 {
                return Err(CandyError::Parse(
                    format!("slide {i}: duration_ms must be >= 1"),
                    self.slides[i].loc.clone(),
                ));
            }
        }
        // Validate counter lifecycle events reference declared counters.
        let counter_names: std::collections::HashSet<&str> =
            self.counters.iter().map(|c| c.name.as_str()).collect();
        for ev in &self.counter_events {
            if !counter_names.contains(ev.name.as_str()) {
                return Err(CandyError::UnknownKey(
                    "ecnew".to_string(),
                    ev.name.clone(),
                    self.artifacts.name_ref_locs.get(&ev.name).cloned(),
                ));
            }
        }
        // Validate keyframe-counter lifecycle events reference declared kc defs.
        let kc_names: std::collections::HashSet<&str> =
            self.kcdefs.iter().map(|c| c.name.as_str()).collect();
        for ev in &self.kc_events {
            if !kc_names.contains(ev.name.as_str()) {
                return Err(CandyError::UnknownKey(
                    "kcnew".to_string(),
                    ev.name.clone(),
                    self.artifacts.name_ref_locs.get(&ev.name).cloned(),
                ));
            }
        }
        // Validate slide actions reference declared mobjects.
        // SceneSwitch targets a scene by name, not a mobject label, so skip it.
        let mobject_names: std::collections::HashSet<&str> =
            self.items.keys().map(|l| l.0.as_str()).collect();
        for s in self.slides.iter() {
            for action in &s.actions {
                if let Some(target) = action.target() {
                    if !mobject_names.contains(target.0.as_str()) {
                        return Err(CandyError::UnknownKey(
                            "mobject".to_string(),
                            target.0.clone(),
                            self.artifacts.name_ref_locs.get(target.0.as_str()).cloned(),
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    /// Total duration in milliseconds across all slides.
    pub fn total_ms(&self) -> u32 {
        self.slides
            .iter()
            .map(|s| s.start_ms.saturating_add(s.duration_ms))
            .max()
            .unwrap_or(0)
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
#[derive(Default)]
pub enum SubPos {
    /// Default anchor when `position:` is omitted.
    #[default]
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

/// A single keyframe on a keyframe counter: the integer `value` reached at
/// `at_ms` (computed at parse time as `push_cursor + offset`). `easing` is the
/// *resolved* effective easing for the segment that **starts** at this keyframe
/// (this node → next node). `"inherit"` is expanded at parse time into the
/// previous node's effective easing, falling back to the counter-level default
/// from `kcnew` for the first node, so the stored easing is always concrete.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Keyframe {
    pub at_ms: u32,
    pub value: i64,
    #[serde(default)]
    pub easing: Easing,
}

/// A keyframe counter declaration (the "keyframe-counter module", `kc*`).
///
/// Unlike the easing counter (a single `seed + step` ramp defined in `ecnew`),
/// a keyframe counter is driven by discrete keyframes pushed at runtime via
/// `kcpush`. The value at any timeline position interpolates between the two
/// surrounding keyframes (per-segment easing). Lifecycle (`pause` / `resume` /
/// `destroy`) reuses `CounterEvent`; the `kc_events` list keeps keyframe-counter
/// events separate from easing-counter events so a same-named `ecnew` is never
/// affected by a `kcpause`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyframeCounterDef {
    /// Counter name (the key).
    pub name: String,
    /// Lexical Typst scope id (for shadowing).
    pub scope: String,
    /// Integer seed (the value before the first keyframe / when no keyframes).
    pub seed: i64,
    /// Counter-level default easing, used by `inherit` on the first node.
    #[serde(default)]
    pub easing: Easing,
    /// Start time on the timeline (ms).
    pub start_ms: u32,
    /// Keyframes, kept sorted ascending by `at_ms`.
    #[serde(default)]
    pub keyframes: Vec<Keyframe>,
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
/// - the set of mobjects (`owns_labels`) declared inside its body,
/// - an optional human-readable **name** for direct scene switching.
///
/// **Named scenes** can be switched to directly via `#switch(name: "scene_name")`
/// or `#switch(target: "scene_name")`, which jumps the timeline cursor to the
/// target scene's `start_ms`. When a named scene is entered, it auto-hides all
/// sibling/ancestor scenes (same as nested scene semantics).
///
/// **Anonymous scenes** (created by old-style `#scene(...)` without a name)
/// are automatically assigned a UUID-like name for internal management.
///
/// Semantics (see `typst/README.md` → *Scene / canvas*):
/// - scenes are **flat** — a `#scene(...)` may only appear at the document root
///   (never inside another scene); a nested `#scene` is a hard error;
/// - scenes **respect Typst's lexical scope** — a mobject belongs to the
///   innermost scene that encloses it at parse time;
/// - a scene is rendered on **one page**; content that overflows the page emits
///   a `W018` content-overflow warning (the page, not the scene, owns the size
///   and any background fill);
/// - with **no explicit scene**, the whole document is one implicit scene
///   (id `0`).
/// - **Scene switching**: use `#switch(target: "name")` to jump to a named scene.
///   Anonymous scenes get auto-assigned UUID names (e.g., `"scene_a1b2c3d4"`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SceneInfo {
    /// Unique scene id (implicit whole-document scene = `0`).
    pub id: usize,
    /// Human-readable name for scene switching (e.g., `"intro"`, `"demo"`).
    /// `None` means anonymous (will be auto-assigned a UUID-like name internally).
    #[serde(default)]
    pub name: Option<String>,
    /// The lexical Typst scope id this scene occupies (for attribution).
    pub scope: usize,
    /// Canvas size in Typst points `(w, h)`, measured from the document's
    /// `#set page(...)` or the global `candy` show rule. `None` ⇒ inherit from
    /// the document-wide page settings or Typst's default page (A4). The page
    /// itself owns the size; the scene never configures width/height.
    #[serde(default)]
    pub page_size: Option<(f64, f64)>,
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

        match c.duration_ms {
            Some(d) if d > 0 => {
                let progress = (elapsed_f / d as f64).clamp(0.0, 1.0);
                let eased = c.easing.resolve()(progress);
                (c.seed as f64 + c.step as f64 * d as f64 * eased).round() as i64
            }
            _ => c.seed + (c.step as f64 * elapsed_f).round() as i64,
        }
    }

    /// Live value of a keyframe counter named `name` at timeline `time_ms`.
    ///
    /// Mirrors [`Scene::counter_value_at`]: shadowing by scope depth, plus
    /// `destroy` freeze and accumulated `pause`..`resume` intervals. The value
    /// interpolates between the two surrounding keyframes (the segment uses the
    /// *starting* keyframe's resolved easing); before the first keyframe it holds
    /// `seed`, after the last it holds the last keyframe's value.
    pub fn kc_value_at(&self, name: &str, time_ms: u32) -> i64 {
        // Collect candidate keyframe counters named `name` that have started.
        let mut candidates: Vec<&KeyframeCounterDef> = self
            .kcdefs
            .iter()
            .filter(|c| c.name == name && c.start_ms <= time_ms)
            .collect();
        if candidates.is_empty() {
            return self
                .kcdefs
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
        for ev in &self.kc_events {
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
        for ev in &self.kc_events {
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

        let elapsed = elapsed_raw.saturating_sub(paused) as f64;

        if c.keyframes.is_empty() {
            return c.seed;
        }
        let mut kfs = c.keyframes.clone();
        kfs.sort_by_key(|k| k.at_ms);

        // Before the first keyframe: hold seed.
        if elapsed <= kfs[0].at_ms as f64 {
            return c.seed;
        }
        // After the last keyframe: hold the last value.
        if elapsed >= kfs[kfs.len() - 1].at_ms as f64 {
            return kfs[kfs.len() - 1].value;
        }
        // Find the bracketing segment kfs[i] -> kfs[i+1].
        for i in 0..kfs.len() - 1 {
            let a = kfs[i].at_ms as f64;
            let b = kfs[i + 1].at_ms as f64;
            if elapsed >= a && elapsed <= b {
                let denom = (b - a).max(1e-9);
                let progress = ((elapsed - a) / denom).clamp(0.0, 1.0);
                let eased = kfs[i].easing.resolve()(progress);
                return (kfs[i].value as f64 + (kfs[i + 1].value - kfs[i].value) as f64 * eased)
                    .round() as i64;
            }
        }
        c.seed
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
                    start_ms: 0,
                    duration_ms: 10000,
                    actions: vec![Action::MoveTo {
                        target: Label("a".into()),
                        to: (3.0, 0.0),
                        easing: Easing::Linear,
                    }],
                    loc: None,
                },
                Slide {
                    start_ms: 10000,
                    duration_ms: 5000,
                    actions: vec![Action::Scale {
                        target: Label("a".into()),
                        to: 2.0,
                        easing: Easing::Smooth,
                    }],
                    loc: None,
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
            kcdefs: Vec::new(),
            kc_events: Vec::new(),
            scopes: Vec::new(),
            scenes: Vec::new(),
            morph_pairs: Vec::new(),
            transform_plans: Vec::new(),
            groups: HashMap::new(),
            artifacts: ParseArtifacts::default(),
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
