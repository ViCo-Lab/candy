# Reference

Lookup-oriented documentation. For a guided introduction, see the
[Tutorial](../tutorial/README.md).

## Pages

- [Directives](directives.md) — every `#directive`, its arguments, and what it does.
- [Easing](easing.md) — named curves and custom `expr:` / `bezier:` specs.
- [Counters](counters.md) — `#ecounter` / `#ecval` and counter control.
- [CLI](cli.md) — `candy build` flags, artifacts, and batch behavior.
- [Codecs](codecs.md) — the codec & container matrix (self-contained + ffmpeg-backed).
- [Errors](errors.md) — the error model (E001–E009, EYEE) and warnings (W001–W015).
- [Rust API](rust-api.md) — the backend pipeline, modules, public API, and architecture.

## Quick index of directives

| Directive | Section |
|---|---|
| `#mobject` / `#animate` / `#pause` / `#play` | [Directives · Core](directives.md#core-directives) |
| `#audio` / `#video` | [Directives · Core](directives.md#core-directives) |
| `#save_state` / `#restore` / `#indicate` / `#flash` / `#wiggle` / `#appear` / `#disappear` / `#set_color` / `#transition` / `#zoom-to` | [Directives · Manim-inspired](directives.md#manim-inspired-directives) |
| `#blink` / `#spiral-in` / `#focus-on` / `#fade-transform` / `#move-along-path` / `#morph` / `#transform` / `#reveal` / `#typewriter` / `#track` | [Directives · Composite](directives.md#composite-animations) |
| `#group` / `#camera` | [Directives · Camera & groups](directives.md#camera--groups) |
| `#subtitle` | [Directives · Subtitles](directives.md#subtitles) |
| `#ecounter` / `#ecval` / `#counter_pause` / `#counter_resume` / `#counter_destroy` | [Counters](counters.md) |
