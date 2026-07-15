# Tutorial

This tutorial is for **Typst users who want to make animations**. You do not need to
know Rust — you write a `.tyx` file (standard Typst + a handful of directives) and run
the `candy` CLI to render it.

Each page builds on the previous one. If you just want the full directive list, skip to
the [Reference](../reference/README.md).

## Pages

1. [Your first clip](first-clip.md) — the `.tyx` format, `mobject` / `animate` /
   `pause`, and `candy build`.
2. [Animation basics](animation-basics.md) — transforms (`to` / `dx` / `scale` /
   `rotate` / `opacity`), `pause` / `play`, easing, and the timing model.
3. [Transforms & text](transforms.md) — `#transform`, `#morph`, `#fade-transform`,
   `#reveal`, `#typewriter` (glyph-by-glyph formula morphing).
4. [Scenes, camera & groups](scenes-camera-groups.md) — `#scene`, `#camera`,
   `#group`, `#track`, `#zoom-to`, `#transition`.
5. [Subtitles & counters](subtitles-counters.md) — `#subtitle`, `#ecounter` /
   `#ecval`.
6. [Output & codecs](output.md) — CLI flags, output formats (mp4/mkv/webm/gif/png/svg),
   the codec matrix, and artifacts.

---

## Installing Candy

Candy is a Rust binary. You need a Rust toolchain (≥ the edition used by the crate) to
install or build it.

```sh
# From crates.io (published as `candy-animation`; the binary is `candy`)
cargo install candy-animation

# Or build from the repository
git clone https://github.com/ViCo-Lab/candy
cd candy/rust
cargo build --release
# binary: target/release/candy
```

**Prerequisites**

- A [Rust toolchain](https://rustup.rs) (`cargo`).
- (Optional) [Typst](https://github.com/typst/typst) — only to *preview* the first
  frame of a `.tyx` with `typst compile`. Candy does its own in-process rendering and
  does **not** shell out to the Typst CLI.
- (Optional) `ffmpeg` on `$PATH` — unlocks higher-quality / hardware-accelerated
  codecs (`x264`, `x265`, `*-vaapi`, `*-videotoolbox`, `*-qsv`). Without it, Candy
  uses its self-contained AV1 (rav1e) / H.264 (openh264) encoders.

**Offline builds.** The default `system-downloader` feature fetches `@preview`
packages from Typst Universe at render time (pure-Rust TLS, no OpenSSL). For a fully
offline build, pre-cache packages with `typst compile` and build with
`--no-default-features`.

**GPU rasterization (optional).** For faster rasterization on a GPU, build with
`cargo build --features gpu` and pass `--gpu` at build time. Falls back to CPU if no
adapter is found.

## The `.tyx` format

A `.tyx` file is **valid, standard Typst**. It is short for **TYpst X-sheet** — the
*Typst animation exposure sheet*. You declare animatable **mobjects** and **actions**,
and Candy's pipeline expands them into per-frame Typst documents that are rendered and
(optionally) encoded.

```typst
// dot_move.tyx — valid standard Typst; `candy build` renders the clip.
#import "@preview/candy:0.1.0": *

#mobject("dot", circle(radius: 1cm, fill: blue))
#animate("dot", to: (4cm, 0pt), duration: 30, easing: "linear")
#pause(duration: 15)
```

> **One file, two jobs.** Compiling a `.tyx` with `typst compile` renders the *first
> frame* (every object at its natural placement, every `play` block visible, and
> `animate` / `pause` / `audio` inert). Candy's Rust pipeline reads the *same*
> directives from the source AST and produces the full clip. So a single `.tyx` is
> simultaneously a normal Typst document *and* a Candy animation script — you can
> iterate on layout with `typst compile`, then render with `candy build`.

Continue to [Your first clip](first-clip.md).
