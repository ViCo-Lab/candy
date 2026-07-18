# Codec & container matrix

Candy ships two **self-contained** video encoders (no system dependencies). The default
codec (`x264`) uses system **`ffmpeg`** for higher-quality encoding; when `ffmpeg` is
unavailable, Candy transparently falls back to openh264 (`h264`).

## Self-contained (default, pure Rust)

| `--codec` | Encoder | Container | Notes |
|---|---|---|---|
| `h264` | openh264 (linked libopenh264) | MP4/MKV/WebM | Software H.264; used as fallback when x264 unavailable. |
| `av1` | rav1e (pure Rust) | MP4/MKV/WebM | Full-quality AV1, then all-intra retry, then H.264 fallback. |

## Default codec (requires ffmpeg)

The default codec (`x264`) uses system **`ffmpeg`** for higher-quality encoding. When `ffmpeg` is unavailable, Candy transparently falls back to openh264 (`h264`) so a valid video is still produced.

| `--codec` | Encoder | Container | Notes |
|---|---|---|---|
| `x264` (default) | ffmpeg + libx264 | MP4/MKV/WebM | Higher-quality H.264; **falls back to openh264 if ffmpeg unavailable**. |

## FFmpeg-backed (runtime-detected, no cargo dep)

| `--codec` | ffmpeg encoder | Use case |
|---|---|---|
| `x264` | libx264 | Higher-quality H.264 than openh264. |
| `x265` | libx265 | H.265/HEVC. |
| `h264-vaapi` / `h265-vaapi` | h264_vaapi / hevc_vaapi | Linux Intel/AMD GPU. |
| `h264-videotoolbox` / `h265-videotoolbox` | h264_videotoolbox / hevc_videotoolbox | macOS hardware. |
| `h264-qsv` / `h265-qsv` | h264_qsv / hevc_qsv | Intel Quick Sync Video (**Windows**). |

## VAAPI / libva (Linux-only, independent group)

| `--codec` | Notes |
|---|---|
| `h264-libva` | Direct VAAPI H.264, no ffmpeg subprocess (Linux Intel/AMD GPU). |
| `h265-libva` | Direct VAAPI HEVC. |
| `av1-libva` | Direct VAAPI AV1. |

These are `#[cfg(target_os = "linux")]` gated — they only appear in `--help`
on Linux. They require `/dev/dri/renderD128` (Intel/AMD GPU) and use
`LibvaStream` with a 1MB BufWriter and `-low_power 1` for minimal latency. If
VAAPI is unavailable, `LibvaStream::new` returns E007.

If ffmpeg is not found, Candy falls back to the self-contained codecs or returns E007
(`h265`/`x264`/`x265` without ffmpeg).

## The ffmpeg path

The ffmpeg path pipes raw RGBA frames to ffmpeg's stdin and writes the muxed container to a
unique temp file (ffmpeg muxers need a seekable output), then reads the bytes back. Hardware
encoders (VAAPI / VideoToolbox / QSV) upload the RGBA frames to a `nv12` hardware surface
with codec-appropriate rate control.

```sh
# Software H.264 via system ffmpeg + libx264
cargo run -- build anim.tyx --codec x264

# Hardware H.265 via VAAPI (Linux, Intel/AMD GPU)
cargo run -- build anim.tyx --codec h265-vaapi

# Hardware H.264 via VideoToolbox (macOS)
cargo run -- build anim.tyx --codec h264-videotoolbox
```

## Encoding fallback

When `--codec x264` (the default) and ffmpeg is unavailable or fails to initialise,
Candy transparently falls back to `h264` (openh264) so a valid video is still produced.

`rav1e` 0.8.1 (the latest published release) can panic during inter-prediction on certain
frame geometries. Candy first tries full-quality AV1, and on that panic automatically
retries in all-intra mode (valid AV1, no temporal compression); only if that also fails does
it fall back to H.264. The panic is caught (`catch_unwind`) so the command never aborts — if
every encoder fails, Candy writes an SVG draft to `.candy/` and surfaces E007.
