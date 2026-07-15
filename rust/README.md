# Candy Rust Backend

**C**ode-oriented **A**nimation e**N**gine **D**esigned for t**Y**pst — the high-performance
rendering & encoding backend.

> **Full documentation.** The complete backend reference — pipeline, modules, public API,
> timing model, codecs, and error model — lives in
> [`docs/reference/rust-api.md`](../docs/reference/rust-api.md). The user-facing Typst DSL and
> a learn-by-doing tutorial are in [`docs/`](../docs/README.md). This README is a short
> overview; the canonical, maintained reference is the docs page.

## Overview

Candy is a **C**ode-oriented **A**nimation e**N**gine **D**esigned for t**Y**pst. The Rust
crate (`candy`) is the high-performance backend that:

- parses a `.tyx` X-sheet (or an SVG round-trip) into a `Scene` AST,
- schedules & interpolates that AST into per-frame `FrameData`,
- renders each frame **in-process** with the `typst` compiler library (never spawning the
  CLI),
- encodes the frames with self-contained codecs (`rav1e` for AV1, `openh264` for H.264) and
  muxes them into MP4 / Matroska (WebM/MKV) — **no FFmpeg, no external CLI**.

When the system has `ffmpeg` on `$PATH`, Candy can additionally shell out to it for
higher-quality / hardware-accelerated codecs (`x264`, `x265`, `*-vaapi`, `*-videotoolbox`,
`*-qsv`), runtime-detected with no cargo dependency.

The crate follows a **strict layered pipeline**: no circular dependencies, no cross-module
side effects.

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

## Module layout

```
rust/src/
├── main.rs            # CLI (clap): `candy build`
├── lib.rs             # Public API: build/build_input/build_input_with_gpu, Input, OutputFormat
├── core/              # pure data + scheduling / interpolation (no I/O, no render)
│   ├── ast.rs         # Scene, FrameData, Action, Label — the shared data model
│   ├── easing.rs      # Easing enum + resolve()
│   ├── diag.rs        # CandyError (E001–E008) + CandyWarn (W001–W013) + macros
│   ├── interpolator.rs# interpolate / interpolate_with
│   ├── morph.rs       # Flubber port: SVG → polygon rings → morph → path string
│   └── scheduler.rs   # schedule(): Scene → keyframes
├── parser/
│   ├── tyx.rs         # parse_tyx: .tyx → Scene
│   └── dsl.rs         # DSL helper extraction
└── renderer/
    ├── typst.rs       # in-process typst compile/render → SVG | RGBA pixels
    ├── gpu.rs         # (feature "gpu") vello + wgpu GPU rasterization
    ├── video.rs       # encode_frames / mux / collect_audio
    ├── rav1e.rs       # AV1 encoder (pure Rust; all-intra fallback; H.264 fallback)
    ├── h264.rs        # openh264 H.264 encoder
    ├── ffmpeg.rs      # find_ffmpeg / encode_via_ffmpeg
    ├── container.rs   # hand-written MP4 / Matroska / WebM muxers
    └── audio.rs       # Opus/AAC audio demuxers
```

## Public API

The crate exposes `build`, `build_input`, and `build_input_with_gpu` (all returning
`Result<(), CandyError>`), plus the `Input`, `OutputFormat`, and `Codec` enums. Full
signatures, the end-to-end build flow, module details, the timing model, the codec matrix,
and the error model are in
[`docs/reference/rust-api.md`](../docs/reference/rust-api.md).

## Building & features

```sh
# Default build (CPU rasterization via typst-render)
cargo build

# GPU rasterization (vello + wgpu) — heavy native deps
cargo build --features gpu

# Use it
cargo run -- build examples/box_anim.tyx --gpu
```

- The default `system-downloader` feature fetches `@preview` packages from Typst Universe at
  render time (pure-Rust TLS, no OpenSSL). Disable with `--no-default-features` for a fully
  offline build (packages must be pre-cached via `typst compile`).
- CI builds for all 10 Rust Tier-1 (with host tools) targets; GPU builds are a separate matrix
  (native only, never cross-compiled). See `.github/workflows/ci.yml`.

## Documentation

- [Backend reference (full)](docs/reference/rust-api.md) — API, modules, architecture, codecs,
  errors.
- [Tutorial](../docs/tutorial/README.md) — for `.tyx` authors.
- [Reference index](../docs/reference/README.md) — directives, easing, counters, CLI.
