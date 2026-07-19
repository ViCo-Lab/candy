//! Candy CLI — render a `.tyx` X-sheet into a self-contained video.
//!
//! ```text
//! candy build examples/dot_move.tyx                 # default: dist/<stem>.mp4 (AV1)
//! candy build examples/dot_move.tyx --format webm   # dist/<stem>.webm (AV1)
//! candy build examples/dot_move.tyx --format mkv --codec h264
//! candy build examples/dot_move.tyx --format gif    # dist/<stem>.gif (animated)
//! candy build examples/dot_move.tyx --format png    # dist/<stem>.png (final frame)
//! candy build examples/dot_move.tyx --format svg    # SVG draft in .candy/
//! candy build a.tyx b.tyx --output out_a.mp4 out_b.mp4   # 1:1 custom names
//! candy build examples/dot_move.tyx --output-dir build/   # redirect all outputs
//! ```
//!
//! Artifacts: intermediates (RGBA/SVG drafts) under `.candy/` (or the chosen
//! `--output-dir`); the final video/GIF/PNG under `dist/` (or `--output-dir`).
//! For video builds, the per-build intermediate directory is removed
//! automatically after a successful run unless `--keep-intermediates` is passed.

#![allow(clippy::result_large_err)]
use std::io::IsTerminal;
use std::path::Path;

use candy::core::ast::{DEFAULT_PAGE_PT, Scene};
use candy::core::diag::CandyWarn;
use candy::{CandyError, Codec, Input, OutputFormat, build_input_with_gpu};
use candy::{error, info, warn};
use clap::{CommandFactory, Parser, Subcommand, ValueEnum};
use colored::Colorize;

#[derive(Parser)]
#[command(
    name = "candy",
    version = concat!(env!("CARGO_PKG_VERSION"), " (", env!("CANDY_CODENAME"), ")"),
    about = "Candy — Code-oriented Animation eNgine Designed for tYpst"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Build a `.tyx` (TYpst X-sheet) into an animation.
    #[command(alias = "render")]
    Build {
        /// Path(s) to one or more `.tyx` Typst X-sheet files (or SVGs with a
        /// `candy-json` block when `--from-svg` is given). Passing several
        /// inputs builds each one in turn, writing a separate output per file.
        /// With no inputs, prints this help.
        #[arg(num_args = 0..)]
        inputs: Vec<PathBufOrStr>,
        /// Force the inputs to be parsed as SVGs rendered by `@preview/candy`
        /// (each containing a `candy-json` block). Without this flag, the
        /// parser is selected by file extension: `.svg` → SVG round-trip via
        /// `extract_scene_from_svg`, anything else → `.tyx`.
        #[arg(long)]
        from_svg: bool,
        /// Output file name(s). These must be given **one per input** (a 1:1
        /// correspondence) and must be plain file names — no path separators,
        /// i.e. no multi-level directories. If the count of names does not
        /// match the number of inputs, or a name contains a path separator,
        /// that name is ignored and the default `dist/<stem>.<ext>` is used
        /// instead (a warning is emitted). Ignored for `--format svg` (the
        /// draft always lands in `.candy/<stem>/`).
        #[arg(short, long, num_args = 0..)]
        output: Vec<String>,
        /// Redirect **every** output file (including custom `--output` names)
        /// into this single directory. Only one `--output-dir` may be given;
        /// giving more than one is an error. When omitted, video/GIF/PNG
        /// outputs go to `dist/`.
        #[arg(long)]
        output_dir: Option<String>,
        /// Output container / target. Default `mp4`. `svg` produces a draft in
        /// `.candy/`; `gif` an animated GIF; `png` a static bitmap of the final
        /// frame.
        #[arg(long, value_enum, default_value = "mp4")]
        format: FormatArg,
        /// Video codec. Default `x264` (via system ffmpeg + libx264). Falls back
        /// to openh264 (`h264`) when ffmpeg is unavailable. `av1` (rav1e) is the
        /// alternative; `hevc`/`x265`/hardware codecs also shell out to system
        /// ffmpeg when available. Ignored for `--format gif` / `--format png`.
        #[arg(long, value_enum, default_value = "x264")]
        codec: CodecArg,
        /// Frames per second (video / GIF path).
        #[arg(short, long, default_value_t = 30)]
        fps: u32,
        /// Pixels per Typst point (video path; higher = sharper, slower).
        /// Ignored when `--width` / `--height` is given (those derive the
        /// effective pixels-per-point from the scene's page size).
        #[arg(short = 'p', long, default_value_t = 2.0)]
        pixel_per_pt: f32,
        /// Output width in **pixels**. When set, the effective pixels-per-point
        /// is derived from the scene's page width, so `--width 1920` pins the
        /// output to 1920 px wide (the height follows the page's aspect ratio).
        /// Mutually exclusive in spirit with `--pixel-per-pt`; `--width` wins
        /// when both are given.
        #[arg(long)]
        width: Option<u32>,
        /// Output height in **pixels**. Like `--width` but pins the height; the
        /// width follows the page's aspect ratio.
        #[arg(long)]
        height: Option<u32>,
        /// Use GPU rasterization (vello + wgpu) for the video path. Requires
        /// candy to be built with `--features gpu`. If the feature is not
        /// enabled or no GPU adapter is available, candy silently falls back
        /// to CPU rasterization (typst-render). Has no effect on `--format svg`.
        #[arg(long, default_value_t = false)]
        gpu: bool,
        /// Keep intermediate files (`.candy/<stem>/`, e.g. `frames.rgba` and
        /// any draft `frame_*.svg`) after a successful build. By default candy
        /// removes that per-build intermediate directory automatically once the
        /// final video is written. Has no effect on `--format svg` (whose
        /// output *is* the `.candy/` draft).
        #[arg(long, default_value_t = false)]
        keep_intermediates: bool,
        /// Parallel rasterization jobs (render thread pool size). Caps how many
        /// frames are rasterized in parallel and — via the bounded streaming
        /// channel — how many frames' RGBA may be live in memory at once. This
        /// is the resource-limit knob for the streaming pipeline: memory peak is
        /// bounded by `jobs` in-flight frames regardless of total frame count.
        /// Defaults to the number of logical CPUs. Pass `1` for a fully serial,
        /// minimal-memory build.
        #[arg(long, default_value_t = 0)]
        jobs: usize,
    },
    /// Hidden easter-egg command. Invoked as `candy candy` or `candy tyx`.
    #[command(alias = "tyx", hide = true)]
    Candy,
}

/// Accept either a string or a path; we only need the string form from CLI.
#[derive(Clone)]
struct PathBufOrStr(std::path::PathBuf);
impl std::str::FromStr for PathBufOrStr {
    type Err = std::convert::Infallible;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(PathBufOrStr(std::path::PathBuf::from(s)))
    }
}

#[derive(Clone, Copy, ValueEnum)]
enum FormatArg {
    Mp4,
    Mkv,
    Webm,
    Gif,
    Png,
    Svg,
}

#[derive(Clone, Copy, ValueEnum)]
enum CodecArg {
    /// AV1 via rav1e (pure Rust, self-contained).
    Av1,
    /// H.264 via openh264 (self-contained). Default.
    H264,
    /// H.265/HEVC. Uses system ffmpeg + x265 if available; E007 otherwise.
    H265,
    /// H.264 via system ffmpeg + libx264 (higher quality than openh264).
    X264,
    /// H.265 via system ffmpeg + libx265.
    X265,
    /// H.264 via VAAPI (Linux Intel/AMD GPU hardware encoder).
    #[cfg(target_os = "linux")]
    #[value(name = "h264-vaapi")]
    H264Vaapi,
    /// H.265 via VAAPI.
    #[cfg(target_os = "linux")]
    #[value(name = "h265-vaapi")]
    H265Vaapi,
    /// H.264 via VideoToolbox (macOS hardware encoder).
    #[cfg(target_os = "macos")]
    #[value(name = "h264-videotoolbox")]
    H264VideoToolbox,
    /// H.265 via VideoToolbox.
    #[cfg(target_os = "macos")]
    #[value(name = "h265-videotoolbox")]
    H265VideoToolbox,
    /// H.264 via Intel Quick Sync Video (QSV).
    #[cfg(target_os = "windows")]
    #[value(name = "h264-qsv")]
    H264Qsv,
    /// H.265 via Intel QSV.
    #[cfg(target_os = "windows")]
    #[value(name = "h265-qsv")]
    H265Qsv,
    /// AV1 via VAAPI (Linux hardware encoder).
    #[cfg(target_os = "linux")]
    #[value(name = "av1-vaapi")]
    Av1Vaapi,
    /// VP9 via libvpx (system ffmpeg).
    #[value(name = "vp9")]
    Vp9,
    /// VP8 via libvpx (system ffmpeg).
    #[value(name = "vp8")]
    Vp8,
}

fn main() {
    if let Err(e) = run() {
        // Fatal error: surface through the unified diagnostic reporter, which
        // prints to stderr and terminates with exit code 64..70 (E001 -> 64).
        error!(&e);
    }
}

fn run() -> Result<(), CandyError> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Candy => {
            // Hidden easter egg: `candy candy` / `candy tyx`.
            const SECRET: &str = "Built for Candy(TYX). In memory of CChO2025.";
            if std::io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none() {
                eprintln!("{}", SECRET.bold());
            } else {
                eprintln!("{SECRET}");
            }
        }
        Commands::Build {
            inputs,
            from_svg,
            output,
            format,
            codec,
            fps,
            pixel_per_pt,
            width,
            height,
            gpu,
            keep_intermediates,
            output_dir,
            jobs,
        } => {
            // No inputs: print the build subcommand's help and exit cleanly
            // (the user asked for help on an empty `candy build`).
            if inputs.is_empty() {
                let mut cmd = Cli::command();
                if let Some(build) = cmd.find_subcommand_mut("build") {
                    let _ = build.print_help();
                } else {
                    let _ = cmd.print_help();
                }
                println!();
                return Ok(());
            }
            // Custom `--output` names must correspond 1:1 with the inputs. If
            // the counts disagree, ignore every custom name and warn once.
            let names_match = output.len() == inputs.len();
            if !names_match && !output.is_empty() {
                warn!(CandyWarn::OutputNameCountMismatch(format!(
                    "{} --output name(s) given for {} input(s)",
                    output.len(),
                    inputs.len()
                )));
            }
            // Build each input in turn, writing a separate output per file.
            //
            // Batch mode is **non-fatal per input**: a failure on one input does
            // NOT abort the others — every input is attempted so partial
            // progress is preserved (outputs already written are kept). Failures
            // are collected and, once all inputs have been tried, surfaced
            // together. When more than one input was given, the process exits
            // with [`BATCH_ERROR_EXIT`] (111) if *any* input failed; for a single
            // input the specific `E00x` code is preserved.
            let mut failures: Vec<(std::path::PathBuf, CandyError)> = Vec::new();
            for (i, input) in inputs.iter().enumerate() {
                let input_path = input.0.clone();
                // Run one input; `?` inside collects into `result` instead of
                // aborting the whole batch.
                let result: Result<(), CandyError> = (|| {
                    let input = &input.0;
                    let stem = input
                        .file_stem()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_else(|| "animation".into());
                    // Intermediate dir: under `--output-dir` when given (so the
                    // draft is also redirected), otherwise the usual `.candy/<stem>`.
                    let intermediate_dir = match &output_dir {
                        Some(d) => Path::new(d).join(&stem),
                        None => Path::new(".candy").join(&stem),
                    };
                    std::fs::create_dir_all(&intermediate_dir)?;

                    let (out_fmt, container_ext) = match format {
                        FormatArg::Mp4 => (OutputFormat::Mp4, "mp4"),
                        FormatArg::Mkv => (OutputFormat::Mkv, "mkv"),
                        FormatArg::Webm => (OutputFormat::Webm, "webm"),
                        FormatArg::Gif => (OutputFormat::Gif, "gif"),
                        FormatArg::Png => (OutputFormat::Png, "png"),
                        FormatArg::Svg => (OutputFormat::Svg, "svg"),
                    };
                    let codec = match codec {
                        CodecArg::Av1 => Codec::Av1,
                        CodecArg::H264 => Codec::H264,
                        CodecArg::H265 => Codec::H265,
                        CodecArg::X264 => Codec::X264,
                        CodecArg::X265 => Codec::X265,
                        #[cfg(target_os = "linux")]
                        CodecArg::H264Vaapi => Codec::H264Vaapi,
                        #[cfg(target_os = "linux")]
                        CodecArg::H265Vaapi => Codec::H265Vaapi,
                        #[cfg(target_os = "macos")]
                        CodecArg::H264VideoToolbox => Codec::H264VideoToolbox,
                        #[cfg(target_os = "macos")]
                        CodecArg::H265VideoToolbox => Codec::H265VideoToolbox,
                        #[cfg(target_os = "windows")]
                        CodecArg::H264Qsv => Codec::H264Qsv,
                        #[cfg(target_os = "windows")]
                        CodecArg::H265Qsv => Codec::H265Qsv,
                        #[cfg(target_os = "linux")]
                        CodecArg::Av1Vaapi => Codec::Av1Vaapi,
                        CodecArg::Vp9 => Codec::Vp9,
                        CodecArg::Vp8 => Codec::Vp8,
                    };

                    let input_kind = if from_svg {
                        Input::Svg(input.to_path_buf())
                    } else {
                        Input::from(input.as_path())
                    };

                    // Derive the effective pixels-per-point. `--width` / `--height`
                    // pin one output edge in pixels (the other edge follows the
                    // scene's page aspect ratio); otherwise `--pixel-per-pt` is
                    // used as-is. The page size is read from the parsed scene.
                    let page_pt = input_kind
                        .parse()
                        .ok()
                        .map(|s| root_page_pt(&s))
                        .unwrap_or(DEFAULT_PAGE_PT);
                    let ppt = resolve_pixel_per_pt(pixel_per_pt, width, height, page_pt);

                    if out_fmt == OutputFormat::Svg {
                        // SVG draft → `intermediate_dir` (`.candy/<stem>` or the
                        // redirected `--output-dir/<stem>`), never `dist/`. GPU flag
                        // is irrelevant for SVG drafts (no rasterization). The draft
                        // IS the deliverable here, so we never auto-clean it.
                        build_input_with_gpu(
                            input_kind,
                            &intermediate_dir,
                            &intermediate_dir.join("svg_draft"),
                            out_fmt,
                            codec,
                            fps,
                            ppt,
                            false,
                            jobs,
                            keep_intermediates,
                        )?;
                        info!("draft: {}/frame_*.svg", intermediate_dir.display());
                        return Ok(());
                    }

                    // Resolve the custom name for this input (1:1 with inputs, and
                    // only if it is a plain file name — no path separators).
                    let custom = if names_match {
                        output.get(i).map(|s| s.as_str())
                    } else {
                        None
                    };
                    let out_path =
                        resolve_output(custom, &stem, container_ext, output_dir.as_deref());
                    build_input_with_gpu(
                        input_kind,
                        &intermediate_dir,
                        &out_path,
                        out_fmt,
                        codec,
                        fps,
                        ppt,
                        gpu,
                        jobs,
                        keep_intermediates,
                    )?;
                    // Successful build: drop the per-build intermediate dir unless
                    // the user asked to keep it (the SVG draft `return`s above, so
                    // it is never cleaned here).
                    if !keep_intermediates {
                        cleanup_intermediate(&intermediate_dir);
                    }
                    info!("built: {}", out_path.display());
                    Ok(())
                })();
                if let Err(e) = result {
                    failures.push((input_path, e));
                }
            }
            // Surface any collected batch failures. In batch mode (more than one
            // input) a midway error forces the exit code to `BATCH_ERROR_EXIT`
            // (111) so callers can detect partial failure; for a single input we
            // keep the specific `E00x` code.
            if !failures.is_empty() {
                if inputs.len() > 1 {
                    // Batch mode: list every input that failed. This runs only
                    // *after* all inputs have been attempted (batch mode never
                    // aborts early), so the complete failure set is known here.
                    eprintln!(
                        "{}",
                        format!("Batch failed on {} input(s):", failures.len())
                            .red()
                            .bold()
                    );
                    for (path, e) in &failures {
                        eprintln!(
                            "  - {}: {} {}",
                            path.display(),
                            candy::core::diag::code_error(e.code()),
                            e.message()
                        );
                    }
                    // Batch partial failure: surface through the unified
                    // diagnostic pipeline as `EYEE` (exit code 111, which
                    // deliberately bypasses the `64`-based rule). `111` ≈
                    // "yī yī yī" → "yee~": the strangled little noise you make
                    // after biting into something spoiled.
                    error!(CandyError::Yee("yee~ Batch failed. \\(!_!)/".to_string()));
                } else {
                    // Single input (non-batch): keep the specific `E00x` code
                    // via the diagnostic pipeline — no "Batch failed" summary.
                    error!(failures.into_iter().next().unwrap().1);
                }
            }
        }
    }
    Ok(())
}

/// Resolve the final output path.
///
/// `output_name` is the user's custom name for this input (already validated to
/// be a 1:1 match and a plain file name by the caller). When it is `None` or
/// contains a path separator, the default `dist/<stem>.<ext>` (or
/// `<output_dir>/<stem>.<ext>` when `--output-dir` is given) is used instead.
fn resolve_output(
    output_name: Option<&str>,
    stem: &str,
    ext: &str,
    output_dir: Option<&str>,
) -> std::path::PathBuf {
    let default_name = format!("{stem}.{ext}");
    let name = match output_name {
        Some(n) if is_plain_filename(n) => n.to_string(),
        Some(n) => {
            // A custom name with a path separator (multi-level directory) is
            // rejected — fall back to the default and warn.
            warn!(CandyWarn::OutputNameInvalid(n.to_string()));
            default_name.clone()
        }
        None => default_name.clone(),
    };
    let dir = output_dir.unwrap_or("dist");
    Path::new(dir).join(name)
}

/// A plain output file name: non-empty and containing no path separators
/// (`/` or `\\`), and not `.` / `..`. Multi-level directory paths are rejected
/// so outputs never escape the chosen output directory.
fn is_plain_filename(name: &str) -> bool {
    !name.is_empty() && !name.contains('/') && !name.contains('\\') && name != "." && name != ".."
}

/// The canvas size (Typst points) of a scene's root for resolution purposes.
fn root_page_pt(scene: &Scene) -> (f64, f64) {
    if scene.scenes.is_empty() {
        scene.page_size.unwrap_or(DEFAULT_PAGE_PT)
    } else {
        scene.effective_page_pt(scene.root_scene.unwrap_or(0))
    }
}

/// Resolve the effective pixels-per-point.
///
/// - `--width` pins the output *width* in pixels → `ppt = width / page_w_pt`.
/// - `--height` pins the output *height* in pixels → `ppt = height / page_h_pt`.
/// - Otherwise `--pixel-per-pt` is used unchanged.
///
/// Specifying one edge's pixel count (the other follows the page's aspect
/// ratio) is exactly the requested "specify the pixel count of a certain edge"
/// behavior.
fn resolve_pixel_per_pt(
    pixel_per_pt: f32,
    width: Option<u32>,
    height: Option<u32>,
    page_pt: (f64, f64),
) -> f32 {
    if let Some(w) = width {
        ((w as f64) / page_pt.0).clamp(0.01, 1000.0) as f32
    } else if let Some(h) = height {
        ((h as f64) / page_pt.1).clamp(0.01, 1000.0) as f32
    } else {
        pixel_per_pt
    }
}

/// Best-effort removal of a per-build intermediate directory (`.candy/<stem>`).
///
/// Called after a successful video build (unless `--keep-intermediates` is
/// given). Errors are non-fatal: we only `warn` and move on, so a file held
/// open by another process won't abort the run. If removing the directory
/// leaves the parent `.candy/` empty, that parent is pruned too to keep the
/// tree tidy.
fn cleanup_intermediate(dir: &Path) {
    if !dir.exists() {
        return;
    }
    if let Err(e) = std::fs::remove_dir_all(dir) {
        warn!(CandyWarn::CleanupFailed(format!("{}: {e}", dir.display())));
        return;
    }
    if let Some(parent) = dir.parent() {
        let is_candy = parent
            .file_name()
            .map(|n| n == "candy" || n == ".candy")
            .unwrap_or(false);
        if is_candy {
            let _ = std::fs::remove_dir(parent);
        }
    }
}
