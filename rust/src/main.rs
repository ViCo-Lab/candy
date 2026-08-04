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
//! candy build examples/dot_move.tyx --output ../out/my_clip.mp4  # single-file precise path
//! candy build examples/dot_move.tyx --output ./out/my_clip.mp4  # `.`/`..` resolved by the OS
//! candy build -r projects/ --output-dir build/  # recurse, mirror tree, skip hidden dirs
//! candy build a.tyx -r dir1 -r dir2  # explicit files + recursive dirs (combined)
//! ```
//!
//! Artifacts: intermediates (RGBA/SVG drafts) under `.candy/` (or the chosen
//! `--output-dir`); the final video/GIF/PNG under `dist/` (or `--output-dir`).
//! For video builds, the per-build intermediate directory is removed
//! automatically after a successful run unless `--keep-intermediates` is passed.

#![allow(clippy::result_large_err)]
use std::io::ErrorKind;
use std::path::Path;
use std::time::Instant;

use candy::core::ast::{DEFAULT_PAGE_PT, Scene};
use candy::core::diag::{CandyWarn, bold, cargo_finished, cargo_status, report_error};
use candy::{
    CandyError, Codec, Input, OutputFormat, build_scene_with_gpu, check_input, migrate_file,
};
use candy::{blue, dim, eprint_styled, error, green, print_styled, red, warn, yellow};
use clap::{CommandFactory, Parser, Subcommand, ValueEnum};

/// Full version string baked in at compile time: `v<version>@<git-hash>(<codename>)`.
/// Shared by clap's `--version` flag (plain, no color) and the `version`
/// subcommand, which additionally prints a colored, multi-line build
/// provenance report: enabled features, target triple, fine-grained ISA level,
/// and a `Built at <time> on <host>` line.
const VERSION: &str = concat!(
    "v",
    env!("CARGO_PKG_VERSION"),
    "@",
    env!("CANDY_GIT_HASH"),
    "(",
    env!("CANDY_CODENAME"),
    ")"
);

#[derive(Parser)]
#[command(
    name = "candy",
    version = VERSION,
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
        /// i.e. no multi-level directories — except in a **single-file,
        /// non-batch** build: there, `--output` may instead be a precise path
        /// (it contains `/` or `\`, or is the platform-independent link `.` /
        /// `..`), in which case the output is written exactly to that path (its
        /// parent directory is created if needed, and any `.` / `..` hops are
        /// resolved by the OS) instead of `dist/`, and `--output-dir` is
        /// ignored. For every other case, if the count of names does not match
        /// the number of inputs, or a batch name contains a path separator, that
        /// name is ignored and the default `dist/<stem>.<ext>` is used instead (a
        /// warning is emitted). Ignored for `--format svg` (the draft always
        /// lands in `.candy/<stem>/`).
        #[arg(short, long, num_args = 0..)]
        output: Vec<String>,
        /// Redirect **every** output file (including custom `--output` names)
        /// into this single directory. Only one `--output-dir` may be given;
        /// giving more than one is an error. When omitted, video/GIF/PNG
        /// outputs go to `dist/`.
        #[arg(long)]
        output_dir: Option<String>,
        /// Recursively render every `.tyx` under the given directory(ies). Each
        /// `-r` argument **must be a directory** (passing a file is an error; on
        /// multiple directories, the `.tyx` files found in all of them are
        /// collected). The source tree's structure is mirrored under
        /// `--output-dir` — a file at `<root>/a/b.tyx` is written to
        /// `<output-dir>/<root-name>/a/b.<ext>` (a root-level `<root>/root.tyx`
        /// goes to `<output-dir>/<root-name>/root.<ext>`) — while directly-passed
        /// plain `dist/<stem>.<ext>` (or `--output-dir/<stem>.<ext>`) placement.
        /// Hidden directories (`.git`, `.candy`, …) are skipped to avoid stray
        /// recursion. May be combined with explicit `<inputs>` and/or repeated
        /// (`-r dir1 -r dir2`). Each `-r` takes exactly one directory, so it can
        /// be placed anywhere on the command line — before or after `<inputs>` —
        /// without greedily swallowing the following positional file arguments.
        #[arg(short = 'r', long = "recursive", num_args = 1)]
        recursive: Vec<PathBufOrStr>,
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
        /// Skip the candy import version check. By default candy verifies that
        /// the `.tyx`'s `@preview/candy:<version>` import matches the installed
        /// candy CLI version (CandyDumpedYou on mismatch). Pass this flag to
        /// bypass the check — useful during development when the package
        /// version has been bumped but the `.tyx` has not been updated yet.
        #[arg(long, default_value_t = false)]
        ignore_version: bool,
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
    /// Force-migrate an old `.tyx` to the current candy version (or a version
    /// given via `--version`), rewriting only the candy import line in place.
    #[command(alias = "upgrade")]
    Migrate {
        /// Path(s) to one or more `.tyx` files whose candy import version line
        /// should be rewritten.
        #[arg(num_args = 1..)]
        inputs: Vec<PathBufOrStr>,
        /// Target candy version to write into the import line. Defaults to the
        /// candy CLI's own version (compiled in from `CARGO_PKG_VERSION`), so a
        /// plain `candy migrate a.tyx` brings `a.tyx` up to the installed CLI.
        #[arg(long)]
        version: Option<String>,
    },
    /// Simulate a render without producing any artifact. Candy compiles and
    /// composes every frame's SVG (Typst → SVG) but never rasterizes to a
    /// bitmap or encodes — a fast way to catch compile/compose errors. The
    /// candy import version check still runs by default.
    #[command(alias = "simulate")]
    Check {
        /// Path(s) to one or more `.tyx` Typst X-sheet files to check.
        #[arg(num_args = 0..)]
        inputs: Vec<PathBufOrStr>,
        /// Frames per second (controls how many frames are composed during the
        /// simulation). Default `30`.
        #[arg(short, long, default_value_t = 30)]
        fps: u32,
        /// Skip the candy import version check. By default `check` verifies that
        /// the `.tyx`'s `@preview/candy:<version>` import matches the installed
        /// candy CLI version (CandyDumpedYou on mismatch).
        #[arg(long, default_value_t = false)]
        ignore_version: bool,
    },
    /// Generate a shell completion script for the candy CLI and print it to
    /// stdout. Redirect it into your shell's completion directory, e.g.
    /// `candy completions zsh > ~/.zsh/completions/_candy` or
    /// `candy completions bash > /etc/bash_completion.d/candy`.
    Completions {
        /// Target shell (bash, elvish, fish, powershell, zsh).
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },
    /// Print candy's version (equivalent to `--version`).
    Version,
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
    /// H.265/HEVC. Uses system ffmpeg + x265 if available; E009 otherwise.
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
    // Easter-egg commands (`candy candy` / `candy tyx`) are handled dynamically
    // here rather than as a clap subcommand, so they are never part of the
    // static command tree that `candy completions` scans and therefore never
    // leak into generated shell completion scripts.
    let args: Vec<String> = std::env::args().collect();
    if args.len() >= 2 && (args[1] == "candy" || args[1] == "tyx") {
        const SECRET: &str = "Built for Candy(TYX). In memory of CChO2025.";
        eprint_styled!("{}", bold!("{}", SECRET));
        return Ok(());
    }
    let cli = Cli::parse();
    match cli.command {
        Commands::Version => {
            // Colored, multi-line build provenance report. ANSI is stripped when
            // piped / NO_COLOR by `print_styled!`. Layout:
            //   candy v<ver>@<hash>(<codename>)          ← header (as before)
            //   Features:  <comma-list>                 ← yellow (bold label)
            //   Target:   <rust target triple>          ← blue   (bold label)
            //   ISA:      <fine-grained level>          ← blue   (bold label)
            //   Built at <ISO 8601, second> on <host>   ← yellow time, blue host
            // Labels are bold + left-aligned so the colons line up; the colon
            // itself stays in the regular font.
            let hash = env!("CANDY_GIT_HASH");
            let colored_hash = if hash == "unknown" {
                dim!("{}", hash)
            } else if hash.ends_with('*') {
                red!("{}", hash)
            } else {
                blue!("{}", hash)
            };
            print_styled!(
                "{} {}{}{}{}{}{}",
                bold!("candy"),
                green!("v{}", env!("CARGO_PKG_VERSION")),
                "@",
                colored_hash,
                "(",
                yellow!("{}", env!("CANDY_CODENAME")),
                ")"
            );

            // Enabled cargo features, collected dynamically at build time and
            // baked into `CANDY_FEATURES` (see build.rs) — no hardcoding here.
            // Labels (bold) are left-aligned to a fixed width so the colons line
            // up; the colon itself stays in the regular (default) font.
            print_styled!(
                "{}:  {}",
                bold!("{:<10}", "Features"),
                yellow!("{}", env!("CANDY_FEATURES"))
            );
            print_styled!(
                "{}:  {}",
                bold!("{:<10}", "Target"),
                blue!("{}", env!("CANDY_TARGET_TRIPLE"))
            );
            print_styled!(
                "{}:  {}",
                bold!("{:<10}", "ISA"),
                blue!("{}", env!("CANDY_ISA_LEVEL"))
            );
            // `Built at <time> on <host>` — single line, multi-color:
            // connectors in the default font, time in yellow, host in blue.
            print_styled!(
                "Built at {} on {}",
                yellow!("{}", env!("CANDY_BUILD_TIME")),
                blue!("{}", env!("CANDY_BUILD_HOST"))
            );
        }
        Commands::Completions { shell } => {
            // Delegate entirely to clap_complete: it walks the full `Cli`
            // command tree (subcommands, flags, value enums) and emits the
            // native completion script for the chosen shell on stdout.
            let mut cmd = Cli::command();
            let bin_name = cmd.get_name().to_string();
            clap_complete::generate(shell, &mut cmd, bin_name, &mut std::io::stdout());
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
            ignore_version,
            output_dir,
            recursive,
            jobs,
        } => {
            // Expand the explicit `<inputs>` plus every `--recursive` directory
            // into one flat list of `.tyx` render units. A `--recursive` argument
            // that is not a directory surfaces as an I/O error (E001). With no
            // files and no directories, print the build subcommand's help and
            // exit cleanly (the user asked for help on an empty `candy build`).
            let render_inputs = collect_render_inputs(&inputs, &recursive)?;
            if render_inputs.is_empty() {
                let mut cmd = Cli::command();
                if let Some(build) = cmd.find_subcommand_mut("build") {
                    let _ = build.print_help();
                } else {
                    let _ = cmd.print_help();
                }
                println!();
                return Ok(());
            }
            // Single-file (non-batch) builds may pass a precise output path (one
            // containing a separator) via `--output`; in that case the file is
            // written exactly to that path instead of `dist/`. Batch builds keep
            // the stricter "plain file name only" rule, so a path separator there
            // is rejected.
            let single_file = render_inputs.len() == 1;
            // Custom `--output` names must correspond 1:1 with the inputs. If
            // the counts disagree, ignore every custom name and warn once.
            let names_match = output.len() == render_inputs.len();
            if !names_match && !output.is_empty() {
                warn!(CandyWarn::OutputNameCountMismatch(format!(
                    "{} --output name(s) given for {} input(s)",
                    output.len(),
                    render_inputs.len()
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
            let mut succeeded = 0usize;
            let build_start = Instant::now();
            for (i, ri) in render_inputs.iter().enumerate() {
                let input_path = ri.path.clone();
                cargo_status!("Building", "{}", input_path.display());
                // Run one input; `?` inside collects into `result` instead of
                // aborting the whole batch.
                let result: Result<(), CandyError> = (|| {
                    let input = &ri.path;
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

                    // Parse ONCE up front. The page size (for `--width`/`--height`
                    // resolution) and the actual build both need the parsed
                    // `Scene`, so we reuse this single parse — a second full parse
                    // would re-run the AST walk and emit every parse warning twice.
                    let project_root = input_kind.project_root();
                    let scene = input_kind.parse_with_ignore_version(ignore_version)?;
                    let page_pt = root_page_pt(&scene);
                    let ppt = resolve_pixel_per_pt(pixel_per_pt, width, height, page_pt);

                    if out_fmt == OutputFormat::Svg {
                        // SVG draft → `intermediate_dir` (`.candy/<stem>` or the
                        // redirected `--output-dir/<stem>`), never `dist/`. GPU flag
                        // is irrelevant for SVG drafts (no rasterization). The draft
                        // IS the deliverable here, so we never auto-clean it.
                        build_scene_with_gpu(
                            scene,
                            project_root,
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
                        cargo_status!(
                            "Finished",
                            "SVG draft at {}/frame_*.svg",
                            intermediate_dir.display()
                        );
                        return Ok(());
                    }

                    // Resolve the custom name for this input (1:1 with inputs, and
                    // only if it is a plain file name — no path separators).
                    let custom = if names_match {
                        output.get(i).map(|s| s.as_str())
                    } else {
                        None
                    };
                    let out_path = resolve_output(
                        custom,
                        &stem,
                        container_ext,
                        output_dir.as_deref(),
                        single_file,
                        ri.rel.as_deref(),
                    );
                    build_scene_with_gpu(
                        scene,
                        project_root,
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
                    Ok(())
                })();
                match result {
                    Ok(()) => succeeded += 1,
                    Err(e) => {
                        // In batch mode surface the failure immediately
                        // (real-time); for a single input the diagnostic is
                        // printed once at the end via `error!` below.
                        if render_inputs.len() > 1 {
                            report_error(&e);
                        }
                        failures.push((input_path, e));
                    }
                }
            }
            // A clean build prints a single cargo-style `Finished … in Xs` summary
            // (just like `cargo build`), naming the count of animations produced.
            if failures.is_empty() {
                cargo_finished!(
                    build_start.elapsed(),
                    "{} animation(s)",
                    render_inputs.len()
                );
            } else if render_inputs.len() > 1 {
                // Batch mode: each failure was already printed in real time
                // above, so the final summary is cargo-style — both success and
                // failure counts, no repeated error text — and forces exit code
                // `BATCH_ERROR_EXIT` (111) so callers can detect partial failure.
                let failed = failures.len();
                error!(CandyError::Yee(format!(
                    "yee~ Batch build failed. \\(!_!)/ {succeeded} succeeded, {failed} failed in {:.2}s",
                    build_start.elapsed().as_secs_f64()
                )));
            } else {
                // Single input (non-batch): keep the specific `E00x` code via the
                // diagnostic pipeline — no "Batch failed" summary.
                error!(failures.into_iter().next().unwrap().1);
            }
        }
        Commands::Migrate { inputs, version } => {
            // No inputs: print the migrate subcommand's help and exit cleanly.
            if inputs.is_empty() {
                let mut cmd = Cli::command();
                if let Some(c) = cmd.find_subcommand_mut("migrate") {
                    let _ = c.print_help();
                } else {
                    let _ = cmd.print_help();
                }
                println!();
                return Ok(());
            }
            // Batch mode is **non-fatal per input** (same as build): every input
            // is attempted so partial progress is preserved, failures are
            // collected and surfaced together at the end. With more than one
            // input the process exits with `BATCH_ERROR_EXIT` (111) if *any*
            // input failed; for a single input the specific `E00x` code is kept.
            let mut failures: Vec<(std::path::PathBuf, CandyError)> = Vec::new();
            let mut succeeded = 0usize;
            let migrate_start = Instant::now();
            for input in &inputs {
                let path = input.0.clone();
                cargo_status!("Migrating", "{}", path.display());
                match migrate_file(&path, version.as_deref()) {
                    Ok(0) => {
                        cargo_status!("Finished", "{} up to date", path.display());
                        succeeded += 1;
                    }
                    Ok(n) => {
                        cargo_status!("Finished", "{} rewrote {n} import line(s)", path.display());
                        succeeded += 1;
                    }
                    Err(e) => {
                        if inputs.len() > 1 {
                            report_error(&e);
                        }
                        failures.push((path, e));
                    }
                }
            }
            if !failures.is_empty() {
                if inputs.len() > 1 {
                    let failed = failures.len();
                    error!(CandyError::Yee(format!(
                        "yee~ Batch migrate failed. \\(!_!)/ {succeeded} succeeded, {failed} failed in {:.2}s",
                        migrate_start.elapsed().as_secs_f64()
                    )));
                } else {
                    error!(failures.into_iter().next().unwrap().1);
                }
            }
        }
        Commands::Check {
            inputs,
            fps,
            ignore_version,
        } => {
            // No inputs: print the check subcommand's help and exit cleanly.
            if inputs.is_empty() {
                let mut cmd = Cli::command();
                if let Some(c) = cmd.find_subcommand_mut("check") {
                    let _ = c.print_help();
                } else {
                    let _ = cmd.print_help();
                }
                println!();
                return Ok(());
            }
            // Batch mode is non-fatal per input (same as build): every input is
            // attempted, failures are collected and surfaced together at the end.
            let mut failures: Vec<(std::path::PathBuf, CandyError)> = Vec::new();
            let mut succeeded = 0usize;
            let check_start = Instant::now();
            for input in &inputs {
                let path = input.0.clone();
                cargo_status!("Checking", "{}", path.display());
                if let Err(e) = check_input(Input::from(path.as_path()), ignore_version, fps) {
                    if inputs.len() > 1 {
                        report_error(&e);
                    }
                    failures.push((path, e));
                } else {
                    succeeded += 1;
                }
            }
            if failures.is_empty() {
                cargo_finished!(check_start.elapsed(), "{} file(s)", inputs.len());
            } else if inputs.len() > 1 {
                let failed = failures.len();
                error!(CandyError::Yee(format!(
                    "yee~ Batch check failed. \\(!_!)/ {succeeded} succeeded, {failed} failed in {:.2}s",
                    check_start.elapsed().as_secs_f64()
                )));
            } else {
                error!(failures.into_iter().next().unwrap().1);
            }
        }
    }
    Ok(())
}

/// A single render unit produced by [`collect_render_inputs`].
///
/// `path` is the `.tyx` file to render; `rel` is the directory portion of that
/// path relative to the recursive root it was discovered under — but always
/// **prefixed with the source directory's own name** so the tree is mirrored
/// inside a folder named after the source. For `-r projects/`, a file at
/// `projects/a/b/c.tyx` yields `rel = projects/a/b`, and even a root-level
/// `projects/root.tyx` yields `rel = projects` (never `None`). `resolve_output`
/// then writes it to `<output_dir>/projects/a/b/c.<ext>` (or
/// `<output_dir>/projects/root.<ext>`), so `dist/` gets a `<source-name>/`
/// folder. `rel` is `None` only for a directly-passed `<input>` file, which
/// lands at the top level of `--output-dir` (not under any source-name folder).
struct RenderInput {
    path: std::path::PathBuf,
    rel: Option<std::path::PathBuf>,
}

/// Expand the user's explicit `<inputs>` files and `--recursive <dir>` arguments
/// into one flat, ordered list of `.tyx` render units.
///
/// - Directly-passed `<inputs>` become `RenderInput`s with `rel = None` (they
///   land at the top level of `--output-dir`, not under any source-name folder).
/// - Each `--recursive` directory is walked (via `walkdir`, a mature
///   cross-platform tree walker — no hand-rolled recursion) for every `.tyx`
///   file; for each, `rel` records the path relative to the recursive root,
///   **prefixed with the source directory's own name**, so [`resolve_output`]
///   mirrors the source tree under a `<source-name>/` folder inside
///   `--output-dir` (e.g. `-r projects/` → `dist/projects/a/b/c.<ext>`). Hidden
///   directories (`.git`, `.candy`, …) are skipped to avoid stray recursion.
/// - A `--recursive` argument that is **not a directory** is rejected with an
///   I/O error (E001) — `-r` only accepts directories by design.
fn collect_render_inputs(
    files: &[PathBufOrStr],
    recursive: &[PathBufOrStr],
) -> Result<Vec<RenderInput>, CandyError> {
    let mut out = Vec::new();
    for f in files {
        out.push(RenderInput {
            path: f.0.clone(),
            rel: None,
        });
    }
    for dir in recursive {
        let dir_path = &dir.0;
        // `-r` only accepts directories: a non-directory (or missing path)
        // surfaces as E001 with a clear message.
        let meta = std::fs::metadata(dir_path).map_err(CandyError::Io)?;
        if !meta.is_dir() {
            return Err(CandyError::Io(std::io::Error::new(
                ErrorKind::InvalidInput,
                format!("{} is not a directory", dir_path.display()),
            )));
        }
        // The recursive root's own name becomes the top-level mirror folder
        // under `--output-dir` (e.g. `-r projects/` → `dist/projects/…`), so the
        // whole source tree is preserved inside a folder named after the source.
        // Fall back to the full path's string form for odd roots (`.`, `/`, …)
        // that have no usable `file_name()`.
        let src_name = dir_path
            .file_name()
            .map(|n| n.to_os_string())
            .unwrap_or_else(|| dir_path.as_os_str().to_os_string());
        for entry in walkdir::WalkDir::new(dir_path)
            .follow_links(true)
            .into_iter()
            .filter_entry(|e| {
                // Skip hidden directories (`.git`, `.candy`, …) to avoid
                // recursing into unrelated trees and stray `.tyx` files.
                !e.file_name().to_string_lossy().starts_with('.')
            })
            .filter_map(|e| e.ok())
        {
            let p = entry.path();
            if p.extension().and_then(|s| s.to_str()) == Some("tyx") && p.is_file() {
                // `sub` = the directory portion of `p` relative to `dir_path`
                // (e.g. `a/b` for `<dir>/a/b/c.tyx`); `None` when the file sits
                // directly in the root. `rel` always carries at least the
                // source dir name, so even a root-level `.tyx` lands inside the
                // `<source-name>/` mirror folder rather than at the output root.
                let sub = p
                    .strip_prefix(dir_path)
                    .ok()
                    .and_then(|suffix| suffix.parent())
                    .filter(|pp| !pp.as_os_str().is_empty())
                    .map(|pp| pp.to_path_buf());
                let rel = match sub {
                    Some(s) => std::path::PathBuf::from(&src_name).join(s),
                    None => std::path::PathBuf::from(&src_name),
                };
                out.push(RenderInput {
                    path: p.to_path_buf(),
                    rel: Some(rel),
                });
            }
        }
    }
    Ok(out)
}

/// Whether `c` counts as a path separator on the current platform.
///
/// `/` is a separator everywhere; `\` is a separator only on Windows (on other
/// platforms it is a legal filename character, e.g. `a\b.mp4`).
fn is_separator_char(c: char) -> bool {
    c == '/' || (cfg!(windows) && c == '\\')
}

/// Resolve the final output path.
///
/// `output_name` is the user's custom name for this input (already validated to
/// be a 1:1 match by the caller). When it is `None`, the default
/// `dist/<stem>.<ext>` (or `<output_dir>/<stem>.<ext>` when `--output-dir` is
/// given) is used. When `allow_path` is true (a single-file, non-batch build),
/// `output_name` may be a precise path: anything containing a separator (`/`
/// everywhere, or `\` on Windows) or the platform-independent links `.` / `..`.
/// In that case it is used exactly as written and the file is written to that
/// path (its parent directory is created by the encoder) instead of `dist/`,
/// with `--output-dir` ignored. `.` / `..` and any other relative hops are
/// resolved by the OS when the file is opened — we never hand-roll path
/// normalization. Otherwise a name containing a separator is rejected (with a
/// warning) and the default is used.
///
/// `rel` mirrors the source tree for `--recursive` builds: it is the directory
/// portion of the `.tyx`'s path relative to its recursive root (e.g. `a/b` for
/// `<root>/a/b/c.tyx`). When `rel` is `Some`, the output lands at
/// `<output_dir>/<rel>/<stem>.<ext>` so the directory structure is preserved;
/// when `None` (a directly-passed file, or a file sitting at the root of a
/// recursive directory) the output is `<output_dir>/<stem>.<ext>`. `--output-dir`
/// itself is a directory path and already permits separators (it is joined
/// verbatim), so nested output trees work in both batch and recursive modes.
fn resolve_output(
    output_name: Option<&str>,
    stem: &str,
    ext: &str,
    output_dir: Option<&str>,
    allow_path: bool,
    rel: Option<&Path>,
) -> std::path::PathBuf {
    let default_name = format!("{stem}.{ext}");
    // Single-file build with a precise path (separator, or `.` / `..`) → use it
    // verbatim and let the OS resolve `.` / `..` natively when written.
    if allow_path {
        if let Some(n) = output_name {
            let is_precise_path = n.chars().any(is_separator_char) || n == "." || n == "..";
            if is_precise_path {
                return std::path::PathBuf::from(n);
            }
        }
    }
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
    let dir = Path::new(output_dir.unwrap_or("dist"));
    match rel {
        // Preserve the source sub-tree under the output directory.
        Some(r) => dir.join(r).join(name),
        None => dir.join(name),
    }
}

/// A plain output file name: non-empty and containing no path separators
/// (`/` everywhere, or `\` on Windows), and not `.` / `..`. Multi-level
/// directory paths are rejected so outputs never escape the chosen output
/// directory.
fn is_plain_filename(name: &str) -> bool {
    !name.is_empty() && !name.chars().any(is_separator_char) && name != "." && name != ".."
}

/// The canvas size (Typst points) of a scene's root for resolution purposes.
fn root_page_pt(scene: &Scene) -> (f64, f64) {
    if scene.scenes.is_empty() {
        scene.page_size.unwrap_or(DEFAULT_PAGE_PT)
    } else {
        scene.effective_page_pt(scene.scenes.first().map(|s| s.id).unwrap_or(0))
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
