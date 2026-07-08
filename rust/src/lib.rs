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
    build_input_with_gpu(input, intermediate_dir, output, format, codec, fps, pixel_per_pt, false)
}

/// Like [`build_input`], but with an explicit `use_gpu` flag.
///
/// When `use_gpu` is true and the `gpu` cargo feature is enabled, candy
/// rasterizes each frame on the GPU via vello + wgpu. If the `gpu` feature is
/// not compiled in, `use_gpu` is silently ignored (CPU path is used). If the
/// feature is enabled but no GPU adapter is available, candy falls back to
/// the CPU path automatically and emits a warning.
pub fn build_input_with_gpu(
    input: Input,
    intermediate_dir: &Path,
    output: &Path,
    format: OutputFormat,
    codec: Codec,
    fps: u32,
    pixel_per_pt: f32,
    use_gpu: bool,
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

    // Step 5: rasterize every frame. Try GPU first if requested.
    #[cfg(feature = "gpu")]
    let gpu_ok = use_gpu;
    #[cfg(not(feature = "gpu"))]
    let gpu_ok = false;
    #[cfg(feature = "gpu")]
    let mut gpu_renderer: Option<crate::renderer::gpu::GpuRenderer> = None;
    #[cfg(feature = "gpu")]
    if gpu_ok {
        match crate::renderer::gpu::GpuRenderer::new() {
            Ok(g) => {
                eprintln!("info: GPU rasterization enabled (vello + wgpu)");
                gpu_renderer = Some(g);
            }
            Err(e) => {
                eprintln!("warn: GPU unavailable, falling back to CPU: {e}");
            }
        }
    } else if use_gpu {
        eprintln!("warn: --gpu requested but candy was built without the 'gpu' feature; using CPU");
    }
    #[cfg(not(feature = "gpu"))]
    if use_gpu {
        eprintln!("warn: --gpu requested but candy was built without the 'gpu' feature; using CPU");
    }
    // Suppress unused-variable warning when gpu feature is off.
    #[allow(unused_variables)]
    let gpu_ok_unused = gpu_ok;

    let probe: Vec<_> = (0..=total)
        .map(|f| {
            #[cfg(feature = "gpu")]
            if let Some(g) = gpu_renderer.as_mut() {
                return renderer.render_frame_pixels_gpu(f, &frames, pixel_per_pt, g);
            }
            renderer.render_frame_pixels(f, &frames, pixel_per_pt)
        })
        .collect::<Result<_, _>>()?;

    // Draft: persist the RGBA frames under `.candy/` for inspection.
    std::fs::create_dir_all(intermediate_dir)?;
    video::write_rgba_draft(&probe, intermediate_dir, "frames")?;

    // Step 6: encode + mux.
    //
    // FFmpeg codecs (X264, X265, VAAPI, VideoToolbox, QSV) shell out to
    // system ffmpeg and return already-muxed bytes — they bypass candy's
    // hand-written muxer. Self-contained codecs (AV1, H264) go through
    // candy's rav1e/openh264 + container muxer. H265 tries ffmpeg, falls
    // back to E007.
    let bytes: Vec<u8> = if codec.uses_ffmpeg() || (codec == Codec::H265 && crate::renderer::ffmpeg::find_ffmpeg().is_some()) {
        // FFmpeg path: compose frames to uniform size, then pipe to ffmpeg.
        let max_w = probe.iter().map(|f| f.width).max().unwrap_or(16);
        let max_h = probe.iter().map(|f| f.height).max().unwrap_or(16);
        let tw = max_w.max(16).next_multiple_of(2);
        let th = max_h.max(16).next_multiple_of(2);
        let composed: Vec<_> = probe.iter().map(|f| compose_uniform(f, tw, th)).collect();
        match crate::renderer::ffmpeg::encode_via_ffmpeg(&composed, fps, codec, container) {
            Ok(bytes) => bytes,
            Err(e) => {
                eprintln!(
                    "warn: [{}] ffmpeg encode failed, wrote SVG draft to .candy: {e}",
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
        }
    } else {
        // Self-contained path: rav1e/openh264 + candy's muxer.
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
        video::mux(&video, audio.as_ref(), container)?
    };

    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(output, bytes)?;
    Ok(())
}

/// Compose a frame onto a uniform `tw × th` opaque-white canvas (copies source
/// pixels to top-left). Used by the ffmpeg path to give ffmpeg a uniform frame
/// size (its rawvideo input doesn't support per-frame dimensions).
fn compose_uniform(frame: &crate::renderer::RenderedFrame, tw: usize, th: usize) -> crate::renderer::RenderedFrame {
    let mut rgba = vec![255u8; tw * th * 4];
    for y in 0..frame.height.min(th) {
        let src = y * frame.width * 4;
        let dst = y * tw * 4;
        rgba[dst..dst + frame.width * 4].copy_from_slice(&frame.rgba[src..src + frame.width * 4]);
    }
    crate::renderer::RenderedFrame {
        width: tw,
        height: th,
        rgba,
    }
}
