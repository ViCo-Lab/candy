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
//! under `dist/` (only video files ever reach `dist/`).

use std::path::Path;

use clap::{Parser, Subcommand, ValueEnum};
use candy::{build_input_with_gpu, Codec, CandyError, Input, OutputFormat};

#[derive(Parser)]
#[command(
    name = "candy",
    version,
    about = "Candy (.tyx) — Code-oriented Animation Engine Designed for Typst"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Build a `.tyx` X-sheet into an animation.
    Build {
        /// Path to the `.tyx` Typst X-sheet file (or an SVG with a
        /// `candy-json` block when `--from-svg` is given).
        input: PathBufOrStr,
        /// Force the input to be parsed as an SVG rendered by
        /// `@preview/candy` (containing a `candy-json` block). Without this
        /// flag, the parser is selected by file extension: `.svg` → SVG
        /// round-trip via `extract_dsl_from_svg`, anything else → `.tyx`.
        #[arg(long)]
        from_svg: bool,
        /// Output name hint (under `dist/` for videos; ignored for SVG drafts).
        #[arg(short, long, default_value = "out")]
        output: String,
        /// Output container. Default `mp4`. `svg` produces a draft in `.candy/`.
        #[arg(long, value_enum, default_value = "mp4")]
        format: FormatArg,
        /// Video codec. Default `av1` (priority). `h264` optional. `hevc` is not
        /// available in this self-contained build.
        #[arg(long, value_enum, default_value = "av1")]
        codec: CodecArg,
        /// Frames per second (video path).
        #[arg(short, long, default_value_t = 30)]
        fps: u32,
        /// Pixels per Typst point (video path; higher = sharper, slower).
        #[arg(short = 'p', long, default_value_t = 2.0)]
        pixel_per_pt: f32,
        /// Use GPU rasterization (vello + wgpu) for the video path. Requires
        /// candy to be built with `--features gpu`. If the feature is not
        /// enabled or no GPU adapter is available, candy silently falls back
        /// to CPU rasterization (typst-render). Has no effect on `--format svg`.
        #[arg(long, default_value_t = false)]
        gpu: bool,
    },
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
    /// AV1 via rav1e (pure Rust, self-contained). Default.
    Av1,
    /// H.264 via openh264 (self-contained).
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
        Commands::Build {
            input,
            from_svg,
            output,
            format,
            codec,
            fps,
            pixel_per_pt,
            gpu,
        } => {
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

            if out_fmt == OutputFormat::Svg {
                // SVG draft → `.candy/<stem>/`, never `dist/`. GPU flag is
                // irrelevant for SVG drafts (no rasterization).
                build_input_with_gpu(
                    input_kind,
                    &intermediate_dir,
                    &intermediate_dir.join("svg_draft"),
                    out_fmt,
                    codec,
                    fps,
                    pixel_per_pt,
                    false,
                )?;
                println!("draft: .candy/{stem}/frame_*.svg");
                return Ok(());
            }

            let output = resolve_output(&output, &stem, container_ext);
            build_input_with_gpu(
                input_kind,
                &intermediate_dir,
                &output,
                out_fmt,
                codec,
                fps,
                pixel_per_pt,
                gpu,
            )?;
            println!("wrote: {}", output.display());
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
