# Codec & container matrix

Candy ships two **self-contained** video encoders (no system dependencies). When the system
has **`ffmpeg`** on `$PATH`, Candy can additionally shell out to it for higher-quality /
hardware-accelerated codecs — runtime-detected, no cargo dependency.

## Self-contained (default, pure Rust)

| `--codec` | Encoder | Container | Notes |
|---|---|---|---|
| `h264` (default) | openh264 (linked libopenh264) | MP4/MKV/WebM | Software H.264; falls back to AV1 if openh264 fails. |
| `av1` | rav1e (pure Rust) | MP4/MKV/WebM | Full-quality AV1, then all-intra retry, then H.264 fallback. |
| `h265` | — | — | Self-contained build returns E007; with system ffmpeg uses x265. |

## FFmpeg-backed (runtime-detected, no cargo dep)

| `--codec` | ffmpeg encoder | Use case |
|---|---|---|
| `x264` | libx264 | Higher-quality H.264 than openh264. |
| `x265` | libx265 | H.265/HEVC. |
| `h264-vaapi` / `h265-vaapi` | h264_vaapi / hevc_vaapi | Linux Intel/AMD GPU. |
| `h264-videotoolbox` / `h265-videotoolbox` | h264_videotoolbox / hevc_videotoolbox | macOS hardware. |
| `h264-qsv` / `h265-qsv` | h264_qsv / hevc_qsv | Intel Quick Sync Video (**Windows**). |

> **Platform availability.** The hardware encoders above are conditionally compiled
> (`#[cfg(target_os = "...")]`): `h264-vaapi` / `h265-vaapi` / `av1-vaapi` appear
> only on **Linux**, `h264-videotoolbox` / `h265-videotoolbox` only on **macOS**,
> and `h264-qsv` / `h265-qsv` only on **Windows**. On other platforms they are
> absent from `--help` and the `--codec` selection interface.

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

`rav1e` 0.8.1 (the latest published release) can panic during inter-prediction on certain
frame geometries. Candy first tries full-quality AV1, and on that panic automatically
retries in all-intra mode (valid AV1, no temporal compression); only if that also fails does
it fall back to H.264. The panic is caught (`catch_unwind`) so the command never aborts — if
every encoder fails, Candy writes an SVG draft to `.candy/` and surfaces E007.

## Audio muxing

- Opus (`.opus`/`.ogg`) → MKV/WebM.
- AAC (`.aac`) → MP4. MP4 only muxes AAC; a non-AAC track is ignored (W008).
- An audio track with an unsupported format or codec mismatch is dropped (W006).
