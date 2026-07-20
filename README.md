# Candy

**C**ode-oriented **A**nimation e**N**gine **D**esigned for t**Y**pst

Candy is an animation engine for Typst, using Rust as a high-performance rendering
and encoding backend. Inspired by 3Blue1Brown's
[Manim](https://github.com/3b1b/manim), with API inspiration from
[tanim](https://github.com/liquidhelium/tanim) and
[kino](https://github.com/aualbert/kino).

> **Documentation has moved.** The full, restructured docs live in [`docs/`](docs/README.md)
> — split into a [Tutorial](docs/tutorial/README.md) (learn by doing) and a
> [Reference](docs/reference/README.md) (look things up). This README is a short project
> overview; the package-specific READMEs are [`typst/README.md`](typst/README.md) (user DSL)
> and [`rust/README.md`](rust/README.md) (backend API).

## Features

- High-performance rendering powered by the Rust [`typst`](https://crates.io/crates/typst) crate — **in-process, no CLI invocation**.
- Code-oriented animation creation, written directly in Typst.
- Self-contained video encoding via [`rav1e`](https://crates.io/crates/rav1e) (AV1) and [`openh264`](https://crates.io/crates/openh264) (H.264) — **no FFmpeg, no external codec CLI**.
- Hand-written MP4 / Matroska / WebM muxers in pure Rust.
- **Animated GIF and static PNG output** (pure-Rust `gif` / `png`) for quick previews and posters — no codec, no container.
- Audio muxing for Opus (`.opus`/`.ogg` → MKV/WebM) and AAC (`.aac` → MP4).
- Smooth **object transitions** (Manim-style `Transform`): morph a mobject into new inline content — including **formulas** — via `#transform`, keeping the original label reusable. For inline content the transform is glyph-by-glyph; `#morph` / `#fade-transform` crossfade two mobjects.
- **Progressive text reveal** (`#reveal` / `#typewriter`) that types a string mobject in word- or character-by-word while its layout box stays reserved.
- **Groups & camera**: `#group` several mobjects to move/scale/rotate them as one, and `#camera` for global pan / zoom / rotate "tours"; `#track` drives a target through multi-property keyframes.
- Familiar appearance for Manim users.

## The `.tyx` format (Typst X-sheet)

Animations are authored in **`.tyx`** files — short for **TYpst X-sheet**, the *Typst
animation exposure sheet*. A `.tyx` file is standard Typst extended with Candy's animation
directives. Instead of manually laying out pages, you declare animatable **mobjects** and
**actions**, and Candy's pipeline expands them into per-frame Typst documents that are
rendered and (optionally) encoded.

```typst
// dot_move.tyx — valid standard Typst; candy build renders the clip.
#import "@preview/candy:0.1.0": *

#mobject("dot", circle(radius: 1cm, fill: blue))
#animate("dot", to: (4cm, 0pt), duration: 1000, easing: "linear")
#pause(duration: 500)
#audio("voice.opus", blocking: false, loop: false, volume: 0.9, slice: none)
```

The `@preview/candy` Typst package (the `typst/` directory) exposes this DSL. Each directive
is *valid, standard Typst*: `typst compile` renders the first frame (no animation); `candy
build` renders the full clip by reading the AST directly.

## Install

```sh
# From crates.io (published as candy-animation; the binary is `candy`)
cargo install candy-animation

# Or build from source
git clone https://github.com/ViCo-Lab/candy
cd candy/rust && cargo build --release
```

See [Tutorial · Install](docs/tutorial/README.md#installing-candy) for prerequisites and
offline-build notes.

## Documentation

- [`docs/README.md`](docs/README.md) — documentation index.
- [`docs/tutorial/`](docs/tutorial/README.md) — learn-by-doing: install, first clip,
  animation basics, transforms, scenes/camera/groups, subtitles & counters, output & codecs.
- [`docs/reference/`](docs/reference/README.md) — lookup: directives, easing, counters, CLI,
  codecs, error model, and the Rust API & architecture.
- [`typst/README.md`](typst/README.md) — the user-facing Typst DSL reference (also shown on
  Typst Universe).
- [`rust/README.md`](rust/README.md) — the Rust backend developer reference (also on crates.io).

See [`examples/`](examples) for runnable `.tyx` X-sheets (the
[Tutorial · Output](docs/tutorial/output.md) and
[Typst README worked examples](typst/README.md#worked-examples) list what each one
demonstrates).

## License

Licensed under either of

 * Apache License, Version 2.0 ([`LICENSE-APACHE`](LICENSE-APACHE) or <http://www.apache.org/licenses/LICENSE-2.0>)
 * MIT license ([`LICENSE-MIT`](LICENSE-MIT) or <http://opensource.org/licenses/MIT>)

at your option.
