# Candy

**C**ode-oriented **A**nimation e**N**gine **D**esigned for t**Y**pst

Candy is an animation engine for Typst, using Rust as a high-performance rendering
and encoding backend. Inspired by 3Blue1Brown's
[Manim](https://github.com/3b1b/manim), with API inspiration from
[tanim](https://github.com/liquidhelium/tanim) and
[kino](https://github.com/aualbert/kino).

## Features

- High-performance rendering powered by the Rust [`typst`](https://crates.io/crates/typst) crate — **in-process, no CLI invocation**.
- Code-oriented animation creation, written directly in Typst.
- Self-contained video encoding via [`rav1e`](https://crates.io/crates/rav1e) (AV1) and [`openh264`](https://crates.io/crates/openh264) (H.264) — **no FFmpeg, no external codec CLI**.
- Hand-written MP4 / Matroska / WebM muxers in pure Rust.
- Audio muxing for Opus (`.opus`/`.ogg` → MKV/WebM) and AAC (`.aac` → MP4).
- Smooth **object transitions** (Manim-style `Transform`): morph a mobject into
  new inline content — including **formulas** — via `#transform`, keeping the
  original label reusable; `#morph` / `#fade-transform` crossfade two mobjects.
- Familiar appearance for Manim users.

## The `.tyx` format (Typst X-sheet)

Animations are authored in **`.tyx`** files — short for **TYpst X-sheet**,
the *Typst animation exposure sheet*. A `.tyx` file is standard Typst extended
with Candy's animation directives. Instead of manually laying out pages, you
declare animatable **mobjects** and **actions**, and Candy's pipeline expands
them into per-frame Typst documents that are rendered and (optionally) encoded.

```typst
// dot_move.tyx — valid standard Typst; candy build renders the clip.
#import "@preview/candy:0.1.0": *

#mobject("dot", circle(radius: 1cm, fill: blue))
#animate("dot", to: (4cm, 0pt), duration: 30, easing: "linear")
#pause(duration: 15)
#audio("voice.opus", blocking: false, loop: false, volume: 0.9, slice: none)
```

- `#mobject(label, body)` — declare an animatable object (its position is taken
  automatically from where `body` lands in the document flow).
- `#animate(target, to:.., scale:.., opacity:.., duration:.., easing:..)` —
  animate an object to a new placement / scale / opacity over `duration` frames.
- `#pause(duration: N)` — hold the current frame for `N` frames.
- `#audio(path, blocking:.., loop:.., volume:.., slice:..)` — attach a voice
  / audio track (`.opus`/`.ogg` for WebM/MKV, `.aac` for MP4).
- `#play(body, duration: N)` — show `body` for `N` frames as its own
  animation unit (block-level, controllable like a mobject).
- `#transform(target, to: <content>, duration: N, easing:..)` — Manim-style
  `Transform` / `ReplacementTransform`: smoothly morph `target`'s content into
  the new inline `content` (a shape or a formula such as `[$a + b + d = c$]`);
  the original `target` label keeps the new content, so you can keep animating
  it. See `examples/transform_demo.tyx`.
- `#morph(from, to, duration: N, easing:..)` / `#fade-transform(from, to, ..)` —
  crossfade two pre-registered mobjects (simplified `Transform` variant).

The `@preview/candy` Typst package (the `typst/` directory) exposes this DSL.
Each directive is *valid, standard Typst*: `typst compile` renders the first
frame (no animation); `candy build` renders the full clip by reading the AST directly.

## Architecture

Strict layered pipeline (no circular deps, no cross-module side effects):

```text
.tyx ─▶ parser::parse_tyx ─▶ Scene (AST, valid standard Typst)
                         │
                         ▼
        core::scheduler::schedule ─▶ keyframes (Vec<FrameData>)
                         │
                         ▼
      core::interpolator::interpolate ─▶ all frames (Vec<FrameData>)
                         │
                         ▼
   renderer::typst::Renderer ─▶ SVG (draft) │ RGBA pixels
                         │
                         ▼
   renderer::video ─▶ AV1 (rav1e) / H.264 (openh264) ─▶ MP4 / MKV / WebM
```

- **`rust/`** — the backend, organized as `core` (pure data + scheduling /
  interpolation), `parser` (`.tyx` → `Scene`, and SVG → `Scene`), and
  `renderer` (in-process `typst` compile/render + `rav1e`/`openh264` encoding
  + hand-written MP4/Matroska muxers + Opus/AAC audio demuxers).
  - Rendering uses the [`typst`](https://crates.io/crates/typst) crate library
    **in-process** — the `typst` CLI is never spawned.
  - Encoding uses [`rav1e`](https://crates.io/crates/rav1e) (AV1) and
    [`openh264`](https://crates.io/crates/openh264) (H.264). HEVC is not
    supported (returns E007); a pure-Rust HEVC encoder is not yet available.
- **`typst/`** — the user-facing package (function signatures) published to
  [Typst Universe](https://typst.app/universe). It defines the animation API;
  the Rust backend does the rendering.

## Usage

```sh
# Default: H.264 in an MP4 container, written to dist/<stem>.mp4
candy build examples/dot_move.tyx

# AV1 in WebM (Matroska with webm doctype)
candy build examples/dot_move.tyx --format webm --codec av1

# H.264 in MP4 (default self-contained codec; opt into AV1 with --codec av1)
candy build examples/dot_move.tyx --format mp4 --codec h264

# SVG draft (one file per frame, written to .candy/<stem>/)
candy build examples/dot_move.tyx --format svg

# Build from an SVG rendered by @preview/candy (candy-json round-trip)
candy build scene.svg --from-svg --format mp4
```

**When Debugging, use `cargo run -- <args>` instead of `candy <args>`.**

### Flags

| Flag | Default | Description |
|---|---|---|
| `<input>` (positional) | required | Path to the `.tyx` X-sheet, or an SVG with a `candy-json` block (see `--from-svg`). |
| `--from-svg` | off | Force the input to be parsed as an SVG rendered by `@preview/candy`. Without this flag, the parser is selected by file extension (`.svg` → SVG round-trip, anything else → `.tyx`). |
| `-o, --output` | `out` | Output name hint under `dist/` for videos; ignored for SVG drafts. |
| `--format` | `mp4` | `mp4` / `mkv` / `webm` / `svg` (SVG draft → `.candy/`). |
| `--codec` | `h264` | `av1` / `h264` / `h265` / `x264` / `x265` / `h264-vaapi` / `h265-vaapi` / `h264-videotoolbox` / `h265-videotoolbox` / `h264-qsv` / `h265-qsv`. The first three are self-contained (rav1e/openh264); the rest shell out to system ffmpeg (runtime-detected, no cargo dep). See [Codecs](#codecs). |
| `-f, --fps` | `30` | Frames per second (video path). |
| `-p, --pixel-per-pt` | `2.0` | Rasterization resolution (pixels per Typst point). |
| `--gpu` | off | Use GPU rasterization (vello + wgpu) for the video path. Requires `cargo build --features gpu`. Falls back to CPU if the feature is off or no GPU adapter is available. |
| `--keep-intermediates` | off | Keep the `.candy/<stem>/` intermediate directory after a successful build (e.g. `frames.rgba`). By default candy deletes it once the final video is written. Has no effect on `--format svg`. |

### Artifacts

- `.candy/<stem>/` — intermediates: `frames.rgba` (raw RGBA bundle),
  `frame_*.svg` (draft frames, also written on encode failure). For video
  builds this directory is **removed automatically** after a successful run
  unless `--keep-intermediates` is passed; `--format svg` keeps it (that draft
  *is* the output).
- `dist/<stem>.<ext>` — final video (MP4 / MKV / WebM).

### GPU rasterization (optional)

Candy's default rasterizer is `typst-render` (CPU, pure Rust). For faster
rasterization on systems with a GPU, candy can use [vello](https://crates.io/crates/vello)
(GPU compute 2D renderer) + [wgpu](https://crates.io/crates/wgpu). This is
opt-in because it pulls in heavy native GPU dependencies:

```sh
# Build with GPU support
cargo build --features gpu

# Use GPU rasterization (falls back to CPU if no GPU adapter is found)
cargo run --features gpu -- build examples/box_anim.tyx --gpu
```

The GPU path produces frames in the same RGBA8 format as the CPU path, so the
downstream video encoder (rav1e/openh264) consumes them unchanged. GPU
rasterization is most beneficial at high resolutions (`-p 4` or above) where
CPU rasterization becomes the bottleneck.

### Codecs

Candy ships two **self-contained** video encoders (no system dependencies):

| `--codec` | Encoder | Container | Notes |
|---|---|---|---|
| `h264` (default) | openh264 (linked libopenh264) | MP4/MKV/WebM | Software H.264; falls back to AV1 if openh264 fails. |
| `av1` | rav1e (pure Rust) | MP4/MKV/WebM | Tries full-quality AV1 (inter-prediction); if rav1e 0.8.1 panics on the frame geometry it automatically retries in all-intra mode, then falls back to H.264. |
| `h265` | — | — | Self-contained build returns E007; with system ffmpeg, uses x265. |

When the system has **`ffmpeg`** on `$PATH`, candy can shell out to it for
additional codecs — no cargo dependency, runtime-detected. This enables
hardware-accelerated encoding and higher-quality software codecs:

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

The ffmpeg path pipes raw RGBA frames to ffmpeg's stdin and writes the muxed
container to a unique temp file (ffmpeg muxers require a seekable output), then
reads the bytes back. Hardware encoders (VAAPI / VideoToolbox / QSV) upload the
RGBA frames to a hardware surface (`format=nv12,hwupload`) and use
codec-appropriate rate control. If ffmpeg is not found, candy falls back to the
self-contained codecs (av1/h264) or returns E007 (h265/x264/x265 without ffmpeg).

> **Note on encoding fallback:** `rav1e` 0.8.1 (the latest published release)
> can panic during inter-prediction on certain frame geometries. Candy first
> tries full-quality AV1, and on that panic automatically retries in all-intra
> mode (valid AV1, no temporal compression); only if that also fails does it
> fall back to H.264. The panic is caught (`catch_unwind`) so the command never
> aborts — if every encoder fails, candy writes an SVG draft to `.candy/` and
> surfaces E007.

## Project status

This is the first usable version (v0.1.0, "Ribose"). The `core`
scheduling/interpolation and the `parser` DSL scanner are complete and tested;
the `renderer` compiles per-frame Typst in-process.

Known v0.1 limitations:

- HEVC/H.265 self-contained encoding is not supported (no pure-Rust encoder);
  use `--codec h265` with system ffmpeg, or `--codec av1` (default).
- The `system-downloader` feature (default on) fetches `@preview` packages
  from Typst Universe at render time. Disable with `--no-default-features`
  for a fully offline build (packages must then be pre-cached via
  `typst compile`).

## Documentation

- [`typst/README.md`](typst/README.md) — the user-facing Typst DSL reference (every
  directive, easing, and counter, with worked examples).
- [`rust/README.md`](rust/README.md) — the Rust backend developer reference (pipeline,
  modules, public API, codecs, error model).
- [`rust/docs/architecture.md`](rust/docs/architecture.md) — architecture & design notes.

See [`examples/`](examples) for runnable `.tyx` X-sheets (the
[Typst doc](typst/README.md#worked-examples) lists what each one demonstrates).

## License

[MIT License](LICENSE)
