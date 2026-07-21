# Rust API & architecture

This is the **developer reference** for Candy's Rust rendering & encoding backend (the
`rust/` crate). For the *user-facing* Typst DSL, see [Directives](../reference/directives.md)
and the [Tutorial](../tutorial/README.md).

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

The same flow is reachable from an SVG produced by `@preview/candy`: `Input::Svg` →
`parser::extract_dsl_from_svg` → `Scene`, recovering the embedded `candy-json` block.

## Module layout

```
rust/src/
├── main.rs            # CLI (clap): `candy build` + hidden easter-egg `candy candy`
├── lib.rs             # Public API: build/build_input/build_input_with_gpu, Input, OutputFormat
├── core/              # pure data + scheduling / interpolation (no I/O, no render)
│   ├── ast.rs         # Scene, FrameData, Action, Label — the shared data model
│   ├── easing.rs      # Easing enum + resolve() (named curves + expr:/bezier:)
│   ├── diag.rs        # CandyError (E001–E009) + CandyWarn (W001–W016) + macros
│   ├── interpolator.rs# interpolate / interpolate_with (sampling frames)
│   ├── meta.rs        # never touch this, may explode
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

## Public API

### `build` functions

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
- `build_input` is the same but takes an explicit `Input`, so callers can force the SVG path
  even when the file extension is not `.svg`.
- `build_input_with_gpu` adds a `use_gpu` flag. When `use_gpu` is true **and** the `gpu` cargo
  feature is compiled in, Candy rasterizes each frame on the GPU via vello + wgpu. If the
  feature is off, `use_gpu` is silently ignored (CPU path). If the feature is on but no GPU
  adapter is available, Candy falls back to CPU and emits a warning.

End-to-end, `build_input_with_gpu` performs:

1. `input.parse()` → `Scene` (parser).
2. `scheduler::schedule(&scene)` → keyframes; the timeline is then extended so long-lived
   `subtitle`s / `counter`s that end *after* the last mobject keyframe are still covered (a
   final keyframe is appended at the extended end, holding each target's last state).
3. `interpolator::interpolate_with(keyframes, Linear, fps)` → all frames.
4. `Renderer::with_root(scene, project_root)`; sample times are collected (one per video
   frame).
5. **SVG draft path** (`OutputFormat::Svg`): each frame's SVG is written to
   `.candy/<stem>/frame_*.svg` and the function returns — no video is produced.
6. **Video path**: rasterize frames (parallel via rayon on CPU; serial on GPU), persist an
   `frames.rgba` draft, then encode + mux:
   - `Mp4` / `Mkv` / `Webm`: FFmpeg codecs (`uses_ffmpeg()`, or `H265` with `ffmpeg` present)
     shell out to ffmpeg and bypass Candy's muxer. Self-contained codecs (`Av1`, `H264`) go
     through `rav1e`/`openh264` + Candy's hand-written muxer, with audio collected via
     `collect_audio`. On any encode failure, Candy writes an SVG draft to `.candy/` and
     surfaces the error (`E009` for encode).
   - `Gif`: an animated GIF of every frame (looping), encoded in-process via the pure-Rust
     `gif` crate. The `--codec` flag is ignored.
   - `Png`: a single static RGBA bitmap of the **final** frame (the animation "poster"),
     encoded in-process via the pure-Rust `png` crate. The `--codec` flag is ignored.

### `Input`

```rust
pub enum Input {
    /// A `.tyx` Typst X-sheet (parsed via `parser::parse_tyx`).
    Tyx(PathBuf),
    /// An SVG rendered by `@preview/candy`, containing a `candy-json` block
    /// (parsed via `parser::extract_dsl_from_svg`).
    Svg(PathBuf),
}
```

`Input::parse()` returns the `Scene`; `Input::project_root()` returns the parent directory of
the source file, wired into `Renderer::with_root` so local `#import "file.typ"` calls resolve
relative to the source.

### `OutputFormat`

```rust
pub enum OutputFormat { Svg, Mp4, Mkv, Webm, Gif, Png }
```

`Svg` is a draft written to `.candy/` (never `dist/`). `Mp4` / `Mkv` / `Webm` are video
containers (`Webm` = Matroska with the `webm` doctype). `Gif` is an animated GIF of all frames
(looping); `Png` is a static RGBA bitmap of the final frame. The CLI exposes these via
`--format {mp4,mkv,webm,gif,png,svg}`, plus `--output <name>...` and `--output-dir <dir>`.

### `Codec`

```rust
pub enum Codec {
    Av1, H264, H265,
    X264, X265,
    // VAAPI — Linux only:
    #[cfg(target_os = "linux")]
    H264Vaapi,
    #[cfg(target_os = "linux")]
    H265Vaapi,
    #[cfg(target_os = "linux")]
    Av1Vaapi,
    // VideoToolbox — macOS only:
    #[cfg(target_os = "macos")]
    H264VideoToolbox,
    #[cfg(target_os = "macos")]
    H265VideoToolbox,
    // QSV — Windows only:
    #[cfg(target_os = "windows")]
    H264Qsv,
    #[cfg(target_os = "windows")]
    H265Qsv,
    Vp9, Vp8,
}
```

- `Av1` / `H264` are self-contained (rav1e / openh264). `H265` returns `E009` unless system
  ffmpeg is available (in which case it uses x265).
- `X264` / `X265` / `Vp9` / `Vp8` are ffmpeg-backed (runtime-detected) on every platform.
- The hardware variants `H264Vaapi` / `H265Vaapi` / `Av1Vaapi` (VAAPI, **Linux only**),
  `H264VideoToolbox` / `H265VideoToolbox` (VideoToolbox, **macOS only**) and
  `H264Qsv` / `H265Qsv` (QSV, **Windows only**) are `#[cfg(target_os = "...")]` gated: they
  are only compiled on their native platform, so they are absent from `Codec` — and from the
  `--codec` CLI selection interface / `--help` — elsewhere. `Codec::uses_ffmpeg()` reports
  whether a codec shells out to ffmpeg.
## Core modules

### `core::ast`

The shared data model — the single source of truth across `parser`, `core`, and `renderer`.
Types are immutable after creation (builder-time mutation is confined to the parser).

- `Label(String)` — unique animatable id; `Label::parse("@name")` validates `@[A-Za-z0-9_-]+`
  (without the leading `@`).
- `Action` — an animation applied to a target within a slide. Core transforms: `MoveTo` /
  `MoveBy` (absolute / relative shift), `Scale` / `ScaleBy`, `Rotate` / `RotateBy`, `FadeIn` /
  `FadeOut` / `FadeTo`, `MoveAlongPath`. Manim-style: `SaveState` / `Restore`, `Indicate`,
  `Flash`, `Wiggle`, `SetColor`, `Show` / `Hide` (instantaneous visibility toggles).
- `FrameData` — a sampled transform state at a given `time_ms` for one target.
- `Scene` — the parsed document: items, initial states, actions, subtitles, counters. It also
  carries the **scene tree**: `scenes: Vec<SceneInfo>` (each with `id` / `parent` / `scope` /
  `page_size` / `start_ms` / `end_ms` / `owns_labels`) and an optional `root_scene` index.
  `active_scene_at(time_ms)` returns the deepest scene active at a given frame (the renderer
  uses it to hide parent scenes); `effective_page_pt(id)` resolves a scene's canvas size
  (inheriting from the nearest ancestor that declares one). When `scenes` is empty (no `scene`
  call), the whole document is one implicit scene — preserving v0.1 behavior.

### `core::scheduler`

`schedule(scene) -> Result<Vec<FrameData>, CandyError>` turns the `Scene` AST into keyframe
`FrameData`. Each animatable item gets a frame-0 default keyframe (seeded from `scene.initial`)
and a final keyframe at the last frame. A non-monotonic `time_ms` for a target returns
`CandyError::Parse` (E002) instead of panicking.

### `core::interpolator`

`interpolate_with(keyframes, method, fps)` samples the keyframes into all output frames (one
per video frame) using the per-action `Easing`. `InterpMethod::Linear` is the default.
Out-of-range interpolation is clamped (emits `W016`, non-fatal).

### `core::easing`

The `Easing` enum + `Easing::resolve()`. Supports named curves (`linear`, `smooth`,
`cubic-in-out`, `there-and-back`, `wiggle`, `lingering`, …) and custom specs `expr:<math>` and
`bezier:x1,y1,x2,y2`. Unknown names fall back to `linear`.

### `core::morph`

Flubber's polygon-morph algorithm, ported to Rust:

1. Render the target mobject to SVG and `extract_rings_from_svg()` — extract `<rect>`,
   `<circle>`, `<ellipse>`, `<polygon>`, `<polyline>`, and `<path d="...">` into polygon rings.
2. `interpolate_ring()` equalizes point counts, finds the best cyclic alignment (O(n²)), and
   interpolates index-by-index.
3. `ring_to_path_string()` converts the morphed ring back to a path string for Typst.

Glyph outlines use **de Casteljau subdivision** (`flatten_quad` / `flatten_cubic`) to flatten
quadratic/cubic Bézier curves into polygon points, enabling true point-by-point morphing of
formula characters.

### `core::diag`

Unified diagnostics. All diagnostic output flows through this module's macros (`error!` /
`warn!` / `debug!` / `info!`); see [Errors](errors.md). (The module was renamed from
`core::error` to `core::diag`.) The level prefixes are colorized on a TTY — `error` red, `warn`
yellow, `info` green, `debug` dim — via the `colored` crate; output falls back to plain text
when the stream is not a terminal or `NO_COLOR` (https://no-color.org) is set.

## Parser modules

- `parser::parse_tyx` — scans a `.tyx` file (valid standard Typst) and builds the `Scene` AST
  via import analysis. Because the parser resolves the *binding* (not the literal `candy.`
  prefix), both `#import "candy": *` + `mobject(...)` and `#import "candy"` + `candy.mobject(...)`
  work.
- `parser::extract_dsl_from_svg` — recovers the `candy-json` block embedded in an SVG rendered
  by `@preview/candy`, reconstructing the `Scene` (the Typst → SVG → candy round-trip).
- `parser::dsl` — shared DSL helper extraction.

## Renderer modules

- `renderer::typst::Renderer` — compiles each mobject/per-frame document **in-process** with the
  `typst` crate library (the CLI is never spawned). `render_frame_at` produces SVG (draft);
  `render_frame_pixels_par` produces RGBA8 pixels (data-parallel via rayon).
  `ensure_flow_public()` pre-computes the flow layout once so the parallel loop can share
  the `WorldState` via `Arc`.
  - **Per-glyph `#transform`** (inline content): `build_transform_fragments` renders the whole
    old and new bodies to SVG and `extract_formula` pulls every glyph/decoration out as a
    positioned fragment via Typst's *own* SVG layout. Old↔new fragments are matched by outline
    signature via LCS into `GlyphAnim`s; during the window matched fragments glide, removed ones
    fade/slide out, and inserted ones fade/slide in. `tokenize_math` keeps fractions (`a/b`,
    `\frac{a}{b}`) intact so the bar renders correctly. Shape transforms fall back to the
    crossfade + scale morph.
- `renderer::gpu` (feature `gpu`) — `GpuRenderer` rasterizes frames on the GPU via vello + wgpu.
  Serial (single device); falls back to CPU if no adapter is found.
- `renderer::video` — `encode_frames` (rav1e/openh264), `mux` (hand-written MP4/Matroska),
  `collect_audio`, plus `Codec` / `Container` / `EncodedVideo`. Also `write_rgba_draft` for the
  `.candy` intermediates.
- `renderer::rav1e` — AV1 encoder (pure Rust). On the known rav1e 0.8.1 inter-prediction panic it
  automatically retries in all-intra mode, then falls back to H.264 (the panic is
  `catch_unwind`-guarded so the command never aborts).
- `renderer::h264` — openh264 software H.264 encoder (linked `libopenh264`).
- `renderer::ffmpeg` — `find_ffmpeg` and `encode_via_ffmpeg` pipe raw RGBA frames to ffmpeg's
  stdin and read back the muxed bytes. Hardware encoders upload RGBA to a `nv12` hardware surface
  with codec-appropriate rate control.
- `renderer::container` — hand-written MP4 / Matroska / WebM muxers. (Note: the EBML vint encoder
  must write multi-byte vints in **forward** byte order, or ffmpeg decoders misread element
  lengths and SimpleBlocks overflow their cluster.)
- `renderer::audio` — Opus (`.opus`/`.ogg`, for WebM/MKV) and AAC (`.aac`, for MP4) demuxers,
  feeding `collect_audio`.

## Timing model

All timing is in **milliseconds** (not frames). `--fps` controls only the output frame rate: a
1000 ms slide at 30 fps ≈ 30 frames, at 60 fps ≈ 60 frames — same wall-clock duration.
`#subtitle` and `#ecnew` lifetimes are given directly in ms; `#animate` / `#pause` / `#play`
durations are all in milliseconds.

## Artifacts

- `.candy/<stem>/` — intermediates: `frames.rgba` (raw RGBA bundle), `frame_*.svg` (draft frames,
  also written on encode failure). For **video** builds this directory is **removed
  automatically** after a successful run unless `--keep-intermediates` is passed; `--format svg`
  keeps it (the draft *is* the output).
- `dist/<stem>.<ext>` — final video (MP4 / MKV / WebM), animated GIF (`.gif`), or static PNG
  bitmap of the final frame (`.png`). With `--output-dir <dir>` every one of these is redirected
  into `<dir>/` instead of `dist/`.

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

## Scene model (design notes)

A `scene` is a special `page`. In standard Typst, `#scene(body)` wraps `body` in a `page()`
call, so each scene renders as an independent page. The Rust renderer treats each scene as an
independent animation segment. The scene tree is a parsed `Vec<SceneInfo>` on the `Scene` AST
(with an optional `root_scene` index), built by `parser::parse_tyx` from nested `scene` calls.
Semantics enforced by the pipeline:

- **Nesting** — scenes may nest; a `scene` inside another scene's body becomes a child
  `SceneInfo` (`parent` links form the tree).
- **Parent auto-hide** — `Scene::active_scene_at(time_ms)` returns the *deepest* scene whose
  `[start_ms, end_ms]` interval contains the frame time (falling back to the root scene). The
  renderer filters mobjects by `label_scene[label] == active`, so a child scene automatically
  hides its parent.
- **Typst scope** — membership follows Typst's lexical scope: every mobject / `play` / transform
  is attributed to `ctx.current_scene` (the innermost enclosing scene) via `ctx.label_scene`.
- **Per-page canvas** — a scene's `page_size` defines the size of *each* page in that scene.
  `Scene::effective_page_pt(scene_id)` inherits the size from the nearest ancestor that declares
  one, then the 16cm × 9cm default.
- **Cross-page scene** — content overflowing a scene's page spills onto subsequent pages. The
  mobjects stay in **one** scene (data shared), but are laid out across the overflow pages and the
  canvas is the vertical stack of those pages in page order, so nothing is clipped off a single
  page and the scene is *not* split into separate sub-scenes. `ensure_flow()` reads every page
  of the natural-layout pass and offsets each mobject's natural y by `k * page_h` (page index `k`).
- **Implicit root** — when `scenes` is empty (no `scene` call), the whole document is one implicit
  scene (id `0`) whose page is the root page size; this path is backward-compatible with v0.1.

Backward compatibility: legacy `.tyx` files with no `scene` calls produce an empty `scenes`
vector, and every renderer path falls back to treating the whole document as a single scene — so
v0.1 behavior is preserved.
