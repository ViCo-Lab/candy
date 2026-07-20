# Candy — Architecture & Design Documentation

## Overview

Candy is a **C**ode-oriented **A**nimation e**N**gine **D**esigned for t**Y**pst.
It turns `.tyx` files (valid Typst documents with candy directives) into
encoded videos (MP4/MKV/WebM) using an in-process Typst compiler and
self-contained video encoders.

## Core Pipeline

```
.tyx ─▶ parser ─▶ Scene (AST, valid standard Typst)
                    │
                    ▼
              scheduler ─▶ keyframes (Vec<FrameData>, ms-based)
                    │
                    ▼
            interpolator ─▶ all frames (sampled at 1000/fps ms intervals)
                    │
                    ▼
         renderer (typst) ─▶ SVG per frame (parallel via rayon)
                    │
                    ▼
         raster (cpu/gpu) ─▶ RGBA pixels
                    │
                    ▼
         encoder ─▶ AV1/H.264/HEVC/VP9 ─▶ MP4/MKV/WebM
```

## Timing Model

All timing is in **milliseconds**. The `--fps` CLI flag controls only the
output video's frame rate. A 1000ms slide at 30fps produces ~30 frames.

## Scene Model

A `scene` is a special `page`. `#scene(body)` wraps `body` in `page()`,
so each scene renders as an independent page. Rendering one scene never
touches the content of another.

## Rendering Pipeline (Typst)

Every frame is rendered by recompiling the **whole document** with a fresh
`set sys.inputs` map. The candy-preprocessed Typst source (`param_source`)
is stable across the entire render; only `sys.inputs` changes per frame, so
comemo reuses compiled output for frames that share the same inputs.
`sys.inputs` carries, per object:

- transforms read by `#move` / `#scale` / `#rotate` / `#opacity`
  (`candy:<label>:x|y|scale|rot|opacity`),
- `#reveal` / `#typewriter` prefix length (`candy:<label>:reveal:len`),
- `#transform` body selection when the new body is non-string
  (`candy:<label>:body_idx`),
- counter values (`candy:counter:name`),
- the active scene gate (`candy:active_scene`).

Because the whole document recompiles each frame, there is **no separate
"natural" measurement pass** — the layout is whatever Typst computes from the
current inputs.

### Camera group
When the document declares `#camera`, the rendered base is wrapped in a
single `<g transform>` driven by the `__camera__` mobject's per-frame
`FrameData` (pan + zoom + rotate about the page center). The group is applied
only on frames where a camera is actually active (a per-frame `camera.is_some()`
decision — the camera test supplies `__camera__` through `frames`, not via a
`#camera` directive), so no-camera scenes in the same document are unaffected.

### Subtitle split
- **Camera documents** (`#camera` present): each `#subtitle(...)` call is
  blanked out of the base document and the caption is re-emitted as a
  standalone SVG, embedded as a **camera-independent overlay** so it stays
  screen-fixed regardless of camera motion.
- **No-camera documents**: subtitles render **naturally inside the document**
  and are never overlaid.

`has_camera_directive` (whether the source text contains `#camera`) is the
single source of truth for both the blanking and the overlay, keeping them in
sync.

### Opacity bypass
Typst 0.15 has no in-document `opacity()`, so a fading object
(`0 < opacity < 1`) cannot be expressed in the compiled SVG. Instead the
base frame is rasterized with every fading object **hidden** (all other
objects at full opacity), and each fading object is then rendered as its own
full-opacity layer and alpha-composited over the base at its target opacity
(premultiplied "over", since tiny-skia `Pixmap` is premultiplied). Frames
with no fading objects take the single-rasterization fast path, so the common
case is unchanged.

## Codec Architecture

### Default path (requires system ffmpeg)
| Codec | Encoder | Container |
|---|---|---|
| `x264` (default) | libx264 via ffmpeg | MP4/MKV/WebM |

### Self-contained (no system deps, fallback when ffmpeg unavailable)
| Codec | Encoder | Container |
|---|---|---|
| `h264` | openh264 (linked libopenh264) | MP4/MKV/WebM |
| `av1` | rav1e (pure Rust) | MP4/MKV/WebM |

### FFmpeg-backed (runtime-detected, no cargo dep)
| Codec | ffmpeg encoder | Notes |
|---|---|---|
| `x265` | libx265 | HEVC/H.265 |
| `h264-vaapi` / `h265-vaapi` / `av1-vaapi` | h264_vaapi / hevc_vaapi / av1_vaapi | Linux Intel/AMD GPU |
| `h264-videotoolbox` / `h265-videotoolbox` | h264_videotoolbox / hevc_videotoolbox | macOS hardware |
| `h264-qsv` / `h265-qsv` | h264_qsv / hevc_qsv | Intel Quick Sync (**Windows**) |
| `vp9` / `vp8` | libvpx-vp9 / libvpx | WebM |

> **Platform availability.** The hardware encoders above are conditionally compiled
> (`#[cfg(target_os = "...")]`): `h264-vaapi` / `h265-vaapi` / `av1-vaapi` appear
> only on **Linux**, `h264-videotoolbox` / `h265-videotoolbox` only on **macOS**,
> and `h264-qsv` / `h265-qsv` only on **Windows**. On other platforms they are
> absent from `--help` and the `--codec` selection interface.

## Performance Optimizations

### Zero-copy rasterization
`Pixmap::take()` consumes the tiny_skia pixmap's inner Vec instead of
cloning, saving one full-frame memcpy per rasterized frame (~8MB at 1080p).

### Zero-copy frame feed to ffmpeg (Linux)
On Linux the ffmpeg frame input is a **pipe** (not a bounded OS pipe — the
pipe capacity is grown to ≥ one full frame via `fcntl(F_SETPIPE_SZ)` so a
single frame always fits). Each frame's RGBA buffer is fed to the pipe via
`vmsplice(2)` with `SPLICE_F_GIFT`, which transfers the buffer's physical
pages to the kernel pipe buffer **without a `write()`-style user→kernel
copy** — true zero-copy on the producer side. The buffer is padded to a
page multiple (zero-filled tail that ffmpeg never reads) so the `vmsplice`
alignment contract holds; large allocations (≥ ~128 KiB) go through glibc's
`mmap` path, which returns page-aligned pointers for free, so HD/4K frames
take the zero-copy path automatically. Small frames fall back to `write()`
silently (still correct, one extra copy).

The pipe's read end blocks ffmpeg's `read()` until data is available or the
write end is closed — this is the streaming contract ffmpeg expects and
prevents the premature-EOF race that a `memfd`-as-file input would have
(ffmpeg reading a regular file sees EOF the instant it catches up to the
file size, finalising the container with only a handful of frames encoded).

The mux **output** sink and the stderr redirection still use `memfd`s —
ffmpeg's MP4 muxer needs a seekable output for the `faststart` moov rewrite,
and the stderr sink needs an unbounded buffer so a long encode cannot
deadlock on a full stderr pipe. Both are write-only-from-ffmpeg so the EOF
race does not apply.

### BufWriter on ffmpeg stdin (non-Linux)
On non-Linux platforms the ffmpeg frame input is a regular stdin pipe
wrapped in a 1MB `BufWriter`, batching ~120 frames per `write()` syscall at
1080p RGBA and reducing syscall count by ~125x. Flushes every 16 frames.

### x86-64-v3 ISA
Native x86_64 builds enable AVX2 + BMI1/2 + FMA + MOVBE + F16C via
build.rs `target-feature` flags, allowing the compiler to auto-vectorize
tight loops in usvg/resvg pixel compositing and rav1e DCT.

### Streaming pipeline
The `StreamingVideo` encoder pushes frames one at a time through a bounded
channel, keeping peak memory at `O(parallelism)` frames regardless of
total frame count. The reorder buffer is window-bounded to prevent O(N)
memory growth.

### Typst cache reuse
- `WorldState::main_source()`: LRU cache of parsed Typst sources (1024 cap)
- `WorldState::library_with_inputs()`: memoized Library per sys.inputs set (16 cap)
- comemo memoization: frames sharing the same inputs reuse compiled output

## Morph Architecture

Morph uses Flubber's algorithm ported to Rust (`core/morph.rs`):

1. Render target bodies to SVG via typst-svg
2. Extract polygon rings via `extract_rings_from_svg()`
3. Flubber morph: equalize point counts + cyclic alignment + lerp
4. Inject morphed polygon as SVG `<path>` overlay via `morph_overlay_svg()`

The morph overlay is injected in `compose_frame_svg` after the content
and before the transform overlay (a single whole-document render path).
The morph polygon is always fully visible (no opacity wrapper) —
the crossfade is handled by the `from` object's FadeOut + ScaleBy.

### Bezier glyph outlines
`glyph_outline()` uses `font.outline()` + de Casteljau subdivision to
flatten quadratic/cubic Bezier curves into polygon points. This enables
morphing of formula characters.

## Transform Architecture

`#transform(target, to: <content>)` morphs a mobject's content into new
content by splitting both old and new formulas into glyph fragments,
matching them by geometry, and interpolating each fragment's position,
opacity, scale, and rotation independently.

The transform overlay is emitted as SVG `<defs>` + `<use>` elements
(one definition per plan, clipped per fragment) to keep the SVG small.

## Subtitle Architecture

On **camera documents**, subtitles are rendered as complete SVG documents by
compiling a small Typst snippet per subtitle. The outer `<svg>` tags are
stripped via `extract_svg_inner()` before embedding into the frame SVG as a
camera-independent overlay, preventing nested `<svg>` elements. (The base
document's `#subtitle(...)` calls are blanked so the caption is not
double-drawn.)

On **no-camera documents**, subtitles render natively inside the document
and are not overlaid — `has_camera_directive` (whether the source contains
`#camera`) is the single source of truth for both the blanking and the
overlay.

Subtitle visibility follows Typst scoping rules: one per scope, parental
shadowing, auto-destroy on scope exit.

## CI / Multi-Architecture Builds

10 Rust Tier-1 non-wasm targets, each with its own job and artifact:
- x86_64/aarch64 Linux (gnu)
- x86_64/aarch64 macOS (darwin)
- x86_64/aarch64 Windows (msvc) + x86_64 Windows (gnu) + i686 Windows (msvc)
- i686 Linux, armv7 Linux

Build cache via `actions/cache@v4` (keyed by target + Cargo.lock).
