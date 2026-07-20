# Candy Documentation

## License

Licensed under either of

 * Apache License, Version 2.0 ([`LICENSE-APACHE`](../LICENSE-APACHE) or <http://www.apache.org/licenses/LICENSE-2.0>)
 * MIT license ([`LICENSE-MIT`](../LICENSE-MIT) or <http://opensource.org/licenses/MIT>)

at your option.

**C**ode-oriented **A**nimation e**N**gine **D**esigned for t**Y**pst.

Candy turns `.tyx` files — standard Typst documents extended with a small set of
animation directives — into self-contained videos (MP4 / MKV / WebM), animated GIFs,
or static PNG posters. Inspired by 3Blue1Brown's
[Manim](https://github.com/3b1b/manim), with API inspiration from
[tanim](https://github.com/liquidhelium/tanim) and
[kino](https://github.com/aualbert/kino).

The backend is written in Rust and renders **in-process** with the `typst` compiler
library — no CLI invocation. Default codec (`x264`) requires system `ffmpeg`; falls back
to self-contained openh264 (`h264`) when unavailable.

## Where to start

- **New to Candy?** Read the [Tutorial](tutorial/README.md) — it walks you from
  install to your first clip, then through transforms, scenes, subtitles, and output.
- **Looking something up?** Jump to the [Reference](reference/README.md) — the full
  directive list, easing curves, counters, CLI flags, codec matrix, error codes, and
  the Rust API.

## Sections

| Section | Audience | Contents |
|---|---|---|
| [Tutorial](tutorial/README.md) | `.tyx` authors (Typst users) | Install, first clip, animation basics, transforms, scenes/camera/groups, subtitles & counters, output & codecs. |
| [Reference](reference/README.md) | Everyone | Directives, easing, counters, CLI, codecs, error model, Rust API & architecture. |

## Install

```sh
# From crates.io (the published crate is named candy-animation)
cargo install candy-animation

# Or build from source
git clone https://github.com/ViCo-Lab/candy
cd candy/rust && cargo build --release
# the binary is `candy`
```

See [Tutorial · Install](tutorial/README.md#installing-candy) for prerequisites and
offline-build notes.

## License

[MIT License](../LICENSE).
