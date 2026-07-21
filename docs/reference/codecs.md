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
| `h264-vaapi` / `h265-vaapi` / `av1-vaapi` | h264_vaapi / hevc_vaapi / av1_vaapi | Linux Intel/AMD GPU. |
| `vp9` | libvpx-vp9 | VP9 (WebM/MKV). |
| `vp8` | libvpx | VP8 (WebM/MKV). |
| `h264-videotoolbox` / `h265-videotoolbox` | h264_videotoolbox / hevc_videotoolbox | macOS hardware. |
| `h264-qsv` / `h265-qsv` | h264_qsv / hevc_qsv | Intel Quick Sync Video (**Windows**). |

If ffmpeg is not found, Candy falls back to the self-contained codecs or returns E009
(`h265`/`x264`/`x265` without ffmpeg).

## The ffmpeg path

The ffmpeg path feeds raw RGBA frames to ffmpeg and writes the muxed container to a
seekable sink (ffmpeg muxers need a seekable output for the MP4 `faststart` moov
rewrite), then reads the bytes back. Hardware encoders (VAAPI / VideoToolbox / QSV)
upload the RGBA frames to a `nv12` hardware surface with codec-appropriate rate
control.

**Frame input transport** (platform-dependent):

- **Linux**: a `pipe(2)` whose read end is handed to ffmpeg via
  `-i /proc/self/fd/N`. The pipe capacity is grown to ≥ one full frame via
  `fcntl(F_SETPIPE_SZ)` so a single frame always fits. Each frame's RGBA
  buffer is fed to the pipe via `vmsplice(2)` with `SPLICE_F_GIFT` for true
  zero-copy (the buffer's physical pages are gifted to the kernel pipe
  buffer without a `write()`-style user→kernel copy). A pipe's `read()`
  blocks until data is available or the write end is closed — this is the
  streaming contract ffmpeg expects and prevents the premature-EOF race
  that a `memfd`-as-file input would have.
- **Other platforms**: ffmpeg's stdin is a regular OS pipe wrapped in a 1MB
  `BufWriter` that batches ~120 frames per `write()` syscall at 1080p RGBA.

**Mux output sink** (always seekable): on Linux an anonymous `memfd`
(tmpfs-resident, seekable, never touches disk); elsewhere a unique temp file.
ffmpeg writes the whole container to this sink and seeks back for the
`faststart` moov rewrite, then Candy reads the bytes back.

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
every encoder fails, Candy writes an SVG draft to `.candy/` and surfaces E009 (and emits the W004 warning).
