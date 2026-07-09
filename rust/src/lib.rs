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
//! video file is written to `dist/`. On a successful video build the CLI drops
//! the per-build `.candy/<stem>/` directory automatically (see `--keep-intermediates`).

pub mod core;
pub mod parser;
pub mod renderer;

/// Unified error type (E001–E007); see `core::error::CandyError`.
pub use crate::core::error::CandyError;
pub use crate::renderer::Codec;

use std::collections::HashMap;
use std::path::Path;

use rayon::prelude::*;

use crate::core::ast::{CounterEventKind, Label, Scene};
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
    /// MP4 container (default), H.264 unless `--codec av1` is given.
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
    let mut keyframes = scheduler::schedule(&scene)?; // Step 3

    // Extend the timeline so persistent subtitles / long-lived counters that
    // end *after* the last mobject keyframe are still covered. We append a
    // final keyframe (equal to each target's last state) at the extended end,
    // so mobjects hold steady while subtitles/counters keep animating.
    let mut render_end = scene.total_ms();
    for s in &scene.subtitles {
        if let Some(e) = s.end_ms {
            render_end = render_end.max(e);
        }
    }
    for ev in &scene.counter_events {
        if let CounterEventKind::Destroy = ev.kind {
            render_end = render_end.max(ev.at_ms);
        }
    }
    let max_kf = keyframes.iter().map(|f| f.time_ms).max().unwrap_or(0);
    if render_end > max_kf {
        let mut last: HashMap<Label, crate::core::ast::FrameData> = HashMap::new();
        for f in &keyframes {
            last.insert(f.target.clone(), f.clone());
        }
        for (_tgt, f) in last {
            let mut ext = f.clone();
            ext.time_ms = render_end;
            keyframes.push(ext);
        }
    }

    let frames = interpolator::interpolate_with(keyframes, interpolator::InterpMethod::Linear, fps); // Step 4
    let mut renderer = Renderer::with_root(scene.clone(), project_root)?;

    // Collect the unique sample times (one per video frame), sorted.
    let mut sample_times: Vec<u32> = frames.iter().map(|f| f.time_ms).collect();
    sample_times.sort();
    sample_times.dedup();

    // SVG draft path: write to `.candy/` only (never `dist/`).
    if format == OutputFormat::Svg {
        std::fs::create_dir_all(intermediate_dir)?;
        for (i, &t_ms) in sample_times.iter().enumerate() {
            let svg = renderer.render_frame_at(t_ms, &frames)?; // Step 5
            std::fs::write(
                intermediate_dir.join(format!("frame_{:05}.svg", i)),
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

    // Pre-compute natural layout once (serial) so the parallel rasterization
    // loop can use the &self render_frame_pixels_par method.
    renderer.ensure_natural_public()?;

    // Rasterize every frame in parallel via rayon (data-parallel over frames).
    // Each frame render is independent (the WorldState is shared via Arc and
    // the typst compile is thread-safe). GPU path stays serial (single GPU
    // device).
    #[cfg(feature = "gpu")]
    let gpu_ok = use_gpu;
    #[cfg(not(feature = "gpu"))]
    let _gpu_ok = false;
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

    let probe: Vec<_> = {
        // GPU path is serial (single device); CPU path is parallel (rayon).
        #[cfg(feature = "gpu")]
        if let Some(g) = gpu_renderer.as_mut() {
            let mut out = Vec::with_capacity(sample_times.len());
            for &t_ms in &sample_times {
                out.push(renderer.render_frame_pixels_gpu(t_ms, &frames, pixel_per_pt, g)?);
            }
            out
        } else {
            sample_times
                .par_iter()
                .map(|&t_ms| renderer.render_frame_pixels_par(t_ms, &frames, pixel_per_pt))
                .collect::<Result<_, _>>()?
        }
        #[cfg(not(feature = "gpu"))]
        {
            sample_times
                .par_iter()
                .map(|&t_ms| renderer.render_frame_pixels_par(t_ms, &frames, pixel_per_pt))
                .collect::<Result<_, _>>()?
        }
    };

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
                for (i, &t_ms) in sample_times.iter().enumerate() {
                    let svg = renderer.render_frame_at(t_ms, &frames)?;
                    std::fs::write(
                        intermediate_dir.join(format!("frame_{:05}.svg", i)),
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
                for (i, &t_ms) in sample_times.iter().enumerate() {
                    let svg = renderer.render_frame_at(t_ms, &frames)?;
                    std::fs::write(
                        intermediate_dir.join(format!("frame_{:05}.svg", i)),
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
