//! Candy — **C**ode-oriented **A**nimation **N**gine **D**esigned for t**Y**pst.
//!
//! Layered, fully self-contained pipeline:
//!
//! ```text
//! .tyx ─▶ parser::parse_tyx ─▶ Scene (AST, valid standard Typst)
//!                         │
//!                         ▼
//!        core::scheduler::schedule ─▶ keyframes (Vec<FrameData>)
//!                         │
//!                         ▼
//!      core::interpolator::interpolate ─▶ all frames (Vec<FrameData>)
//!                         │
//!                         ▼
//!   renderer::typst::Renderer ─▶ pixel frames
//!                         │
//!                         ▼
//!   renderer::video ─▶ AV1 (rav1e) / H.264 (openh264) ─▶ MP4 / Matroska
//! ```
//!
//! No external tools are ever invoked: the Typst compilation, the video
//! encoding, and the container muxing all run in-process. Build artifacts:
//! intermediates (RGBA drafts, SVG drafts) live under `.candy/`; only the final
//! video file is written to `dist/`.

pub mod core;
pub mod parser;
pub mod renderer;

/// Unified error type (E001–E007); see `core::error::CandyError`.
pub use crate::core::error::CandyError;
pub use crate::renderer::Codec;

use std::path::Path;

use crate::core::ast::Scene;
use crate::core::interpolator;
use crate::core::scheduler;
use crate::parser::extract_dsl_from_svg;
use crate::parser::parse_tyx;
use crate::renderer::video::{self, Container, EncodedVideo};
use crate::renderer::Renderer;

/// Input source for the `build` pipeline.
///
/// Candy v0.1 only accepted a `.tyx` path. The `@preview/candy` Typst package
/// also supports rendering an SVG with an embedded `candy-json` block, which
/// `extract_dsl_from_svg` recovers. Exposing both paths from `build()` makes
/// the SVG round-trip (Typst → SVG → candy) actually reachable, instead of
/// leaving `extract_dsl_from_svg` as dead exported code.
#[derive(Debug, Clone)]
pub enum Input {
    /// A `.tyx` Typst X-sheet (parsed via `parser::parse_tyx`).
    Tyx(std::path::PathBuf),
    /// An SVG rendered by `@preview/candy`, containing a `candy-json` block
    /// (parsed via `parser::extract_dsl_from_svg`).
    Svg(std::path::PathBuf),
}

impl Input {
    /// Parse the input into a [`Scene`] AST.
    pub fn parse(&self) -> Result<Scene, CandyError> {
        match self {
            Input::Tyx(p) => parse_tyx(p),
            Input::Svg(p) => extract_dsl_from_svg(p),
        }
    }

    /// The project root for Typst file resolution: the parent directory of
    /// the source file. Used to wire `Renderer::with_root` so local
    /// `#import "file.typ"` calls resolve relative to the source.
    pub fn project_root(&self) -> std::path::PathBuf {
        match self {
            Input::Tyx(p) | Input::Svg(p) => p
                .parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_default(),
        }
    }
}

impl From<&std::path::Path> for Input {
    fn from(p: &std::path::Path) -> Self {
        match p.extension().and_then(|e| e.to_str()) {
            Some("svg") => Input::Svg(p.to_path_buf()),
            _ => Input::Tyx(p.to_path_buf()),
        }
    }
}

/// Output target for the `build` pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    /// SVG draft written to `.candy/` (not a video, never enters `dist/`).
    Svg,
    /// MP4 container (default), AV1 unless `--codec h264` is given.
    Mp4,
    /// Matroska (`.mkv`).
    Mkv,
    /// WebM (Matroska with `webm` doctype).
    Webm,
}

/// End-to-end build: `.tyx` → `Scene` → keyframes → frames → output.
///
/// * `input`            — path to the `.tyx` X-sheet (valid standard Typst).
/// * `intermediate_dir` — directory (`.candy/<stem>`) for draft artifacts.
/// * `output`           — final video path (under `dist`) for video formats.
/// * `format`           — [`OutputFormat`].
/// * `codec`            — [`Codec`] (AV1 preferred; H264 optional; HEVC errors).
/// * `fps`              — frames per second (video time base).
/// * `pixel_per_pt`     — rasterization resolution for the video path.
///
/// Backward-compatible wrapper around [`build_input`]: dispatches on the
/// file extension (`.svg` → SVG round-trip via `extract_dsl_from_svg`;
/// anything else → `.tyx` parser).
pub fn build(
    input: &Path,
    intermediate_dir: &Path,
    output: &Path,
    format: OutputFormat,
    codec: Codec,
    fps: u32,
    pixel_per_pt: f32,
) -> Result<(), CandyError> {
    build_input(Input::from(input), intermediate_dir, output, format, codec, fps, pixel_per_pt)
}

/// Like [`build`], but takes an explicit [`Input`] so callers can force the
/// SVG path even when the file extension is not `.svg` (e.g. an SVG produced
/// by `@preview/candy` and saved with a `.txt` extension).
pub fn build_input(
    input: Input,
    intermediate_dir: &Path,
    output: &Path,
    format: OutputFormat,
    codec: Codec,
    fps: u32,
    pixel_per_pt: f32,
) -> Result<(), CandyError> {
    let scene: Scene = input.parse()?; // Steps 1–2
    let project_root = input.project_root();
    let keyframes = scheduler::schedule(&scene)?; // Step 3
    let frames = interpolator::interpolate(keyframes); // Step 4
    let mut renderer = Renderer::with_root(scene.clone(), project_root)?;

    let total = frames.iter().map(|f| f.frame_idx).max().unwrap_or(0);

    // SVG draft path: write to `.candy/` only (never `dist/`).
    if format == OutputFormat::Svg {
        std::fs::create_dir_all(intermediate_dir)?;
        for f in 0..=total {
            let svg = renderer.render_frame_at(f, &frames)?; // Step 5
            std::fs::write(
                intermediate_dir.join(format!("frame_{:05}.svg", f)),
                svg,
            )?;
        }
        return Ok(());
    }

    let container = match format {
        OutputFormat::Mp4 => Container::Mp4,
        OutputFormat::Mkv => Container::Mkv,
        OutputFormat::Webm => Container::Webm,
        OutputFormat::Svg => unreachable!(),
    };

    // Step 5: rasterize every frame.
    let probe: Vec<_> = (0..=total)
        .map(|f| renderer.render_frame_pixels(f, &frames, pixel_per_pt))
        .collect::<Result<_, _>>()?;

    // Draft: persist the RGBA frames under `.candy/` for inspection.
    std::fs::create_dir_all(intermediate_dir)?;
    video::write_rgba_draft(&probe, intermediate_dir, "frames")?;

    // Step 6: encode + mux. On failure, emit an SVG draft and surface E007.
    let video: EncodedVideo = match video::encode_frames(&probe, fps, codec) {
        Ok(v) => v,
        Err(e) => {
            eprintln!(
                "warn: [{}] video encode failed, wrote SVG draft to .candy: {e}",
                e.code()
            );
            for f in 0..=total {
                let svg = renderer.render_frame_at(f, &frames)?;
                std::fs::write(
                    intermediate_dir.join(format!("frame_{:05}.svg", f)),
                    svg,
                )?;
            }
            return Err(e);
        }
    };
    let audio = video::collect_audio(&scene.audio, fps);
    let bytes = video::mux(&video, audio.as_ref(), container)?;

    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(output, bytes)?;
    Ok(())
}
