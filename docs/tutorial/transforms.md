# Transforms & text

Candy ports Manim-style content morphing. The headline feature is `#transform`: morph a
mobject's content — including **formulas** — into new inline content, glyph by glyph.

## `#transform` — Manim-style ReplacementTransform

`#transform(target, to: none, duration: 24, easing: "smooth")` smoothly replaces
`target`'s current body with `to` (a Typst body — a shape or a formula such as
`[$a + b + d = c$]`). The **original `target` label keeps the new content**, so you can
keep animating it afterwards.

For **inline content** (formulas and text) the transform is **glyph-by-glyph**, not a
whole-block dissolve. Candy renders the old and new bodies with Typst's own SVG layout
and extracts every glyph and decoration (fraction bars, roots, …) as a positioned
fragment, then matches old↔new fragments by their outline signature. During the window:

- **matched** fragments *glide* from their old slot to their new slot,
- **removed** fragments *fade and slide out* toward the next kept glyph,
- **inserted** fragments *fade and slide in* from the previous kept glyph,

so the old equation visibly disassembles and reassembles into the new one. Fractions
(`a/b`, `\frac{a}{b}`) are kept intact as a single token so the fraction bar renders
correctly (stacked numerator/denominator, not a bare slash). For **shapes** (non-inline
content) the transform falls back to a crossfade + scale morph.

```typst
#mobject("eq", [$a + b = c$])
#transform("eq", to: [$a + b + d = c$], duration: 1000, easing: "smooth")
```

## `#morph` / `#fade-transform` — crossfade two mobjects

`#morph(from, to, duration: 24, easing: "smooth")` morphs one mobject into another by
crossfading + scaling. Both must be registered via `#mobject`. This is the **simplified**
Morph — true point-by-point morphing (Manim's `Transform`) requires structured mobjects,
which Candy's opaque-content model does not support; the crossfade + scale variant is a
reasonable approximation.

`#fade-transform(from, to, duration: 300, easing: "smooth")` is the even simpler variant:
fade out `from` while fading in `to`.

## `#reveal` / `#typewriter` — progressive text

`#reveal(target, by: "word", duration: 1000, easing: "linear")` progressively reveals a
*string* mobject by swapping its body to longer and longer prefixes over `duration`.
`by: "word"` reveals word-by-word; `by: "char"` reveals character-by-character. Non-string
bodies fall back to a plain `FadeIn` with a warning. The body must be a string literal
(`#mobject("cap", "Hello")`), not a content block. At frame 0 the target is `none`, but
Candy keeps its layout box reserved (see
[Tutorial · first clip](../tutorial/first-clip.md#mobjects--actions--the-core-idea)), so
later content does not jump as it types in.

`#typewriter(target, duration: 1000, easing: "linear")` is a convenience alias for
`#reveal(.., by: "char")`.

```typst
#mobject("cap", "Step 1: divide by a.")
#typewriter("cap", duration: 1500, easing: "linear")
```

## Other Manim-inspired effects

These mirror Manim Community Edition primitives. Each is inert under standard Typst.

| Directive | Effect |
|---|---|
| `#save_state(target, slot: "default")` | snapshot a mobject's transform into a named slot (mirrors `save_state()`). |
| `#restore(target, slot: "default", duration: 500, easing: "linear")` | interpolate back to a saved state. |
| `#indicate(target, factor: 1.1, dx: 0, dy: 0, duration: 300, easing: "smooth")` | brief scale + shift "look here". |
| `#flash(target, factor: 2.0, duration: 200, easing: "smooth")` | scale up + fade toward transparent, then restore. |
| `#wiggle(target, degrees: 15.0, duration: 500, easing: "wiggle")` | oscillate rotation by ±`degrees`, then return. |
| `#appear(target)` / `#disappear(target)` | instant `opacity: 1.0` / `opacity: 0.0`, no interpolation. |
| `#set_color(target, color: "black", duration: 1, easing: "linear")` | record a color change (no-op in the current renderer; tracked for future structured mobjects). |
| `#blink(target, blinks: 3, duration: 500, easing: "linear")` | alternate opacity 1↔0 `blinks` times. |
| `#spiral-in(target, scale: 3.0, rotate: 360.0, duration: 300, easing: "smooth")` | fly in from a scaled-up, rotated, invisible state. |
| `#focus-on(target, factor: 0.5, duration: 300, easing: "smooth")` | shrink a "spotlight" onto the target. |
| `#move-along-path(target, path, duration: 500, easing: "linear")` | move along a polyline of `(x, y)` points (cm, absolute). |

```typst
#save_state("dot", slot: "home")
#animate("dot", to: (3cm, 2cm), duration: 800)
#restore("dot", slot: "home", duration: 200, easing: "cubic-in-out")

#move-along-path("ball", ((2, 2), (6, 5), (10, 2), (14, 4)), duration: 2000, easing: "smooth")
```

Full signatures: [Reference · Directives](../reference/directives.md).

Next: [Scenes, camera & groups](scenes-camera-groups.md).
