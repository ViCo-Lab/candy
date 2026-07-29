# CLI reference

The `candy` CLI exposes one command, `build`, which renders a `.tyx` X-sheet (or an SVG
round-trip) into a video, GIF, PNG, or SVG draft.

## Synopsis

```sh
candy build <input> [--format FMT] [--codec CODEC] [-f FPS] [-p PIXELS_PER_PT]
       [--from-svg] [-o NAME] [--output-dir DIR] [-r DIR ...] [--gpu]
       [--keep-intermediates]
```

## Flags

| Flag | Default | Description |
|---|---|---|
| `<input>` (positional) | required | Path to the `.tyx` X-sheet, or an SVG with a `candy-json` block (see `--from-svg`). |
| `--from-svg` | off | Force the input to be parsed as an SVG rendered by `@preview/candy`. Without this flag, the parser is selected by file extension (`.svg` → SVG round-trip, anything else → `.tyx`). |
| `-o, --output` (repeatable) | — | One plain file name per input — no path separators. Mismatched counts fall back to `dist/<stem>.<ext>` with a warning. **Single-file (non-batch) builds** may instead pass a precise path (it contains `/` or `\`, or is the platform-independent link `.` / `..`): the output is written exactly to that path (parent dir created if needed, and any `.` / `..` hops are resolved by the OS) instead of `dist/`, and `--output-dir` is ignored. |
| `--format` | `mp4` | `mp4` / `mkv` / `webm` / `gif` / `png` / `svg` (SVG draft → `.candy/`). The `--codec` flag is ignored for `gif` / `png`. |
| `--codec` | `x264` | `av1` / `h264` / `h265` / `x264` / `x265` / `h264-vaapi` / `h265-vaapi` / `h264-videotoolbox` / `h265-videotoolbox` / `h264-qsv` / `h265-qsv` / `av1-vaapi` / `vp9` / `vp8`. The first two (`h264`, `av1`) are self-contained (rav1e/openh264); `x264` is the default but requires system ffmpeg. See [Codecs](codecs.md). The hardware `*-vaapi` / `*-videotoolbox` / `*-qsv` variants are conditionally compiled and appear in `--help` only on their native platform (VAAPI → Linux, VideoToolbox → macOS, QSV → Windows). |
| `-f, --fps` | `30` | Frames per second (video path). |
| `-p, --pixel-per-pt` | `2.0` | Rasterization resolution (pixels per Typst point). |
| `--width <px>` | — | Pin output width in pixels. |
| `--height <px>` | — | Pin output height in pixels. |
| `--gpu` | off | Use GPU rasterization (vello + wgpu) for the video path. Requires `cargo build --features gpu`. Falls back to CPU if the feature is off or no GPU adapter is available. |
| `--jobs <n>` | `0` (= #CPUs) | Parallel rasterization jobs. |
| `--keep-intermediates` | off | Keep the `.candy/<stem>/` intermediate directory after a successful build (e.g. `frames.rgba`). By default Candy deletes it once the final video is written. Has no effect on `--format svg`. |
| `--output-dir <dir>` | `dist/` | Redirect every output file into a single directory. The directory may contain separators (nested output trees work in both batch and recursive modes); it is joined verbatim, so `build/sub/` mirrors the source tree's sub-paths. |
| `-r, --recursive <dir>` (repeatable) | — | Recursively render every `.tyx` under the given directory(ies). Each argument **must be a directory** (a file or missing path is a fatal E001 I/O error). Hidden directories (`.git`, `.candy`, …) are skipped. The source tree's structure is mirrored inside a folder named after the source directory under `--output-dir`: a file at `<root>/a/b.tyx` is written to `<output-dir>/<root-name>/a/b.<ext>` (and a root-level `<root>/root.tyx` to `<output-dir>/<root-name>/root.<ext>`). May be combined with explicit `<input>` files and repeated (`-r dir1 -r dir2`). |
| `--output <name>` (repeatable) | — | One plain file name per input — no path separators. Mismatched counts or directory paths fall back to `dist/<stem>.<ext>` with a warning (W012 / W013). In a single-file (non-batch) build, `<name>` may be a precise path (containing `/`, or `\` on Windows, or the platform-independent link `.` / `..`): the output is written exactly there instead of `dist/`, and `--output-dir` is ignored. `.` / `..` are resolved by the OS. |

## Artifacts

- `.candy/<stem>/` — intermediates: `frames.rgba` (raw RGBA bundle), `frame_*.svg` (draft
  frames, also written on encode failure). For video builds this directory is **removed
  automatically** after a successful run unless `--keep-intermediates` is passed;
  `--format svg` keeps it (that draft *is* the output).
- `dist/<stem>.<ext>` — final video (MP4 / MKV / WebM), animated GIF (`.gif`), or static
  PNG bitmap of the final frame (`.png`). With `--output-dir <dir>` every one of these is
  redirected into `<dir>/` instead of `dist/`. For a single-file build, `--output` may be a
  precise path (it contains `/`, or `\` on Windows, or is the platform-independent link `.` / `..`); the file is then written exactly there (with `.` / `..` resolved by the OS), not under `dist/`. With `--recursive`, the source tree is mirrored inside a folder named after the source directory: `<root>/a/b.tyx` → `<output-dir>/<root-name>/a/b.<ext>` (root-level `<root>/root.tyx` → `<output-dir>/<root-name>/root.<ext>`); a directly-passed `<input>` stays at `<output-dir>/<stem>.<ext>` (top level, no source-name folder).

## Batch builds

`candy build` accepts multiple inputs (`candy build a.tyx b.tyx …`). Every input is
attempted (no fail-fast); if any fails, Candy reports each failed input and exits with code
`111` (the `EYEE` batch marker) while the successful ones still produce output. A single
failed input keeps its specific `E00x` code. See [Errors](errors.md).
