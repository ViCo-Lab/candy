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
use crate::parser::parse_tyx;
use crate::renderer::video::{self, Container, EncodedVideo};
use crate::renderer::Renderer;

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
pub fn build(
    input: &Path,
    intermediate_dir: &Path,
    output: &Path,
    format: OutputFormat,
    codec: Codec,
    fps: u32,
    pixel_per_pt: f32,
) -> Result<(), CandyError> {
    let scene: Scene = parse_tyx(input)?; // Steps 1–2
    let keyframes = scheduler::schedule(&scene); // Step 3
    let frames = interpolator::interpolate(keyframes); // Step 4
    let mut renderer = Renderer::new(scene.clone())?;

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
