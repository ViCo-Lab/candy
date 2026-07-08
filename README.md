# Candy

**C**ode-oriented **A**nimation **N**gine **D**esigned for **T**ypst

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
- Familiar appearance for Manim users.

## The `.tyx` format (Typst X-sheet)

Animations are authored in **`.tyx`** files — short for **TYpst X-sheet**,
the *Typst animation exposure sheet*. A `.tyx` file is standard Typst extended
with Candy's animation directives. Instead of manually laying out pages, you
declare animatable **mobjects** and **actions**, and Candy's pipeline expands
them into per-frame Typst documents that are rendered and (optionally) encoded.

```typst
// dot_move.tyx — valid standard Typst; candy build renders the clip.
#import "candy": *

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

The `@preview/candy` Typst package (the `typst/` directory) exposes this DSL.
Each directive is *valid, standard Typst*: `typst compile` renders the first
frame; `candy build` renders the full clip by reading the AST directly.

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
# Default: AV1 in an MP4 container, written to dist/<stem>.mp4
cargo run -- build examples/dot_move.tyx

# AV1 in WebM (Matroska with webm doctype)
cargo run -- build examples/dot_move.tyx --format webm

# H.264 in MP4 (fallback when rav1e is unavailable)
cargo run -- build examples/dot_move.tyx --format mp4 --codec h264

# SVG draft (one file per frame, written to .candy/<stem>/)
cargo run -- build examples/dot_move.tyx --format svg
```

### Flags

| Flag | Default | Description |
|---|---|---|
| `<input>` (positional) | required | Path to the `.tyx` X-sheet. |
| `-o, --output` | `out` | Output name hint under `dist/` for videos; ignored for SVG drafts. |
| `--format` | `mp4` | `mp4` / `mkv` / `webm` / `svg` (SVG draft → `.candy/`). |
| `--codec` | `av1` | `av1` (preferred) / `h264` / `h265` (returns E007). |
| `--fps` | `30` | Frames per second (video path). |
| `-p, --pixel-per-pt` | `2.0` | Rasterization resolution (pixels per Typst point). |

### Artifacts

- `.candy/<stem>/` — intermediates: `frames.rgba` (raw RGBA bundle),
  `frame_*.svg` (draft frames, also written on encode failure).
- `dist/<stem>.<ext>` — final video (MP4 / MKV / WebM).

> **Note on encoding fallback:** `rav1e` 0.8 can panic on certain frame
> geometries; candy wraps the encoder in `catch_unwind` and falls back to
> H.264 automatically. If both encoders fail, candy writes an SVG draft to
> `.candy/` and surfaces E007 — the command never aborts without producing
> *some* output.

## Project status

This is the first usable version (spec v0.1.0, "Orange Candy"). The `core`
scheduling/interpolation and the `parser` DSL scanner are complete and tested;
the `renderer` compiles per-frame Typst in-process.

Known v0.1 limitations:

- `#mobject` bodies must be valid standalone Typst — the in-process `World`
  provides no `file()`/`font()`/`source()` for non-main ids, so package
  imports inside mobject bodies cannot be resolved. Workaround: keep mobject
  bodies self-contained, or render via the SVG draft and import manually.
- HEVC/H.265 is not supported (no pure-Rust encoder; returns E007).

## License

[MIT License](LICENSE)
