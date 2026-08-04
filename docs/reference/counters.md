# Counters

Counters let you drive Typst parameters (widths, radii, text content) with live,
animatable integers. The substitution happens in the Rust renderer, so the same
`.tyx` also compiles under plain `typst compile` — a counter reference resolves to
its seed / default value there.

There are two counter families:

- **Easing counters** (`ec*`) — a single ramp defined entirely at declaration:
  `seed` steps by `step` over `duration`, shaped by one `easing` (see
  [Easing](../reference/easing.md)).
- **Keyframe counters** (`kc*`) — a discrete-keyframe model. You register it with
  `kcnew`, then push keyframes at runtime via `kcpush` at the call-site timeline
  cursor (plus an optional `offset`), and the value is interpolated between
  adjacent keyframes at render time, with per-segment easing.

Both families share the same lexical-scope, lifecycle, and `E006 UnknownKey`
exception model. They use **independent namespaces** (`candy:counter:<name>` vs
`candy:kc:<name>`), so `ecnew("x")` and `kcnew("x")` never clash.

## Easing counters (`ec*`)

### `#ecnew(name, seed: 0, step: 1, duration: none, easing: "linear")`

Register an integer counter. Returns `seed` under standard Typst (so binding it
captures the initial value). With no `duration`, the counter steps once per
millisecond; a positive `duration` ramps `seed → seed + step·duration` over that
window, shaped by `easing`.

### `#ecval(value, default: 0)`

Read the current value of an easing counter. Inside Candy's pipeline it is
substituted with the live, eased integer and may be used directly as a Typst
parameter (`rect(width: ecval(n) * 1cm)`). Under standard Typst it returns its
argument unchanged when it is already a number, so bind the `ecnew` result
(`#let n = ecnew("n")`) and pass `n`.

### `#ecpause(name)` / `#ecresume(name)` / `#ecdestroy(name)`

Pause / resume / freeze a counter. Inert under standard Typst.

```typst
#let r = ecnew("r", seed: 40, step: 1)
#mobject("dot", circle(radius: ecval(r) * 1pt + 1cm, fill: blue))
#pause(duration: 600)
#ecpause("r")
#pause(duration: 600)
#ecresume("r")
#ecdestroy("r")
```

## Keyframe counters (`kc*`)

A keyframe counter holds a sequence of `(time, value)` keyframes. You push them
as the animation plays, and Candy interpolates the value between neighbours at
render time — useful for hand-authored, non-linear motion paths that don't fit a
single ramp.

### `#kcnew(name, seed: 0, easing: "linear")`

Register a keyframe counter. Returns `none` under standard Typst (same as
`ecnew`). `easing` is the counter-level default used by the first keyframe's
`inherit` (see below). `name` must be a string.

### `#kcpush(name, value, offset: 0, easing: "inherit")`

Push a keyframe.

- **No `at` parameter.** The keyframe's natural time is the timeline cursor
  (`ctx.cursor`) where `kcpush` is called. `value` is an integer — the counter
  value reached at that time.
- `offset` (integer, default `0`) shifts the effective keyframe time by `offset`
  ms: `effective_at = push_cursor + offset`. This moves where on the timeline the
  keyframe lands (and therefore affects the video-time computation of the
  surrounding segments).
- `easing` default is `"inherit"` (see Easing below). It controls the segment that
  *starts* at this keyframe (this node → next node); if there is no following
  node, the easing is ignored.
- Returns `none` under standard Typst; `name` non-string → `panic`.

### `#kcval(name, default: 0)`

Read the current interpolated value. In the Candy pipeline the Rust renderer
substitutes the live integer value, usable directly as a Typst argument
(e.g. `rect(width: kcval("n") * 1cm)`). Under standard Typst: `name` non-string →
`panic`; otherwise returns `default` (same as `ecval`, so the first frame shows
`default`).

### `#kcpause(name)` / `#kcresume(name)` / `#kcdestroy(name)`

Lifecycle events. Return `none` under standard Typst; `name` non-string → `panic`
(same as `ecpause`/`ecresume`/`ecdestroy`). `kcdestroy` freezes the value at the
destroy time.

### Easing semantics — `inherit`

The default `easing` on `kcpush` is `"inherit"`, used only by `kcpush`. It means:
take the **previous node's** effective easing. If there is no previous node (this
is the first keyframe), it resolves to the counter-level default `easing` passed
to `kcnew`. The easing set on the **current** node takes effect for the segment
that **starts at this node** — i.e. it controls `this node → next node`. If there
is no following node, the easing is ignored (there is no outgoing segment).
Segment interpolation uses the **starting** node's effective easing.

### Offset and the "pierce-through" rule (clamp + warning)

`effective_at = push_cursor + offset`. An offset is **unreasonable** when, after
sorting by time, the new keyframe would *pierce through* a neighbouring keyframe —
its effective time falls at or before the previous keyframe's time, or at or after
the next keyframe's time — which would break the monotonic ordering of the
keyframe track.

**Resolution: clamp, do not error.** Candy clamps `effective_at` into the valid
interval bounded by the neighbouring keyframe times and emits a **warning**
`W017 KeyframeOffsetClamp` describing which keyframe was clamped and why (see
[Errors](../reference/errors.md)). Rendering proceeds with the clamped, ordered
track. This is non-fatal, consistent with the project's existing warning style
(e.g. `W016 Interpolation`).

### Scope restriction

`kc*` follows the same lexical-scope rules as `ec*`. A counter is visible only
within the scope where it was declared and in descendant (nested) scopes; a
redeclaration in a nested scope is legitimate shadowing and auto-destroys when the
scope exits. Operating on a `kc` **outside its visible scope** is invalid — an
**invalid name**. This applies to `kcpush` / `kcval` / `kcpause` / `kcresume` /
`kcdestroy`: if the referenced name is not visible in the current scope chain, it
is routed through the same `E006 UnknownKey` path as `ecval` (pointing at the
declaration site). This is stricter than `ec*`, which records orphan events; for
`kc*` an out-of-scope name is reported rather than silently ignored.

### Exception handling (mirrors `ec*`)

- **Name type**: every `kc*` `name` must be a string; non-string `panic`s under
  standard Typst. In the Rust pipeline `kcnew`/`kcpush`/`kcval` require `name` to
  resolve to a string key, otherwise the directive is silently dropped (same as
  `ec*`).
- **Unknown / out-of-scope counter → `E006 UnknownKey`**: `kcval` on an
  undeclared or already-`kcdestroy`ed name, and any `kc*` operation on an
  out-of-scope name, go through the exact `E006` path used by `ecval`.
- **Duplicate name in same scope**: `DuplicateName("kcnew", ...)` warning + later
  definition shadows (identical to `ecnew`).
- **Pierce-through offset**: clamp + `W017` warning (see above), not an error.
- **Standard-Typst fallback**: `kcnew` → `none`; `kcpush`/`kcpause`/`kcresume`/
  `kcdestroy` → `none`; `kcval(name, default)` → `default`.

## Comparison

| Capability | Easing counter `ec*` | Keyframe counter `kc*` |
| --- | --- | --- |
| Register | `ecnew(name, seed, step, duration, easing)` | `kcnew(name, seed, easing)` |
| Drive | single ramp (`step × time`) fixed at declaration | `kcpush(name, value, offset, easing)` at runtime |
| Read | `ecval(name, default)` | `kcval(name, default)` |
| Pause / Resume / Destroy | `ecpause` / `ecresume` / `ecdestroy` | `kcpause` / `kcresume` / `kcdestroy` |
| Value model | `seed + step·elapsed` (eased if `duration` set) | interpolation between keyframes (per-segment easing) |
| Namespace | `candy:counter:<name>` | `candy:kc:<name>` (independent, no clash) |

## Full example

```typst
#scene()[
  #let k = kcnew("k", seed: 0, easing: "linear")
  #mobject("box", rect(width: kcval("k") * 1pt + 1cm, height: 1cm, fill: blue))
  #kcpush("k", 100, easing: "linear")
  #pause(duration: 500)
  #kcpush("k", 0, easing: "linear")
  #pause(duration: 500)
  #kcdestroy("k")
]
```
