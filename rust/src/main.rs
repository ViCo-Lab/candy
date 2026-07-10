//! Candy CLI — render a `.tyx` X-sheet into a self-contained video.
//!
//! ```text
//! candy build examples/dot_move.tyx                 # default: dist/<stem>.mp4 (AV1)
//! candy build examples/dot_move.tyx --format webm   # dist/<stem>.webm (AV1)
//! candy build examples/dot_move.tyx --format mkv --codec h264
//! candy build examples/dot_move.tyx --format svg    # SVG draft in .candy/
//! ```
//!
//! Artifacts: intermediates (RGBA/SVG drafts) under `.candy/`; the final video
//! under `dist/` (only video files ever reach `dist/`). For video builds, the
//! per-build `.candy/<stem>/` directory is removed automatically after a
//! successful run unless `--keep-intermediates` is passed.

use std::path::Path;

use candy::core::ast::{DEFAULT_PAGE_PT, Scene};
use candy::{CandyError, Codec, Input, OutputFormat, build_input_with_gpu};
use clap::{Parser, Subcommand, ValueEnum};

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
        #[arg(num_args = 1..)]
        inputs: Vec<PathBufOrStr>,
        /// Force the inputs to be parsed as SVGs rendered by `@preview/candy`
        /// (each containing a `candy-json` block). Without this flag, the
        /// parser is selected by file extension: `.svg` → SVG round-trip via
        /// `extract_dsl_from_svg`, anything else → `.tyx`.
        #[arg(long)]
        from_svg: bool,
        /// Output name hint (under `dist/` for videos; ignored for SVG drafts).
        /// When building multiple files, this is a shared hint; a per-file
        /// default of `dist/<stem>.<ext>` is used unless a real file name is
        /// given.
        #[arg(short, long, default_value = "out")]
        output: String,
        /// Output container. Default `mp4`. `svg` produces a draft in `.candy/`.
        #[arg(long, value_enum, default_value = "mp4")]
        format: FormatArg,
        /// Video codec. Default `h264` (self-contained, via openh264). `av1`
        /// (rav1e) is the fallback/alternative; `hevc`/`x264`/`x265`/hardware
        /// codecs shell out to system ffmpeg when available.
        #[arg(long, value_enum, default_value = "h264")]
        codec: CodecArg,
        /// Frames per second (video path).
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
    #[value(name = "h264-vaapi")]
    H264Vaapi,
    /// H.265 via VAAPI.
    #[value(name = "h265-vaapi")]
    H265Vaapi,
    /// H.264 via VideoToolbox (macOS hardware encoder).
    #[value(name = "h264-videotoolbox")]
    H264VideoToolbox,
    /// H.265 via VideoToolbox.
    #[value(name = "h265-videotoolbox")]
    H265VideoToolbox,
    /// H.264 via Intel Quick Sync Video (QSV).
    #[value(name = "h264-qsv")]
    H264Qsv,
    /// H.265 via Intel QSV.
    #[value(name = "h265-qsv")]
    H265Qsv,
}

fn main() -> Result<(), CandyError> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Candy => {
            // Hidden easter egg: `candy candy` / `candy tyx`.
            println!("Built for Candy(TYX). In memory of CChO2025.");
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
        } => {
            // Build each input in turn, writing a separate output per file.
            // A failure on one file fails the whole batch (fail-fast); outputs
            // already written are kept.
            for input in &inputs {
                let input = &input.0;
                let stem = input
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "animation".into());
                let intermediate_dir = Path::new(".candy").join(&stem);
                std::fs::create_dir_all(&intermediate_dir)?;

                let (out_fmt, container_ext) = match format {
                    FormatArg::Mp4 => (OutputFormat::Mp4, "mp4"),
                    FormatArg::Mkv => (OutputFormat::Mkv, "mkv"),
                    FormatArg::Webm => (OutputFormat::Webm, "webm"),
                    FormatArg::Svg => (OutputFormat::Svg, "svg"),
                };
                let codec = match codec {
                    CodecArg::Av1 => Codec::Av1,
                    CodecArg::H264 => Codec::H264,
                    CodecArg::H265 => Codec::H265,
                    CodecArg::X264 => Codec::X264,
                    CodecArg::X265 => Codec::X265,
                    CodecArg::H264Vaapi => Codec::H264Vaapi,
                    CodecArg::H265Vaapi => Codec::H265Vaapi,
                    CodecArg::H264VideoToolbox => Codec::H264VideoToolbox,
                    CodecArg::H265VideoToolbox => Codec::H265VideoToolbox,
                    CodecArg::H264Qsv => Codec::H264Qsv,
                    CodecArg::H265Qsv => Codec::H265Qsv,
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
                    // SVG draft → `.candy/<stem>/`, never `dist/`. GPU flag is
                    // irrelevant for SVG drafts (no rasterization). The draft IS
                    // the deliverable here, so we never auto-clean it.
                    build_input_with_gpu(
                        input_kind,
                        &intermediate_dir,
                        &intermediate_dir.join("svg_draft"),
                        out_fmt,
                        codec,
                        fps,
                        ppt,
                        false,
                    )?;
                    println!("draft: .candy/{stem}/frame_*.svg");
                    continue;
                }

                let output = resolve_output(&output, &stem, container_ext);
                build_input_with_gpu(
                    input_kind,
                    &intermediate_dir,
                    &output,
                    out_fmt,
                    codec,
                    fps,
                    ppt,
                    gpu,
                )?;
                // Successful video build: drop the per-build intermediate dir
                // (`.candy/<stem>`) unless the user asked to keep it.
                if !keep_intermediates {
                    cleanup_intermediate(&intermediate_dir);
                }
                println!("built: {}", output.display());
            }
        }
    }
    Ok(())
}

/// Resolve the final video path under `dist/`.
fn resolve_output(output: &str, stem: &str, ext: &str) -> std::path::PathBuf {
    let p = std::path::Path::new(output);
    match p.file_name() {
        Some(name) if name != std::ffi::OsStr::new("out") => {
            Path::new("dist").join(name.to_string_lossy().into_owned())
        }
        _ => Path::new("dist").join(format!("{stem}.{ext}")),
    }
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
        eprintln!(
            "warn: could not remove intermediate dir {}: {e}",
            dir.display()
        );
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
