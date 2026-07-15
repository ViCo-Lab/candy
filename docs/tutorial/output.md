# Output & codecs

How to invoke `candy build`, what formats and codecs exist, and where the artifacts land.

## Basic usage

```sh
# Default: H.264 in an MP4 container, written to dist/<stem>.mp4
candy build examples/dot_move.tyx

# AV1 in WebM (Matroska with webm doctype)
candy build examples/dot_move.tyx --format webm --codec av1

# H.264 in MP4 (default self-contained codec; opt into AV1 with --codec av1)
candy build examples/dot_move.tyx --format mp4 --codec h264

# SVG draft (one file per frame, written to .candy/<stem>/)
candy build examples/dot_move.tyx --format svg

# Animated GIF of every frame (looping)
candy build examples/dot_move.tyx --format gif

# Static PNG poster of the final frame (the animation "poster")
candy build examples/dot_move.tyx --format png

# Build from an SVG rendered by @preview/candy (candy-json round-trip)
candy build scene.svg --from-svg --format mp4
```

**When debugging**, use `cargo run -- <args>` instead of `candy <args>`.

**Batch builds.** `candy build` accepts multiple inputs (`candy build a.tyx b.tyx …`).
Every input is attempted (no fail-fast); if any fails, Candy reports each failed input
and exits with code `111` (the `EYEE` batch marker) while the successful ones still
produce output. A single failed input keeps its specific `E00x` code.

## Output formats

| `--format` | Result |
|---|---|
| `mp4` (default) | H.264/AV1 video in an MP4 container. |
| `mkv` | video in a Matroska container. |
| `webm` | video in a Matroska container with the `webm` doctype. |
| `gif` | animated GIF of every frame (looping) — no codec needed; `--codec` ignored. |
| `png` | static RGBA bitmap of the **final** frame (poster) — no codec needed; `--codec` ignored. |
| `svg` | SVG draft, one file per frame under `.candy/<stem>/`. |

## Codec matrix

Candy ships two **self-contained** video encoders (no system dependencies):

| `--codec` | Encoder | Container | Notes |
|---|---|---|---|
| `h264` (default) | openh264 (linked libopenh264) | MP4/MKV/WebM | Software H.264; falls back to AV1 if openh264 fails. |
| `av1` | rav1e (pure Rust) | MP4/MKV/WebM | Tries full-quality AV1 (inter-prediction); if rav1e 0.8.1 panics on the frame geometry it automatically retries in all-intra mode, then falls back to H.264. |
| `h265` | — | — | Self-contained build returns E007; with system ffmpeg, uses x265. |

When the system has **`ffmpeg`** on `$PATH`, Candy can shell out to it for additional
codecs — no cargo dependency, runtime-detected. This enables hardware-accelerated
encoding and higher-quality software codecs:

| `--codec` | ffmpeg encoder | Use case |
|---|---|---|
| `x264` | libx264 | Higher-quality H.264 than openh264. |
| `x265` | libx265 | H.265/HEVC (smaller files at same quality). |
| `h264-vaapi` | h264_vaapi | Linux Intel/AMD GPU H.264. |
| `h265-vaapi` | hevc_vaapi | Linux Intel/AMD GPU H.265. |
| `h264-videotoolbox` | h264_videotoolbox | macOS hardware H.264. |
| `h265-videotoolbox` | hevc_videotoolbox | macOS hardware H.265. |
| `h264-qsv` | h264_qsv | Intel Quick Sync Video H.264. |
| `h265-qsv` | hevc_qsv | Intel Quick Sync Video H.265. |

```sh
# Software H.264 via system ffmpeg + libx264
cargo run -- build anim.tyx --codec x264

# Hardware H.265 via VAAPI (Linux, Intel/AMD GPU)
cargo run -- build anim.tyx --codec h265-vaapi

# Hardware H.264 via VideoToolbox (macOS)
cargo run -- build anim.tyx --codec h264-videotoolbox
```

The ffmpeg path pipes raw RGBA frames to ffmpeg's stdin and writes the muxed container to
a unique temp file (ffmpeg muxers require a seekable output), then reads the bytes back.
Hardware encoders (VAAPI / VideoToolbox / QSV) upload the RGBA frames to a hardware
surface (`format=nv12,hwupload`) and use codec-appropriate rate control. If ffmpeg is not
found, Candy falls back to the self-contained codecs (av1/h264) or returns E007
(`h265`/`x264`/`x265` without ffmpeg).

> **Encoding fallback.** `rav1e` 0.8.1 can panic during inter-prediction on certain frame
> geometries. Candy first tries full-quality AV1, and on that panic automatically retries
> in all-intra mode (valid AV1, no temporal compression); only if that also fails does it
> fall back to H.264. The panic is caught (`catch_unwind`) so the command never aborts —
> if every encoder fails, Candy writes an SVG draft to `.candy/` and surfaces E007.

## CLI flags

| Flag | Default | Description |
|---|---|---|
| `<input>` (positional) | required | Path to the `.tyx` X-sheet, or an SVG with a `candy-json` block (see `--from-svg`). |
| `--from-svg` | off | Force the input to be parsed as an SVG rendered by `@preview/candy`. Without this flag, the parser is selected by file extension (`.svg` → SVG round-trip, anything else → `.tyx`). |
| `-o, --output` | `out` | Output name hint under `dist/` for videos; ignored for SVG drafts. |
| `--format` | `mp4` | `mp4` / `mkv` / `webm` / `gif` / `png` / `svg`. The `--codec` flag is ignored for `gif` / `png`. |
| `--codec` | `h264` | `av1` / `h264` / `h265` / `x264` / `x265` / `h264-vaapi` / `h265-vaapi` / `h264-videotoolbox` / `h265-videotoolbox` / `h264-qsv` / `h265-qsv`. |
| `-f, --fps` | `30` | Frames per second (video path). |
| `-p, --pixel-per-pt` | `2.0` | Rasterization resolution (pixels per Typst point). |
| `--gpu` | off | Use GPU rasterization (vello + wgpu) for the video path. Requires `cargo build --features gpu`. Falls back to CPU if the feature is off or no GPU adapter is available. |
| `--keep-intermediates` | off | Keep the `.candy/<stem>/` intermediate directory after a successful build. |

Full flag and artifact details: [Reference · CLI](../reference/cli.md).

## Artifacts

- `.candy/<stem>/` — intermediates: `frames.rgba` (raw RGBA bundle), `frame_*.svg` (draft
  frames, also written on encode failure). For video builds this directory is **removed
  automatically** after a successful run unless `--keep-intermediates` is passed;
  `--format svg` keeps it (that draft *is* the output).
- `dist/<stem>.<ext>` — final video (MP4 / MKV / WebM), animated GIF (`.gif`), or static
  PNG bitmap of the final frame (`.png`).

That's the end of the tutorial. For the complete directive list, easing curves, error
codes, and the Rust API, see the [Reference](../reference/README.md).
