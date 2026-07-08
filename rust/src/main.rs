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
use candy::{build, Codec, CandyError, OutputFormat};

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
        /// Path to the `.tyx` Typst X-sheet file.
        input: PathBufOrStr,
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
    Av1,
    H264,
    H265,
}

fn main() -> Result<(), CandyError> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Build {
            input,
            output,
            format,
            codec,
            fps,
            pixel_per_pt,
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
            };

            if out_fmt == OutputFormat::Svg {
                // SVG draft → `.candy/<stem>/`, never `dist/`.
                build(
                    input,
                    &intermediate_dir,
                    &intermediate_dir.join("svg_draft"),
                    out_fmt,
                    codec,
                    fps,
                    pixel_per_pt,
                )?;
                println!("draft: .candy/{stem}/frame_*.svg");
                return Ok(());
            }

            let output = resolve_output(&output, &stem, container_ext);
            build(
                input,
                &intermediate_dir,
                &output,
                out_fmt,
                codec,
                fps,
                pixel_per_pt,
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
