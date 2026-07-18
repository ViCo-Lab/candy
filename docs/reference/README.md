# Reference

Lookup-oriented documentation. For a guided introduction, see the
[Tutorial](../tutorial/README.md).

## Pages

- [Directives](directives.md) — every `#directive`, its arguments, and what it does.
- [Easing](easing.md) — named curves and custom `expr:` / `bezier:` specs.
- [Counters](counters.md) — `#ecnew` / `#ecval` and counter control.
- [CLI](cli.md) — `candy build` flags, artifacts, and batch behavior.
- [Codecs](codecs.md) — the codec & container matrix (self-contained + ffmpeg-backed).
- [Errors](errors.md) — the error model (E001–E009, EYEE) and warnings (W001–W015).
- [Rust API](rust-api.md) — the backend pipeline, modules, public API, and architecture.

## Quick index of directives

| Directive | Section |
|---|---|
| `#mobject` / `#group` / `#video` | [Directives · Mobjects & definitions](directives.md#mobjects--definitions) |
| `#animate` / `#appear` / `#disappear` / `#save-state` / `#restore` / `#indicate` / `#flash` / `#wiggle` / `#set-color` / `#blink` / `#spiral-in` / `#focus-on` / `#fade-transform` / `#move-along-path` / `#morph` / `#transform` / `#reveal` / `#typewriter` / `#track` / `#audio` | [Directives · Object animations](directives.md#object-animations) |
| `#scene` / `#scene-switch` / `#transition` / `#camera` / `#zoom-to` | [Directives · Scene & camera](directives.md#scene--camera) |
| `#play` / `#pause` | [Directives · Content blocks](directives.md#content-blocks) |
| `#subtitle` | [Directives · Subtitles (masks)](directives.md#subtitles-masks) |
| `#ecnew` / `#ecval` / `#ecpause` / `#ecresume` / `#ecdestroy` | [Counters](counters.md) |
