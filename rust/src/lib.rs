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
use std::io::Write;
use std::path::{Path, PathBuf};

use rayon::prelude::*;

use crate::core::ast::{CounterEventKind, FrameData, Label, Scene};
use crate::core::interpolator;
use crate::core::meta::PrivateMeta;
use crate::core::scheduler;
use crate::parser::extract_scene_from_svg;
use crate::parser::parse_tyx;
use crate::renderer::RenderedFrame;
use crate::renderer::audio::AudioData;
use crate::renderer::Renderer;
use crate::renderer::encode::{self, Container};

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
    jobs: usize,
    keep_intermediates: bool,
) -> Result<(), CandyError> {
    build_input(
        Input::from(input),
        intermediate_dir,
        output,
        format,
        codec,
        fps,
        pixel_per_pt,
        jobs,
        keep_intermediates,
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
    jobs: usize,
    keep_intermediates: bool,
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
        jobs,
        keep_intermediates,
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
    jobs: usize,
    keep_intermediates: bool,
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

    // Uniform canvas size every frame is composited onto (largest scene page ×
    // ppi). Known up front (from scene metadata, not from already-rendered
    // frames) so the streaming encoder can size its output without buffering
    // every frame first.
    let (tw, th) = renderer.uniform_canvas(pixel_per_pt);
    let meta = scene.private_metadata.clone();
    let audio = encode::collect_audio(&scene.audio, fps);

    // Ensure the output's parent directory exists (e.g. `dist/` or a custom
    // `--output-dir`).
    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // GPU path (feature-gated, serial): render one frame at a time on the GPU
    // and push it straight into the streaming encoder, so at most one frame's
    // RGBA is ever live. The CPU path below does the same with bounded
    // parallelism.
    #[cfg(feature = "gpu")]
    if let Some(g) = gpu_renderer.as_mut() {
        stream_encode_gpu(
            &mut renderer, &frames, &sample_times, pixel_per_pt, fps, codec,
            container_for(format), is_gif(format), &meta, tw, th,
            audio, keep_intermediates, intermediate_dir, output, g,
        )?;
        return Ok(());
    }

    // CPU path: bounded-parallel render → bounded channel → streaming encoder.
    // Frames are encoded and dropped one at a time, so peak memory is bounded by
    // `jobs` in-flight frames plus the (small) coded stream — never all N frames
    // at once. This is the core OOM fix: the old code collected every frame's
    // RGBA into `probe` before encoding.
    stream_encode_cpu(
        &renderer, &frames, &sample_times, pixel_per_pt, fps, codec,
        container_for(format), is_gif(format), meta, tw, th,
        audio, jobs, keep_intermediates, intermediate_dir, output,
    )?;
    Ok(())
}

/// Map an [`OutputFormat`] to its container (video targets only).
fn container_for(f: OutputFormat) -> Container {
    match f {
        OutputFormat::Mp4 => Container::Mp4,
        OutputFormat::Mkv => Container::Mkv,
        OutputFormat::Webm => Container::Webm,
        _ => Container::Mp4,
    }
}

/// Whether `f` is the animated-GIF target (vs. a video container).
fn is_gif(f: OutputFormat) -> bool {
    matches!(f, OutputFormat::Gif)
}

/// A frame encoder that streams: frames are pushed one at a time and the output
/// is written/finalized at [`finish`](StreamEncoder::finish). Unifies the GIF
/// and video (self-contained + ffmpeg) paths so the caller never has to hold
/// more than one frame's RGBA at once.
enum StreamEncoder {
    Gif(encode::video::GifStream),
    Video(encode::video::StreamingVideo),
}

impl StreamEncoder {
    fn new(
        is_gif: bool,
        fps: u32,
        codec: Codec,
        container: Container,
        meta: &PrivateMeta,
        tw: usize,
        th: usize,
        audio: Option<AudioData>,
        output: &Path,
    ) -> Result<Self, CandyError> {
        if is_gif {
            Ok(StreamEncoder::Gif(encode::video::GifStream::new(
                output, fps, meta, tw, th,
            )?))
        } else {
            Ok(StreamEncoder::Video(encode::video::StreamingVideo::new(
                fps, codec, container, meta, tw, th, audio,
            )?))
        }
    }

    fn push(&mut self, f: &RenderedFrame) -> Result<(), CandyError> {
        match self {
            StreamEncoder::Gif(g) => g.push(f),
            StreamEncoder::Video(v) => v.push(f),
        }
    }

    fn finish(self, output: &Path) -> Result<(), CandyError> {
        match self {
            StreamEncoder::Gif(g) => g.finish(),
            StreamEncoder::Video(v) => {
                // Streams the coded samples straight from their temp file into
                // `output`, so the whole container is never buffered in RAM.
                v.finish(output)
            }
        }
    }
}

/// Consumer side of the streaming pipeline: pulls frames from `rx`, writes the
/// optional RGBA draft incrementally, and pushes each frame into the encoder
/// (which writes it out / drops its RGBA immediately). Runs on its own thread so
/// the producer (parallel renderer) is never blocked except by the bounded
/// channel's back-pressure.
fn consume_frames(
    rx: std::sync::mpsc::Receiver<Result<RenderedFrame, CandyError>>,
    is_gif: bool,
    fps: u32,
    codec: Codec,
    container: Container,
    meta: PrivateMeta,
    tw: usize,
    th: usize,
    audio: Option<AudioData>,
    keep: bool,
    intermediate_dir: PathBuf,
    output: PathBuf,
    frame_count: usize,
) -> Result<(), CandyError> {
    let mut enc =
        StreamEncoder::new(is_gif, fps, codec, container, &meta, tw, th, audio, &output)?;
    let mut draft = if keep {
        std::fs::create_dir_all(&intermediate_dir)?;
        let mut f = std::fs::File::create(intermediate_dir.join("frames.rgba"))?;
        // Streaming draft format: [u32 count][u32 tw][u32 th] then per frame
        // [u32 w][u32 h][rgba...]. Self-describing so it needs no other metadata.
        f.write_all(&(frame_count as u32).to_le_bytes())?;
        f.write_all(&(tw as u32).to_le_bytes())?;
        f.write_all(&(th as u32).to_le_bytes())?;
        Some(f)
    } else {
        None
    };
    let mut first_err: Option<CandyError> = None;
    for item in rx {
        match item {
            Ok(f) => {
                if first_err.is_none() {
                    if let Some(d) = draft.as_mut() {
                        d.write_all(&(f.width as u32).to_le_bytes())?;
                        d.write_all(&(f.height as u32).to_le_bytes())?;
                        d.write_all(&f.rgba)?;
                    }
                    if let Err(e) = enc.push(&f) {
                        first_err = Some(e);
                    }
                }
            }
            Err(e) => {
                if first_err.is_none() {
                    first_err = Some(e);
                }
            }
        }
    }
    if let Some(e) = first_err {
        return Err(e);
    }
    enc.finish(&output)
}

/// CPU streaming pipeline: render frames with bounded parallelism (at most
/// `jobs` in flight, back-pressured by a bounded channel) and stream them into
/// the encoder on a dedicated consumer thread. Peak memory ≈ `jobs` frames'
/// RGBA + the small coded stream — independent of the total frame count `N`.
fn stream_encode_cpu(
    renderer: &Renderer,
    frames: &[FrameData],
    sample_times: &[u32],
    pixel_per_pt: f32,
    fps: u32,
    codec: Codec,
    container: Container,
    is_gif: bool,
    meta: PrivateMeta,
    tw: usize,
    th: usize,
    audio: Option<AudioData>,
    jobs: usize,
    keep: bool,
    intermediate_dir: &Path,
    output: &Path,
) -> Result<(), CandyError> {
    // Bounded channel: at most `jobs` frames may be buffered between producer
    // and consumer, so in-flight RGBA is capped regardless of `N`.
    let cap = jobs.max(1);
    let (tx, rx) = std::sync::mpsc::sync_channel::<Result<RenderedFrame, CandyError>>(cap);
    // Hoist owned/Copy values out of the thread closure so it captures *no*
    // references to this function's locals (the closure must be `'static` for
    // `std::thread::spawn`). `meta` is owned and moved in; the others are
    // `Copy`/`PathBuf` so they're safe to move across the thread boundary.
    let idir = intermediate_dir.to_path_buf();
    let opath = output.to_path_buf();
    let frame_count = sample_times.len();
    let enc_handle = std::thread::Builder::new()
        .name("candy-encoder".into())
        .spawn(move || {
            consume_frames(
                rx, is_gif, fps, codec, container, meta, tw, th,
                audio, keep, idir, opath, frame_count,
            )
        })
        .map_err(|e| CandyError::Encode(format!("spawn encoder thread: {e}")))?;

    // Producers: data-parallel over frames, each sending its rendered RGBA into
    // the bounded channel. `tx.send` blocks when the channel is full, which is
    // exactly the back-pressure that bounds memory.
    let render = |t: u32| renderer.render_frame_pixels_par(t, frames, pixel_per_pt);
    if jobs > 0 {
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(jobs)
            .build()
            .map_err(|e| CandyError::Encode(format!("rayon pool init: {e}")))?;
        pool.install(|| {
            sample_times
                .par_iter()
                .map(|&t| {
                    let _ = tx.send(render(t));
                })
                .count();
        });
    } else {
        sample_times
            .par_iter()
            .map(|&t| {
                let _ = tx.send(render(t));
            })
            .count();
    }
    drop(tx); // close the channel so the consumer thread terminates
    enc_handle
        .join()
        .map_err(|e| CandyError::Encode(format!("encoder thread panicked: {:?}", e)))??;
    Ok(())
}

/// GPU streaming pipeline (feature-gated, serial): render one frame at a time
/// on the GPU and stream it into the encoder immediately. Memory stays bounded
/// to a single frame's RGBA since there is no parallelism to buffer.
#[cfg(feature = "gpu")]
fn stream_encode_gpu(
    renderer: &mut Renderer,
    frames: &[FrameData],
    sample_times: &[u32],
    pixel_per_pt: f32,
    fps: u32,
    codec: Codec,
    container: Container,
    is_gif: bool,
    meta: &PrivateMeta,
    tw: usize,
    th: usize,
    audio: Option<AudioData>,
    keep: bool,
    intermediate_dir: &Path,
    output: &Path,
    gpu: &mut crate::renderer::raster::gpu::GpuRenderer,
) -> Result<(), CandyError> {
    let mut enc =
        StreamEncoder::new(is_gif, fps, codec, container, meta, tw, th, audio, output)?;
    let mut draft = if keep {
        std::fs::create_dir_all(intermediate_dir)?;
        let mut f = std::fs::File::create(intermediate_dir.join("frames.rgba"))?;
        f.write_all(&(sample_times.len() as u32).to_le_bytes())?;
        f.write_all(&(tw as u32).to_le_bytes())?;
        f.write_all(&(th as u32).to_le_bytes())?;
        Some(f)
    } else {
        None
    };
    for &t in sample_times {
        let f = renderer.render_frame_pixels_gpu(t, frames, pixel_per_pt, gpu)?;
        if let Some(d) = draft.as_mut() {
            d.write_all(&(f.width as u32).to_le_bytes())?;
            d.write_all(&(f.height as u32).to_le_bytes())?;
            d.write_all(&f.rgba)?;
        }
        enc.push(&f)?;
    }
    enc.finish(output)
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
