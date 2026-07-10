# Candy — Architecture & Design Documentation

## Overview

Candy is a **C**ode-oriented **A**nimation e**N**gine **D**esigned for t**Y**pst.
It turns `.tyx` files (valid Typst documents with candy directives) into
encoded videos (MP4/MKV/WebM) using an in-process Typst compiler and
self-contained video encoders.

## Core Pipeline

```
.tyx ─▶ parser::parse_tyx ─▶ Scene (AST, valid standard Typst)
                         │
                         ▼
        core::scheduler::schedule ─▶ keyframes (Vec<FrameData>, ms-based)
                         │
                         ▼
      core::interpolator::interpolate ─▶ all frames (Vec<FrameData>)
                         │
                         ▼
   renderer::typst::Renderer ─▶ pixel frames (parallel via rayon)
                         │
                         ▼
   renderer::video ─▶ AV1 (rav1e) / H.264 (openh264) / ffmpeg ─▶ MP4 / MKV / WebM
```

## Timing Model

All timing is in **milliseconds** (not frames). The `--fps` CLI flag controls
only the output video's frame rate (how many frames per second are rasterized
and encoded). A 1000ms slide at 30fps produces ~30 frames; at 60fps it produces
~60 frames — the wall-clock duration is the same.

## Scene Model

A `scene` is a special `page`. In standard Typst, `#scene(body)` wraps `body`
in a `page()` call, so each scene renders as an independent page. The Rust
renderer treats each scene as an independent animation segment — rendering one
scene never touches the content of another.

## Morph Architecture

Morph uses Flubber's algorithm, ported to Rust in `core/morph.rs`:

1. **Render target to SVG**: the Typst renderer compiles each mobject to SVG
   via `typst-svg`.
2. **Extract polygon rings**: `extract_rings_from_svg()` walks the SVG byte
   stream and extracts `<rect>`, `<circle>`, `<ellipse>`, `<polygon>`,
   `<polyline>`, and `<path d="...">` elements into polygon rings.
3. **Flubber morph**: `interpolate_ring()` equalizes point counts, finds the
   best cyclic alignment (O(n²)), and interpolates index-by-index.
4. **Feed back to Typst**: `ring_to_path_string()` converts the morphed ring
   back to an SVG path string, which Typst renders per frame.

### Bezier Curve Support

Glyph outlines use **de Casteljau subdivision** to flatten quadratic and cubic
Bezier curves from `ab_glyph::OutlineCurve` into polygon points:
- `flatten_quad(p0, p1, p2, ring, depth)`: 2^depth segments per curve
- `flatten_cubic(p0, p1, p2, p3, ring, depth)`: 2^depth segments per curve

This enables morphing of formula characters — each glyph is extracted as a
true outline polygon (not a bounding box), so `morph("a", "b")` produces a
smooth point-by-point transformation of the letter shapes.

### Custom Easing

The easing system supports:
- Named curves: `"linear"`, `"smooth"`, `"cubic-in-out"`, `"there-and-back"`,
  `"wiggle"`, `"lingering"`, etc.
- `keyframe::functions::*` delegation for standard ease curves.
- Custom curves can be added by extending the `Easing` enum and
  `Easing::resolve()`.

## CI / Multi-Architecture Builds

The CI pipeline builds candy for 8 Rust Tier-1 non-wasm targets:

| Target | OS | Method |
|---|---|---|
| `x86_64-unknown-linux-gnu` | Ubuntu | Native |
| `aarch64-unknown-linux-gnu` | Ubuntu | Cross |
| `x86_64-apple-darwin` | macOS Intel | Native |
| `aarch64-apple-darwin` | macOS ARM | Native |
| `x86_64-pc-windows-msvc` | Windows | Native |
| `aarch64-pc-windows-msvc` | Windows ARM | Cross |
| `i686-unknown-linux-gnu` | Linux 32-bit | Cross |
| `armv7-unknown-linux-gnueabihf` | Raspberry Pi | Cross |

Each target gets its own job and separate artifact (`candy-<target>`).
Build caching via `actions/cache@v4` (keyed by target + `Cargo.lock`).

**Note**: CI workflow updates require a PAT with `workflow` scope. The
updated CI script is at `.github/workflows/ci.yml` in the repository.
