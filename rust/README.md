# Candy Rust Backend — Architecture & API

This document is the **developer reference** for Candy's Rust rendering & encoding
backend (the `rust/` crate). It covers the layered pipeline, the module layout, the
public API, the timing model, codecs, and the error model. For the *user-facing* Typst
DSL, see the [Typst package README](../typst/README.md).

---

## Table of contents

- [Overview](#overview)
- [Pipeline](#pipeline)
- [Module layout](#module-layout)
- [Public API](#public-api)
  - [`candy::build` / `build_input` / `build_input_with_gpu`](#build-functions)
  - [`Input`](#input)
  - [`OutputFormat`](#outputformat)
  - [`Codec`](#codec)
- [Core modules](#core-modules)
  - [`core::ast`](#coreast)
  - [`core::scheduler`](#corescheduler)
  - [`core::interpolator`](#coreinterpolator)
  - [`core::easing`](#coreeasing)
  - [`core::morph`](#coremorph)
  - [`core::error`](#coreerror)
- [Parser modules](#parser-modules)
- [Renderer modules](#renderer-modules)
- [Timing model](#timing-model)
- [Codec & container matrix](#codec--container-matrix)
- [Error model (E001–E007)](#error-model-e001e007)
- [Artifacts](#artifacts)
- [Building & features](#building--features)

---

## Overview

Candy is a **C**ode-oriented **A**nimation e**N**gine **D**esigned for t**Y**pst. The
Rust crate (`candy`) is the high-performance backend that:

- parses a `.tyx` X-sheet (or an SVG round-trip) into a `Scene` AST,
- schedules & interpolates that AST into per-frame `FrameData`,
- renders each frame **in-process** with the `typst` compiler library (never spawning
  the CLI),
- encodes the frames with self-contained codecs (`rav1e` for AV1, `openh264` for
  H.264) and muxes them into MP4 / Matroska (WebM/MKV) — **no FFmpeg, no external CLI**.

When the system has `ffmpeg` on `$PATH`, candy can additionally shell out to it for
higher-quality / hardware-accelerated codecs (`x264`, `x265`, `*-vaapi`,
`*-videotoolbox`, `*-qsv`), runtime-detected with no cargo dependency.

The crate follows a **strict layered pipeline**: no circular dependencies, no
cross-module side effects. Each layer's postconditions are cheap to assert.

---

## Pipeline

```
.tyx ─▶ parser::parse_tyx ─▶ Scene (AST, valid standard Typst)
                         │
                         ▼
        core::scheduler::schedule ─▶ keyframes (Vec<FrameData>)
                         │
                         ▼
      core::interpolator::interpolate ─▶ all frames (Vec<FrameData>)
                         │
                         ▼
   renderer::typst::Renderer ─▶ pixel frames (parallel via rayon)
                         │
                         ▼
   renderer::video ─▶ AV1 (rav1e) / H.264 (openh264) / ffmpeg ─▶ MP4 / MKV / WebM
                         └▶ GIF (animated, pure-Rust `gif`)
                         └▶ PNG (static bitmap of final frame, pure-Rust `png`)
```

The same flow is reachable from an SVG produced by `@preview/candy`: `Input::Svg` →
`parser::extract_dsl_from_svg` → `Scene`, recovering the embedded `candy-json` block.

---

## Module layout

```
rust/src/
├── main.rs            # CLI (clap): `candy build` + hidden easter-egg `candy candy`
├── lib.rs             # Public API: build/build_input/build_input_with_gpu, Input, OutputFormat
├── core/              # pure data + scheduling / interpolation (no I/O, no render)
│   ├── ast.rs         # Scene, FrameData, Action, Label — the shared data model
│   ├── easing.rs      # Easing enum + resolve() (named curves + expr:/bezier:)
│   ├── diag.rs        # CandyError (E001–E007) + CandyWarn (W001–W011) + `error!`/`warn!`/`debug!`/`info!` macros
│   ├── interpolator.rs# interpolate / interpolate_with (sampling frames)
│   ├── meta.rs        # PrivateMeta (internal bookkeeping)
│   ├── morph.rs       # Flubber port: SVG → polygon rings → morph → path string
│   └── scheduler.rs   # schedule(): Scene → keyframes (Vec<FrameData>)
├── parser/
│   ├── tyx.rs         # parse_tyx: .tyx → Scene (AST scan + import analysis)
│   ├── dsl.rs         # DSL helper extraction
│   └── mod.rs
└── renderer/
    ├── typst.rs       # in-process typst compile/render → SVG (draft) | RGBA pixels
    ├── gpu.rs         # (feature "gpu") vello + wgpu GPU rasterization
    ├── video.rs       # encode_frames / mux / collect_audio; Codec, Container, EncodedVideo
    ├── rav1e.rs       # AV1 encoder (pure Rust; all-intra fallback; H.264 fallback)
    ├── h264.rs        # openh264 H.264 encoder
    ├── ffmpeg.rs      # find_ffmpeg / encode_via_ffmpeg (system ffmpeg shell-out)
    ├── container.rs   # hand-written MP4 / Matroska / WebM muxers
    └── audio.rs       # Opus/AAC audio demuxers
```

---

## Public API

### `build` functions {#build-functions}

```rust
pub fn build(
    input: &Path,
    intermediate_dir: &Path,
    output: &Path,
    format: OutputFormat,
    codec: Codec,
    fps: u32,
    pixel_per_pt: f32,
) -> Result<(), CandyError>;

pub fn build_input(
    input: Input,
    intermediate_dir: &Path,
    output: &Path,
    format: OutputFormat,
    codec: Codec,
    fps: u32,
    pixel_per_pt: f32,
) -> Result<(), CandyError>;

pub fn build_input_with_gpu(
    input: Input,
    intermediate_dir: &Path,
    output: &Path,
    format: OutputFormat,
    codec: Codec,
    fps: u32,
    pixel_per_pt: f32,
    use_gpu: bool,
) -> Result<(), CandyError>;
```

- `build` is a backward-compatible wrapper that selects the parser by file extension
  (`.svg` → SVG round-trip, anything else → `.tyx`).
- `build_input` is the same but takes an explicit [`Input`], so callers can force the
  SVG path even when the file extension is not `.svg`.
- `build_input_with_gpu` adds a `use_gpu` flag. When `use_gpu` is true **and** the
  `gpu` cargo feature is compiled in, candy rasterizes each frame on the GPU via vello
  + wgpu. If the feature is off, `use_gpu` is silently ignored (CPU path). If the
  feature is on but no GPU adapter is available, candy falls back to CPU and emits a
  warning.

End-to-end, `build_input_with_gpu` performs:

1. `input.parse()` → `Scene` (parser).
2. `scheduler::schedule(&scene)` → keyframes; the timeline is then extended so
   long-lived `subtitle`s / `counter`s that end *after* the last mobject keyframe are
   still covered (a final keyframe is appended at the extended end, holding each
   target's last state).
3. `interpolator::interpolate_with(keyframes, Linear, fps)` → all frames.
4. `Renderer::with_root(scene, project_root)`; sample times are collected (one per
   video frame).
5. **SVG draft path** (`OutputFormat::Svg`): each frame's SVG is written to
   `.candy/<stem>/frame_*.svg` and the function returns — no video is produced.
6. **Video path**: rasterize frames (parallel via rayon on CPU; serial on GPU), persist
   an `frames.rgba` draft, then encode + mux:
   - `Mp4` / `Mkv` / `Webm`: video containers. FFmpeg codecs (`uses_ffmpeg()`, or
     `H265` with `ffmpeg` present) shell out to ffmpeg and bypass candy's muxer.
     Self-contained codecs (`Av1`, `H264`) go through `rav1e`/`openh264` + candy's
     hand-written muxer, with audio collected via `collect_audio`. On any encode
     failure, candy writes an SVG draft to `.candy/` and surfaces the error
     (`E007` for encode).
   - `Gif`: an animated GIF of every frame (looping), encoded in-process via the
     pure-Rust `gif` crate. The `--codec` flag is ignored.
   - `Png`: a single static RGBA bitmap of the **final** frame (the animation
     "poster"), encoded in-process via the pure-Rust `png` crate. The `--codec`
     flag is ignored.

### `Input` {#input}

```rust
pub enum Input {
    /// A `.tyx` Typst X-sheet (parsed via `parser::parse_tyx`).
    Tyx(PathBuf),
    /// An SVG rendered by `@preview/candy`, containing a `candy-json` block
    /// (parsed via `parser::extract_dsl_from_svg`).
    Svg(PathBuf),
}
```

`Input::parse()` returns the `Scene`; `Input::project_root()` returns the parent
directory of the source file, wired into `Renderer::with_root` so local
`#import "file.typ"` calls resolve relative to the source.

### `OutputFormat` {#outputformat}

```rust
pub enum OutputFormat { Svg, Mp4, Mkv, Webm, Gif, Png }
```

`Svg` is a draft written to `.candy/` (never `dist/`). `Mp4` / `Mkv` / `Webm` are video
containers (`Webm` = Matroska with the `webm` doctype). `Gif` is an animated GIF of all
frames (looping); `Png` is a static RGBA bitmap of the final frame. The CLI exposes these
via `--format {mp4,mkv,webm,gif,png,svg}`, plus `--output <name>...` (one plain file name
per input — no path separators; mismatched counts or directory paths fall back to
`dist/<stem>.<ext>` with a warning) and `--output-dir <dir>` (redirects every output file
into a single directory).

### `Codec` {#codec}

```rust
pub enum Codec {
    Av1, H264, H265,
    X264, X265,
    H264Vaapi, H265Vaapi,
    H264VideoToolbox, H265VideoToolbox,
    H264Qsv, H265Qsv,
}
```

- `Av1` / `H264` are self-contained (rav1e / openh264). `H265` returns `E007` unless
  system ffmpeg is available (in which case it uses x265).
- The rest are ffmpeg-backed (runtime-detected). `Codec::uses_ffmpeg()` reports whether
  a codec shells out to ffmpeg.

---

## Core modules

### `core::ast` {#coreast}

The shared data model — the single source of truth across `parser`, `core`, and
`renderer`. Types are immutable after creation (builder-time mutation is confined to
the parser).

- `Label(String)` — unique animatable id; `Label::parse("@name")` validates
  `@[A-Za-z0-9_-]+` (without the leading `@`).
- `Action` — an animation applied to a target within a slide. Core transforms:
  `MoveTo` / `MoveBy` (absolute / relative shift), `Scale` / `ScaleBy`, `Rotate` /
  `RotateBy`, `FadeIn` / `FadeOut` / `FadeTo`, `MoveAlongPath`. Manim-style:
  `SaveState` / `Restore`, `Indicate`, `Flash`, `Wiggle`, `SetColor`, `Show` / `Hide`
  (instantaneous visibility toggles).
- `FrameData` — a sampled transform state at a given `time_ms` for one target.
- `Scene` — the parsed document: items, initial states, actions, subtitles,
  counters. It also carries the **scene tree**: `scenes: Vec<SceneInfo>` (each with
  `id` / `parent` / `scope` / `page_size` / `start_ms` / `end_ms` / `owns_labels`)
  and an optional `root_scene` index. `active_scene_at(time_ms)` returns the deepest
  scene active at a given frame (the renderer uses it to hide parent scenes);
  `effective_page_pt(id)` resolves a scene's canvas size (inheriting from the nearest
  ancestor that declares one). When `scenes` is empty (no `scene` call), the whole
  document is one implicit scene — preserving v0.1 behavior.

### `core::scheduler` {#corescheduler}

`schedule(scene) -> Result<Vec<FrameData>, CandyError>` turns the `Scene` AST into
keyframe `FrameData`. Each animatable item gets a frame-0 default keyframe (seeded from
`scene.initial`) and a final keyframe at the last frame. A non-monotonic `time_ms` for a
target returns `CandyError::Parse` (E002) instead of panicking (spec §6).

### `core::interpolator` {#coreinterpolator}

`interpolate_with(keyframes, method, fps)` samples the keyframes into all output frames
(one per video frame) using the per-action `Easing`. `InterpMethod::Linear` is the
default. Out-of-range interpolation is clamped (emits `E005`, non-fatal).

### `core::easing` {#coreeasing}

The `Easing` enum + `Easing::resolve()`. Supports named curves (`linear`, `smooth`,
`cubic-in-out`, `there-and-back`, `wiggle`, `lingering`, …) and custom specs
`expr:<math>` (an expression in `t ∈ [0,1]`) and `bezier:x1,y1,x2,y2` (CSS-style cubic
Bézier). Unknown names fall back to `linear`.

### `core::morph` {#coremorph}

Flubber's polygon-morph algorithm, ported to Rust:

1. Render the target mobject to SVG and `extract_rings_from_svg()` — extract `<rect>`,
   `<circle>`, `<ellipse>`, `<polygon>`, `<polyline>`, and `<path d="...">` into polygon
   rings.
2. `interpolate_ring()` equalizes point counts, finds the best cyclic alignment (O(n²)),
   and interpolates index-by-index.
3. `ring_to_path_string()` converts the morphed ring back to a path string for Typst.

Glyph outlines use **de Casteljau subdivision** (`flatten_quad` / `flatten_cubic`) to
flatten quadratic/cubic Bézier curves into polygon points, enabling true point-by-point
morphing of formula characters.

### `core::error` {#coreerror}

See [Error model](#error-model-e001e007) below.

---

## Parser modules

- `parser::parse_tyx` — scans a `.tyx` file (valid standard Typst) and builds the
  `Scene` AST via import analysis. Because the parser resolves the *binding* (not the
  literal `candy.` prefix), both `#import "candy": *` + `mobject(...)` and
  `#import "candy"` + `candy.mobject(...)` work.
- `parser::extract_dsl_from_svg` — recovers the `candy-json` block embedded in an SVG
  rendered by `@preview/candy`, reconstructing the `Scene` (the Typst → SVG → candy
  round-trip).
- `parser::dsl` — shared DSL helper extraction.

---

## Renderer modules

- `renderer::typst::Renderer` — compiles each mobject/per-frame document **in-process**
  with the `typst` crate library (the CLI is never spawned). `render_frame_at` produces
  SVG (draft); `render_frame_pixels_par` produces RGBA8 pixels (data-parallel via rayon).
  `ensure_natural_public()` pre-computes the natural layout once so the parallel loop can
  share the `WorldState` via `Arc`.
  - **Per-glyph `#transform`** (inline content): `build_transform_fragments` renders the
    whole old and new bodies to SVG and `extract_formula` pulls every glyph/decoration
    (fraction bars, roots, …) out as a positioned fragment via Typst's *own* SVG layout
    (no custom token scanner). Old↔new fragments are matched by outline signature via LCS
    into `GlyphAnim`s stored in `transform_fragments: Vec<TransformFragmentPlan>`; during
    the window matched fragments glide, removed ones fade/slide out, and inserted ones
    fade/slide in — so the content disassembles and reassembles glyph-by-glyph instead of
    dissolving as one block. `tokenize_math` keeps fractions (`a/b`, `\frac{a}{b}`) intact
    so the bar renders correctly. Shape transforms fall back to the crossfade + scale
    morph.
- `renderer::gpu` (feature `gpu`) — `GpuRenderer` rasterizes frames on the GPU via vello
  + wgpu (`render_frame_pixels_gpu`). Serial (single device); falls back to CPU if no
  adapter is found.
- `renderer::video` — `encode_frames` (rav1e/openh264), `mux` (hand-written
  MP4/Matroska), `collect_audio`, plus `Codec` / `Container` / `EncodedVideo`. Also
  `write_rgba_draft` for the `.candy` intermediates.
- `renderer::rav1e` — AV1 encoder (pure Rust). On the known rav1e 0.8.1 inter-prediction
  panic it automatically retries in all-intra mode, then falls back to H.264 (the panic
  is `catch_unwind`-guarded so the command never aborts).
- `renderer::h264` — openh264 software H.264 encoder (linked `libopenh264`).
- `renderer::ffmpeg` — `find_ffmpeg` and `encode_via_ffmpeg` pipe raw RGBA frames to
  ffmpeg's stdin and read back the muxed bytes (ffmpeg muxers need a seekable output, so
  bytes go to a unique temp file). Hardware encoders upload RGBA to a `nv12` hardware
  surface with codec-appropriate rate control.
- `renderer::container` — hand-written MP4 / Matroska / WebM muxers. (Note: the EBML
  vint encoder must write multi-byte vints in **forward** byte order, or ffmpeg decoders
  misread element lengths and SimpleBlocks overflow their cluster.)
- `renderer::audio` — Opus (`.opus`/`.ogg`, for WebM/MKV) and AAC (`.aac`, for MP4)
  demuxers, feeding `collect_audio`.

---

## Timing model

All timing is in **milliseconds** (not frames). `--fps` controls only the output frame
rate: a 1000 ms slide at 30 fps ≈ 30 frames, at 60 fps ≈ 60 frames — same wall-clock
duration. `#subtitle` and `#ecounter` lifetimes are given directly in ms; `#animate` /
`#pause` / `#play` durations count in frames but the scheduler converts the timeline to
ms for sampling.

---

## Codec & container matrix

**Self-contained** (no system dependencies — the default, pure Rust):

| `--codec` | Encoder | Container | Notes |
|---|---|---|---|
| `h264` (default) | openh264 (linked libopenh264) | MP4/MKV/WebM | Software H.264; falls back to AV1 if openh264 fails. |
| `av1` | rav1e (pure Rust) | MP4/MKV/WebM | Full-quality AV1, then all-intra retry, then H.264 fallback. |
| `h265` | — | — | Self-contained build returns E007; with system ffmpeg uses x265. |

**FFmpeg-backed** (runtime-detected, no cargo dep):

| `--codec` | ffmpeg encoder | Use case |
|---|---|---|
| `x264` | libx264 | Higher-quality H.264 than openh264. |
| `x265` | libx265 | H.265/HEVC. |
| `h264-vaapi` / `h265-vaapi` | h264_vaapi / hevc_vaapi | Linux Intel/AMD GPU. |
| `h264-videotoolbox` / `h265-videotoolbox` | h264_videotoolbox / hevc_videotoolbox | macOS hardware. |
| `h264-qsv` / `h265-qsv` | h264_qsv / hevc_qsv | Intel Quick Sync Video. |

If ffmpeg is not found, candy falls back to the self-contained codecs or returns E007
(`h265`/`x264`/`x265` without ffmpeg).

---

## Error model (E001–E007) {#error-model-e001e007}

All fallible operations return `Result<T, CandyError>`; production code must not panic
(spec §6). `CandyError::code()` maps each variant to a mandatory error code:

| Code | Variant | Meaning |
|---|---|---|
| E001 | `Io` | `.tyx` file not found / generic I/O failure. |
| E002 | `Parse` | Invalid `.tyx` syntax (or non-monotonic `time_ms` in `schedule`). |
| E003 | `Dsl` | `candy-json` missing/invalid (DSL extraction). |
| E004 | `LabelNotFound` | `@label` not found in the Typst layout. |
| E005 | `Interp` | Invalid interpolation range (clamped, non-fatal). |
| E006 | `Typst` | Typst render failure. |
| E007 | `Encode` | Rav1e/openh264 encoding failure. |

---

## Artifacts

- `.candy/<stem>/` — intermediates: `frames.rgba` (raw RGBA bundle),
  `frame_*.svg` (draft frames, also written on encode failure). For **video** builds
  this directory is **removed automatically** after a successful run unless
  `--keep-intermediates` is passed; `--format svg` keeps it (the draft *is* the output).
- `dist/<stem>.<ext>` — final video (MP4 / MKV / WebM), animated GIF (`.gif`), or static
  PNG bitmap of the final frame (`.png`). With `--output-dir <dir>` every one of these is
  redirected into `<dir>/` instead of `dist/`.

---

## Building & features

```sh
# Default build (CPU rasterization via typst-render)
cargo build

# GPU rasterization (vello + wgpu) — heavy native deps
cargo build --features gpu

# Use it
cargo run -- build examples/box_anim.tyx --gpu
```

- The default `system-downloader` feature fetches `@preview` packages from Typst
  Universe at render time (pure-Rust TLS, no OpenSSL). Disable with
  `--no-default-features` for a fully offline build (packages must be pre-cached via
  `typst compile`).
- CI builds for all 10 Rust Tier-1 (with host tools) targets; GPU builds are a separate
  matrix (native only, never cross-compiled). See `.github/workflows/ci.yml`.
