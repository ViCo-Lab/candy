# Animation basics

This page covers the everyday animation primitives: what `#animate` can do, how timing
works, and the `#pause` / `#play` helpers.

## The timing model

- Candy keeps an internal **millisecond** timeline. The `--fps` CLI flag only sets the
  *output* frame rate; a 1000 ms slide at 30 fps yields ~30 frames, at 60 fps ~60 frames
  — the wall-clock duration is unchanged.
- `#animate` / `#pause` / `#play` / `#transform` / … `duration:` arguments are expressed
  directly in **milliseconds** (default `500`). There is no frame-based timing — the
  scheduler works entirely in ms, and only the final rasterization samples that timeline
  at `--fps`.
- `#subtitle` and `#ecnew` lifetimes are likewise expressed in **milliseconds** directly.

## `#animate` — transforms

`#animate(target, …)` animates `target` over `duration` milliseconds (default `500`). It
supports absolute and relative transforms in any combination; each produces a parallel
action.

| Argument | Meaning |
|---|---|
| `to: (x, y)` | absolute target point in lengths, e.g. `(4cm, 0pt)` |
| `dx:` / `dy:` | relative offset in cm (Manim-style `shift`), e.g. `dx: 2cm` |
| `scale:` | absolute scale factor (e.g. `1.5`) |
| `scale-by:` | relative scale multiplier (e.g. `1.5` grows 50%) |
| `rotate:` | absolute clockwise rotation in degrees (e.g. `45`) |
| `rotate-by:` | relative rotation in degrees (e.g. `15` adds 15°) |
| `opacity:` | target opacity in `[0, 1]` |
| `duration:` | length of the animation in **milliseconds** (default `500`) |
| `easing:` | rate curve (default `"linear"`; see [Reference · Easing](../reference/easing.md)) |

```typst
#animate("dot", to: (4cm, 0pt), duration: 1000, easing: "linear")
#animate("box", scale: 1.5, duration: 800, easing: "smooth")
#animate("sq", dx: 2cm, rotate-by: 90, opacity: 0.5, duration: 600)
```

## `#pause` — hold a frame

`#pause(duration: 500)` holds the current frame for `duration` milliseconds. Inert under
standard Typst.

## `#play` — a block-level animation unit

`#play(body, duration: 500)` shows `body` for `duration` milliseconds as its own animation
unit (block-level, controllable like a mobject). Under standard Typst the body is shown
in the first frame.

```typst
#play([#text(28pt, weight: "bold")[Step 1 of 3]], duration: 1000)
#play([#text(28pt, weight: "bold")[Step 2 of 3]], duration: 1000)
```

## `#audio` — a voice / music track

`#audio(path, blocking: false, loop: false, volume: 1.0, slice: none)` attaches an
audio track. Inert under standard Typst.

| Argument | Meaning |
|---|---|
| `path` | audio file (`.opus`/`.ogg` for WebM/MKV, `.aac` for MP4) |
| `blocking:` | if `true`, the timeline waits for the clip to finish |
| `loop:` | repeat the clip |
| `volume:` | gain in `[0, 1]` |
| `slice:` | optional `(start, end)` seconds sub-range of the clip |

```typst
#audio("voice.opus", blocking: false, loop: false, volume: 0.9, slice: none)
```

## `#video` — a video placeholder

`#video(path, width: 8cm, height: 5cm)` inserts a **video reference** as a placeholder
mobject. Typst cannot embed video, so Candy renders a labeled placeholder box (rounded
rect + ▶ icon + filename). The placeholder behaves like any other mobject body (can be
animated). To show the real first frame, extract it with ffmpeg and use
`#mobject("vid", image(...))` instead.

```typst
#mobject("clip", video("intro.mp4", width: 10cm, height: 6cm))
#animate("clip", scale: 1.2, duration: 500, easing: "smooth")
```

## Easing at a glance

`easing:` accepts a named curve or a custom spec. Named curves include `"linear"`,
`"smooth"`, `"smoothstep"`, `"cubic-in-out"` (alias `"ease-in-out"`), `"there-and-back"`,
`"wiggle"`, `"lingering"`, and more. Custom specs: `expr:<math>` (e.g.
`"expr: 1 - (1 - t)^3"`) and `bezier:x1,y1,x2,y2` (e.g. `"bezier:0.25,0.1,0.25,1.0"`).

See [Reference · Easing](../reference/easing.md) for the full list.

Next: [Transforms & text](transforms.md).
