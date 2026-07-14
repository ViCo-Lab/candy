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
//!                         └▶ GIF (animated) / PNG (bitmap, final frame)
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

/// Unified error type (E001–E007 → exit code 64–70; `EYEE` → exit code 111, a
/// batch partial-failure marker that deliberately bypasses the `64` rule) and
/// non-fatal warning type (W001–W011); see `core::diag::{CandyError, CandyWarn}`
/// and the `core::diag::{error, warn, debug, info}` reporters.
pub use crate::core::diag::{CandyError, CandyWarn};
pub use crate::renderer::Codec;

use std::collections::HashMap;
use std::path::Path;

use rayon::prelude::*;

use crate::core::ast::{CounterEventKind, Label, Scene};
use crate::core::interpolator;
use crate::core::scheduler;
use crate::parser::extract_scene_from_svg;
use crate::parser::parse_tyx;
use crate::renderer::Renderer;
use crate::renderer::encode::{self, Container, EncodedVideo};

/// Input source for the `build` pipeline.
///
/// Candy v0.1 only accepted a `.tyx` path. The `@preview/candy` Typst package
/// also supports rendering an SVG with an embedded `candy-json` block, which
/// `extract_scene_from_svg` recovers. Exposing both paths from `build()` makes
/// the SVG round-trip (Typst → SVG → candy) actually reachable, instead of
/// leaving `extract_scene_from_svg` as dead exported code.
#[derive(Debug, Clone)]
pub enum Input {
    /// A `.tyx` Typst X-sheet (parsed via `parser::parse_tyx`).
    Tyx(std::path::PathBuf),
    /// An SVG rendered by `@preview/candy`, containing a `candy-json` block
    /// (parsed via `parser::extract_scene_from_svg`).
    Svg(std::path::PathBuf),
}

impl Input {
    /// Parse the input into a [`Scene`] AST.
    pub fn parse(&self) -> Result<Scene, CandyError> {
        match self {
            Input::Tyx(p) => parse_tyx(p),
            Input::Svg(p) => extract_scene_from_svg(p),
        }
    }

    /// The project root for Typst file resolution: the parent directory of
    /// the source file. Used to wire `Renderer::with_root` so local
    /// `#import "file.typ"` calls resolve relative to the source.
    pub fn project_root(&self) -> std::path::PathBuf {
        match self {
            Input::Tyx(p) | Input::Svg(p) => {
                p.parent().map(|p| p.to_path_buf()).unwrap_or_default()
            }
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
    /// Animated GIF (all frames, looping). Self-contained — no ffmpeg.
    Gif,
    /// Static PNG bitmap of the final frame (the animation "poster").
    Png,
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
/// file extension (`.svg` → SVG round-trip via `extract_scene_from_svg`;
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
    build_input(
        Input::from(input),
        intermediate_dir,
        output,
        format,
        codec,
        fps,
        pixel_per_pt,
    )
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
    build_input_with_gpu(
        input,
        intermediate_dir,
        output,
        format,
        codec,
        fps,
        pixel_per_pt,
        false,
    )
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
            std::fs::write(intermediate_dir.join(format!("frame_{:016}.svg", i)), svg)?;
        }
        return Ok(());
    }

    // Nothing to render: a degenerate input (no `#candy` content / no animatable
    // objects — e.g. a file whose only content is an unknown top-level Typst
    // call like `#invalid()`) parses into an empty scene, which yields zero
    // sample times. Surface this as a clean error instead of letting the encoder
    // index into an empty frame buffer and panic (index out of bounds).
    if sample_times.is_empty() {
        return Err(CandyError::Encode(
            "no frames to render: the input produced an empty scene \
             (no #candy content or no animatable objects were found)"
                .into(),
        ));
    }

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
    let mut gpu_renderer: Option<crate::renderer::raster::gpu::GpuRenderer> = None;
    #[cfg(feature = "gpu")]
    if gpu_ok {
        match crate::renderer::raster::gpu::GpuRenderer::new() {
            Ok(g) => {
                info!("GPU rasterization enabled (vello + wgpu)");
                gpu_renderer = Some(g);
            }
            Err(e) => {
                warn!(CandyWarn::GpuUnavailable(e.to_string()));
            }
        }
    } else if use_gpu {
        warn!(CandyWarn::GpuFeatureDisabled);
    }
    #[cfg(not(feature = "gpu"))]
    if use_gpu {
        warn!(CandyWarn::GpuFeatureDisabled);
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

    // Draft: persist the RGBA frames under `intermediate_dir` for inspection.
    std::fs::create_dir_all(intermediate_dir)?;
    encode::write_rgba_draft(&probe, intermediate_dir, "frames")?;

    // Ensure the output's parent directory exists (e.g. `dist/` or a custom
    // `--output-dir`). Done once here for every rasterized target (video,
    // GIF, PNG).
    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // Step 6: encode + mux / write, branching on the output container.
    match format {
        OutputFormat::Svg => unreachable!(),
        // Animated GIF: all frames, looping forever. Self-contained.
        OutputFormat::Gif => {
            encode::write_gif(&probe, fps, output, &scene.private_metadata)?;
        }
        // Static PNG bitmap of the final frame (the animation "poster").
        OutputFormat::Png => {
            let last = probe
                .last()
                .ok_or_else(|| CandyError::Encode("no frames to write as PNG".into()))?;
            encode::write_png(last, output, &scene.private_metadata)?;
        }
        // Video containers (MP4 / MKV / WebM): encode + mux.
        OutputFormat::Mp4 | OutputFormat::Mkv | OutputFormat::Webm => {
            let container = match format {
                OutputFormat::Mp4 => Container::Mp4,
                OutputFormat::Mkv => Container::Mkv,
                OutputFormat::Webm => Container::Webm,
                _ => unreachable!(),
            };
            // FFmpeg codecs (X264, X265, VAAPI, VideoToolbox, QSV) shell out to
            // system ffmpeg and return already-muxed bytes — they bypass candy's
            // hand-written muxer. Self-contained codecs (AV1, H264) go through
            // candy's rav1e/openh264 + container muxer. H265 tries ffmpeg, falls
            // back to E007.
            let bytes: Vec<u8> = if codec.uses_ffmpeg()
                || (codec == Codec::H265
                    && crate::renderer::encode::ffmpeg::find_ffmpeg().is_some())
            {
                // FFmpeg path: compose frames to uniform size, then pipe to ffmpeg.
                let max_w = probe.iter().map(|f| f.width).max().unwrap_or(16);
                let max_h = probe.iter().map(|f| f.height).max().unwrap_or(16);
                let tw = max_w.max(16).next_multiple_of(2);
                let th = max_h.max(16).next_multiple_of(2);
                let composed: Vec<_> =
                    probe.iter().map(|f| compose_uniform(f, tw, th)).collect();
                match crate::renderer::encode::ffmpeg::encode_via_ffmpeg(
                    &composed, fps, codec, container, &scene.private_metadata,
                ) {
                    Ok(bytes) => bytes,
                    Err(e) => {
                        warn!(CandyWarn::EncodeFallback(format!(
                            "ffmpeg encode failed: {e}"
                        )));
                        for (i, &t_ms) in sample_times.iter().enumerate() {
                            let svg = renderer.render_frame_at(t_ms, &frames)?;
                            std::fs::write(
                                intermediate_dir.join(format!("frame_{:016}.svg", i)),
                                svg,
                            )?;
                        }
                        return Err(e);
                    }
                }
            } else {
                // Self-contained path: rav1e/openh264 + candy's muxer.
                let video: EncodedVideo = match encode::encode_frames(&probe, fps, codec, &scene.private_metadata) {
                    Ok(v) => v,
                    Err(e) => {
                        warn!(CandyWarn::EncodeFallback(format!(
                            "video encode failed: {e}"
                        )));
                        for (i, &t_ms) in sample_times.iter().enumerate() {
                            let svg = renderer.render_frame_at(t_ms, &frames)?;
                            std::fs::write(
                                intermediate_dir.join(format!("frame_{:016}.svg", i)),
                                svg,
                            )?;
                        }
                        return Err(e);
                    }
                };
                let audio = encode::collect_audio(&scene.audio, fps);
                encode::mux(&video, audio.as_ref(), container, &scene.private_metadata)?
            };

            std::fs::write(output, bytes)?;
        }
    }
    Ok(())
}

/// Compose a frame onto a uniform `tw × th` opaque-white canvas (copies source
/// pixels to top-left). Used by the ffmpeg path to give ffmpeg a uniform frame
/// size (its rawvideo input doesn't support per-frame dimensions).
fn compose_uniform(
    frame: &crate::renderer::RenderedFrame,
    tw: usize,
    th: usize,
) -> crate::renderer::RenderedFrame {
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

/// Resolve the path to the `@preview/candy` Typst package manifest
/// (`typst/typst.toml`) relative to this crate's manifest directory.
///
/// The Rust backend and the Typst package live side by side under the repo
/// root (`rust/` and `typst/`), so the manifest is always
/// `<crate_root>/../typst/typst.toml`.
#[cfg(test)]
fn typst_package_manifest() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../typst/typst.toml")
}

/// Read the `version` field of a `typst.toml` (or any TOML) file.
///
/// This is how test code auto-fetches the published `@preview/candy` version
/// instead of hard-coding it in assertions (project convention: only test code
/// needs the Typst package version auto-fetched).
#[cfg(test)]
fn read_typst_toml_version(path: &Path) -> Result<String, CandyError> {
    let text = std::fs::read_to_string(path)?; // E001 on missing file
    for line in text.lines() {
        let line = line.trim_start();
        if let Some(rest) = line.strip_prefix("version") {
            // Match the key itself, not a longer identifier like `versions`.
            match rest.chars().next() {
                Some('=') | Some(' ') | Some('\t') => {}
                _ => continue,
            }
            if let Some(eq) = rest.find('=') {
                let val = rest[eq + 1..].trim().trim_matches('"');
                if !val.is_empty() {
                    return Ok(val.to_string());
                }
            }
        }
    }
    Err(CandyError::Io(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        "typst.toml: missing `version` field",
    )))
}

/// Auto-fetch the published version of the `@preview/candy` Typst package from
/// `typst/typst.toml`. Test-only helper (see the project convention above).
#[cfg(test)]
pub(crate) fn typst_package_version() -> Result<String, CandyError> {
    read_typst_toml_version(&typst_package_manifest())
}

#[cfg(test)]
mod version_tests {
    use super::*;

    #[test]
    fn typst_package_version_is_fetched_from_manifest() {
        // Auto-fetch proof: the version is read from the package manifest,
        // never hard-coded in the assertion.
        let v = typst_package_version().expect("typst/typst.toml must exist");
        assert!(!v.is_empty(), "version must not be empty");
        // Must look like plain semver: digits and dots only, with a dot.
        assert!(
            v.chars().all(|c| c.is_ascii_digit() || c == '.'),
            "version `{v}` is not plain semver"
        );
        assert!(v.contains('.'), "version `{v}` should contain a dot");
    }

    #[test]
    fn read_typst_toml_version_parses_known_value() {
        let tmp = std::env::temp_dir().join("candy_test_typst_version.toml");
        std::fs::write(&tmp, "[package]\nname = \"candy\"\nversion = \"9.8.7\"\n").unwrap();
        let got = read_typst_toml_version(&tmp).expect("temp toml must parse");
        assert_eq!(got, "9.8.7");
        std::fs::remove_file(&tmp).ok();
    }

    #[test]
    fn read_typst_toml_version_handles_missing_file() {
        let err = read_typst_toml_version(Path::new("/nonexistent/candy/typst.toml"))
            .expect_err("missing file must error");
        assert_eq!(err.code(), "E001");
    }

    #[test]
    fn read_typst_toml_version_handles_missing_key() {
        let tmp = std::env::temp_dir().join("candy_test_typst_noversion.toml");
        std::fs::write(&tmp, "[package]\nname = \"candy\"\n").unwrap();
        let err = read_typst_toml_version(&tmp).expect_err("missing version must error");
        // InvalidData surfaces as E001 (Io), the right bucket for this helper.
        assert_eq!(err.code(), "E001");
        std::fs::remove_file(&tmp).ok();
    }
}
