# Error model (E001–E010, EYEE)

All fallible operations return `Result<T, CandyError>`; production code must not panic.
`CandyError::code()` maps each variant to a mandatory error code:

> **Source location:** diagnostics that originate from a specific piece of user
> source point at it. When a warning/error carries a location, the reporter
> prints `path:line:col`, the offending source line, and a `^` caret under the
> exact span — e.g. `W014` (duplicate mobject/ecnew name) and `E004`
> (LabelNotFound) both do. This lets you jump straight to the problematic code
> instead of guessing from the message alone.

| Code | Variant | Meaning |
|---|---|---|
| EYEE | `Yee` | Batch partial failure — `candy build a.tyx b.tyx …` ran **every** input but at least one failed midway. |
| E001 | `Io` | `.tyx` file not found / generic I/O failure. |
| E002 | `Parse` | Invalid `.tyx` syntax (or non-monotonic `time_ms` in `schedule`). |
| E003 | `Svg` | `candy-json` missing/invalid (SVG extraction). |
| E004 | `LabelNotFound` | `@label` not found in the Typst layout. |
| E005 | `Interpolation` | Invalid interpolation range (clamped, non-fatal). |
| E006 | `Typst` | Typst render failure — the full `typst::diag::SourceDiagnostic` (message + any `hint:` lines) is captured and surfaced. |
| E007 | `Encode` | Rav1e/openh264 encoding failure. |
| E008 | `CandyDumpedYou` | The `.tyx` does not `#import` the candy package (or imports it with a version that does not match the installed candy CLI version). Candy can only render documents that import the candy package, whose root scene then owns all static content; a bare Typst document would otherwise produce empty / garbage output. |
| E009 | `UnknownKey` | A key reference (`@label`, `target:`, `animate(target:)`, etc.) points to a mobject that was never registered via `#mobject`. Also used when `ecval(...)` or lifecycle events (`ecpause`, `ecdestroy`, …) reference an unknown counter name. |
| E010 | `InvalidKey` | A key parameter evaluated to a non-string type (e.g., number, boolean, array). Keys must always resolve to strings. |

## Process exit codes

The terminal `error!` reporter prints `error: [Exxx] <message>` to **stderr** and terminates
the process with `CandyError::exit_code()`:

- **E001–E010** follow the `64`-based scheme `ERROR_EXIT_BASE + n - 1`
  (`ERROR_EXIT_BASE = 64`), so `E001` → `64` … `E007` → `70`, `E008` → `71`, `E009` → `72`, `E010` → `73`. This keeps all
  Candy fatal codes in a dedicated `64–73` segment that does not collide with `0` (success),
  `1` (generic), `2` (clap usage), or `101` (Rust panic).
- **EYEE is the one exception**: it deliberately does **not** use the `64` rule. Its exit code
  is the dedicated `BATCH_ERROR_EXIT = 111`. A batch is run to completion (no fail-fast) so
  partial progress is preserved; callers can detect "some inputs failed" via `111` without
  aborting the remaining inputs.

**Where `111` (and `yee~`) comes from.** `111` reads as "yī yī yī" → *"yee~"*, the strangled
little noise you make after biting into something spoiled — a fitting sound for a batch that
mostly worked but had a bad input somewhere in the middle. When a batch fails, Candy lists
every failed input (`Batch failed on N input(s):` + `- <path>: <error>`) and then surfaces the
marker through the diag pipeline as `error: [EYEE] yee~ Batch failed` before exiting with
`111`. A **single** failed input keeps its specific `E00x` code (e.g. `69` for `E006`) rather
than `111`.

## Warnings (W001–W015)

Warnings are **non-fatal**: they are printed to **stderr** as `warn: [Wxxx] …` and the render
continues. They describe recoverable or merely undesirable conditions. `CandyWarn::code()` maps
each variant to its `W` code.

| Code | Variant | Meaning |
|---|---|---|
| W001 | `TimeDependent` | `.tyx` uses `datetime.today()`; the render depends on the wall clock and is not reproducible. |
| W002 | `GpuUnavailable` | `--gpu` requested but the adapter/device could not be initialized; falling back to CPU rasterization. |
| W003 | `GpuFeatureDisabled` | `--gpu` passed but Candy was built without the `gpu` feature; using CPU. |
| W004 | `EncodeFallback` | Video encoding failed; an SVG draft was written under `.candy/`. |
| W005 | `CodecFallback` | A codec encode failed and Candy transparently fell back to another self-contained codec. |
| W006 | `AudioDropped` | An audio track was dropped (unsupported format or codec mismatch). |
| W007 | `EncodeRetry` | `rav1e` inter-prediction panicked; retrying AV1 in all-intra mode. |
| W008 | `AudioIgnored` | MP4 only muxes AAC audio; a non-AAC track was ignored. |
| W009 | `UnknownEasing` | An unknown easing name was given; falling back to `linear`. |
| W010 | `RevealFallback` | A `#reveal` body was not a string literal; falling back to `FadeIn`. |
| W011 | `CleanupFailed` | An intermediate directory could not be removed after a build. |
| W012 | `OutputNameCountMismatch` | The number of `--output` names does not match the number of inputs; custom names ignored, default `dist/<stem>.<ext>` used. |
| W013 | `OutputNameInvalid` | An `--output` name contains a path separator / multi-level directory; default `dist/<stem>.<ext>` used. |
| W014 | `DuplicateName` | A mobject label or ecnew name was redefined in the **same** lexical scope; the later definition shadows the earlier. Redefining inside a **nested** scope is legitimate Typst shadowing and is not warned. |
| W015 | `CallingPrivate` | The user called a Candy private function (name starts with `_`). These are internal helpers, not part of the public API. |
