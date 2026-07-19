# CLI reference

The `candy` CLI exposes one command, `build`, which renders a `.tyx` X-sheet (or an SVG
round-trip) into a video, GIF, PNG, or SVG draft.

## Synopsis

```sh
candy build <input> [--format FMT] [--codec CODEC] [-f FPS] [-p PIXELS_PER_PT]
       [--from-svg] [-o NAME] [--output-dir DIR] [--gpu] [--keep-intermediates]
```

## Flags

| Flag | Default | Description |
|---|---|---|
| `<input>` (positional) | required | Path to the `.tyx` X-sheet, or an SVG with a `candy-json` block (see `--from-svg`). |
| `--from-svg` | off | Force the input to be parsed as an SVG rendered by `@preview/candy`. Without this flag, the parser is selected by file extension (`.svg` → SVG round-trip, anything else → `.tyx`). |
| `-o, --output` (repeatable) | — | One plain file name per input — no path separators. Mismatched counts fall back to `dist/<stem>.<ext>` with a warning. |
| `--format` | `mp4` | `mp4` / `mkv` / `webm` / `gif` / `png` / `svg` (SVG draft → `.candy/`). The `--codec` flag is ignored for `gif` / `png`. |
| `--codec` | `x264` | `av1` / `h264` / `h265` / `x264` / `x265` / `h264-vaapi` / `h265-vaapi` / `h264-videotoolbox` / `h265-videotoolbox` / `h264-qsv` / `h265-qsv` / `av1-vaapi` / `vp9` / `vp8`. The first two (`h264`, `av1`) are self-contained (rav1e/openh264); `x264` is the default but requires system ffmpeg. See [Codecs](codecs.md). The hardware `*-vaapi` / `*-videotoolbox` / `*-qsv` variants are conditionally compiled and appear in `--help` only on their native platform (VAAPI → Linux, VideoToolbox → macOS, QSV → Windows). |
| `-f, --fps` | `30` | Frames per second (video path). |
| `-p, --pixel-per-pt` | `2.0` | Rasterization resolution (pixels per Typst point). |
| `--width <px>` | — | Pin output width in pixels. |
| `--height <px>` | — | Pin output height in pixels. |
| `--gpu` | off | Use GPU rasterization (vello + wgpu) for the video path. Requires `cargo build --features gpu`. Falls back to CPU if the feature is off or no GPU adapter is available. |
| `--jobs <n>` | `0` (= #CPUs) | Parallel rasterization jobs. |
| `--keep-intermediates` | off | Keep the `.candy/<stem>/` intermediate directory after a successful build (e.g. `frames.rgba`). By default Candy deletes it once the final video is written. Has no effect on `--format svg`. |
| `--output-dir <dir>` | `dist/` | Redirect every output file into a single directory. |
| `--output <name>` (repeatable) | — | One plain file name per input — no path separators. Mismatched counts or directory paths fall back to `dist/<stem>.<ext>` with a warning (W012 / W013). |

## Artifacts

- `.candy/<stem>/` — intermediates: `frames.rgba` (raw RGBA bundle), `frame_*.svg` (draft
  frames, also written on encode failure). For video builds this directory is **removed
  automatically** after a successful run unless `--keep-intermediates` is passed;
  `--format svg` keeps it (that draft *is* the output).
- `dist/<stem>.<ext>` — final video (MP4 / MKV / WebM), animated GIF (`.gif`), or static
  PNG bitmap of the final frame (`.png`). With `--output-dir <dir>` every one of these is
  redirected into `<dir>/` instead of `dist/`.

## Batch builds

`candy build` accepts multiple inputs (`candy build a.tyx b.tyx …`). Every input is
attempted (no fail-fast); if any fails, Candy reports each failed input and exits with code
`111` (the `EYEE` batch marker) while the successful ones still produce output. A single
failed input keeps its specific `E00x` code. See [Errors](errors.md).
