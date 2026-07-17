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

### libva direct (Linux-only, independent codec group)
| Codec | Notes |
|---|---|
| `h264-libva` | Direct VAAPI H.264, no ffmpeg subprocess |
| `h265-libva` | Direct VAAPI HEVC |
| `av1-libva` | Direct VAAPI AV1 |

These are `#[cfg(target_os = "linux")]` gated — they only appear in
`--help` on Linux. They use `LibvaStream` with 1MB BufWriter and
`-low_power 1` for minimal latency.

## Performance Optimizations

### Zero-copy rasterization
`Pixmap::take()` consumes the tiny_skia pixmap's inner Vec instead of
cloning, saving one full-frame memcpy per rasterized frame (~8MB at 1080p).

### BufWriter on ffmpeg stdin
1MB BufWriter batches ~120 frames per write() syscall (at 1080p RGBA),
reducing syscall count by ~125x. Flushes every 16 frames.

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
- `WorldState::detached_cached()`: LRU cache of parsed Typst sources (1024 cap)
- `WorldState::library_with_inputs()`: memoized Library per sys.inputs set (16 cap)
- comemo memoization: frames sharing the same inputs reuse compiled output

## Morph Architecture

Morph uses Flubber's algorithm ported to Rust (`core/morph.rs`):

1. Render target bodies to SVG via typst-svg
2. Extract polygon rings via `extract_rings_from_svg()`
3. Flubber morph: equalize point counts + cyclic alignment + lerp
4. Inject morphed polygon as SVG `<path>` overlay via `morph_overlay_svg()`

The morph overlay is injected in both render paths (whole-doc and
non-whole-doc) after the content and before the transform overlay.
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

Subtitles are rendered as complete SVG documents by compiling a small
Typst snippet per subtitle. The outer `<svg>` tags are stripped via
`extract_svg_inner()` before embedding into the frame SVG, preventing
nested `<svg>` elements.

Subtitle visibility follows Typst scoping rules: one per scope, parental
shadowing, auto-destroy on scope exit.

## CI / Multi-Architecture Builds

8 Rust Tier-1 non-wasm targets, each with its own job and artifact:
- x86_64/aarch64 Linux (gnu)
- x86_64/aarch64 macOS (darwin)
- x86_64/aarch64 Windows (msvc)
- i686 Linux, armv7 Linux

Build cache via `actions/cache@v4` (keyed by target + Cargo.lock).
