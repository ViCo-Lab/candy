# Candy

**C**ode-oriented **A**nimation **N**gine **D**esigned for **T**ypst

Candy is an animation engine for Typst, using Rust as a high-performance rendering
and encoding backend. Inspired by 3Blue1Brown's
[Manim](https://github.com/3b1b/manim), with API inspiration from
[tanim](https://github.com/liquidhelium/tanim) and
[kino](https://github.com/aualbert/kino).

## Features

- High-performance rendering powered by the Rust [`typst`](https://crates.io/crates/typst) crate — **in-process, no CLI invocation**
- Code-oriented animation creation, written directly in Typst
- Self-contained AV1 encoding via [`rav1e`](https://crates.io/crates/rav1e) — **no FFmpeg dependency**
- Familiar appearance for Manim users

## The `.tyx` format (Typst X-sheet)

Animations are authored in **`.tyx`** files — short for **TYpst X-sheet**,
the *Typst animation exposure sheet*. A `.tyx` file is standard Typst extended
with Candy's animation directives. Instead of manually laying out pages, you
declare animatable **items** and **actions**, and Candy's pipeline expands them
into per-frame Typst documents that are rendered and (optionally) encoded.

```typst
// dot_move.tyx
#candy.init()
#candy.animate(duration: 30)[
  #candy.action(MoveTo(@dot, to: (4cm, 0)))
]
#circle(radius: 1cm, fill: blue)
#candy.finish
```

- `#candy.item("name", <body>)` — declare an animatable item (optional; without
  it, the rest of the source becomes the body of a synthetic item keyed by the
  action's `@label`).
- `#candy.animate(duration: N)[ ... ]` — a slide of `N` frames.
- `#candy.action(...)` — `MoveTo` / `Scale` / `FadeIn` / `FadeOut` on a `@label`.

The `@preview/candy` Typst package (the `typst/` directory) exposes this DSL for
use inside normal Typst documents; it embeds the resulting `Scene` as a hidden
`candy-json` block so the Rust backend can recover it from a rendered SVG.

## Architecture

Strict layered pipeline (no circular deps, no cross-module side effects):

```
.tyx ─▶ parser::parse_tyx ─▶ Scene (AST)
                         │
                         ▼
        core::scheduler::schedule ─▶ keyframes (Vec<FrameData>)
                         │
                         ▼
      core::interpolator::interpolate ─▶ all frames (Vec<FrameData>)
                         │
                         ▼
   renderer::typst::Renderer ─▶ SVG (default) │ renderer::rav1e ─▶ AV1/IVF
```

- **`rust/`** — the backend, organized as `core` (pure data + scheduling/
  interpolation), `parser` (`.tyx` → `Scene`, and SVG → `Scene`), and
  `renderer` (in-process `typst` compile/render + `rav1e` AV1 encoding).
  - Rendering uses the [`typst`](https://crates.io/crates/typst) crate library
    **in-process** — the `typst` CLI is never spawned.
  - Encoding uses [`rav1e`](https://crates.io/crates/rav1e) — **no FFmpeg**.
- **`typst/`** — the user-facing package (function signatures) published to
  [Typst Universe](https://typst.app/universe). It defines the animation API;
  the Rust backend does the rendering.

## Usage

Build a `.tyx` X-sheet. SVG (default) writes one file per frame into a
directory; `webm` rasterizes and encodes to AV1 (`*.ivf`).

```sh
# SVG sequence (one file per frame)
cargo run -- build examples/dot_move.tyx -o frames --format svg

# AV1 / IVF video
cargo run -- build examples/dot_move.tyx -o out.ivf --format webm --fps 30 -p 2
```

- `-o, --output` — directory for `--format svg`, file for `--format webm`
- `--format` — `svg` (default) or `webm` (AV1/IVF; see note below)
- `--fps` — frames per second (default `30`, video path)
- `-p, --pixel-per-pt` — rasterization resolution, pixels per Typst point (default `2`)

> **Note on "WebM":** the spec labels the video output "WebM", but `rav1e`
> produces AV1 elementary/IVF and does not mux WebM/Matroska. The `webm` output
> is therefore an AV1 stream in an IVF container; a proper WebM muxer is a
> reserved future extension.
>
> The `rav1e` encoder is gated behind the `video` cargo feature (off by
> default) because `rav1e` 0.8.1 can panic on certain frame geometries
> (unrecoverable inside a worker thread). Without it, `candy build --format webm`
> returns **E007** and falls back to writing the SVG sequence (spec §6), so the
> command never aborts. Enable it with `cargo run --features video -- build … --format webm`.

## Project status

This is the first usable version (spec v0.1.0, "Orange Candy"). The `core`
scheduling/interpolation and the `parser` DSL scanner are complete and tested;
the `renderer` compiles per-frame Typst in-process. Known v0.1 limitations:
opacity is tracked in `FrameData` but not yet applied to Typst output, and
`#candy.item` bodies must be valid standalone Typst (no package imports in the
engine's minimal `World`).

## License

[MIT License](LICENSE)
