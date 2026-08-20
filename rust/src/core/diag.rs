//! Unified diagnostics. **All** diagnostic output in candy flows through this
//! module so it can be routed and coded consistently:
//!
//! | Level  | Stream  | Code | Behavior                                            |
//! |--------|---------|------|-----------------------------------------------------|
//! | Error  | stderr  | `E`  | print, then terminate (exit code `64`–`74`, e.g. `E001` → `64`, `E011` → `74`) |
//! | Error  | stderr  | `EYEE` | batch partial failure → terminate with exit code `111` (NOT the `64` rule) |
//! | Warn   | stderr  | `W`  | print, continue (non-fatal)                        |
//! | Debug  | stdout  | —    | print (developer diagnostics)                      |
//! | Info   | stdout  | —    | print (user-facing progress)                       |
//!
//! Fallible operations still return `Result<T, CandyError>` and propagate via
//! `?`; the terminal `error!` reporter is invoked exactly once at the process
//! boundary (see `main`) to surface a fatal error and set the exit code.
//!
//! All four reporters ([`error!`], [`warn!`], [`debug!`], [`info!`]) are
//! **macros** (not functions) so call sites read like `eprintln!`/`println!`
//! without wrapping every message in `format!`.

use std::fmt;
use std::io::{IsTerminal, Write};
use std::path::PathBuf;

pub use anstream::{AutoStream, ColorChoice};
// Re-export the `#[macro_export]` macros at the `core::diag` path so existing
// call sites (`crate::core::diag::cargo_status!`, `candy::core::diag::bold!`, …)
// keep working.
pub use crate::{
    blue, bold, cargo_finished, cargo_status, dim, eprint_styled, green, print_styled, red, yellow,
};
use anstyle::Style;

/// `Color` is re-exported (pub) so the `error!` / `warn!` macros can refer to
/// the caret color (`$crate::core::diag::Color::Red` / `::Yellow`) without
/// naming the `anstyle` crate directly at every call site. It aliases
/// `anstyle::AnsiColor`, whose `Red` / `Yellow` / `Green` / `BrightBlack`
/// variants are what the diagnostic styling uses.
pub use anstyle::AnsiColor as Color;

// ============================ Style helpers ============================
// All styling goes through `anstyle` `Style`s. The strings we build always
// carry ANSI codes; the `anstream`-backed writers ([`eprint_styled`] /
// [`print_styled`]) strip them when the destination isn't a terminal or
// `NO_COLOR` (https://no-color.org) is set, so the same code path serves both
// colored (TTY) and plain (piped / CI) output — no per-call TTY checks.

/// Wrap `text` in `style` and a matching reset (`{style:#}`), so the color
/// never bleeds past the end of the text.
pub fn paint(style: Style, text: &str) -> String {
    format!("{style}{text}{style:#}")
}

/// Dim (bright-black) style for the rustc-style gutter / arrow / line number,
/// and for "unknown" provenance values (e.g. a git hash that couldn't be read).
pub fn style_dim() -> Style {
    Style::new().fg_color(Some(anstyle::Color::Ansi(anstyle::AnsiColor::BrightBlack)))
}

/// Plain green (used for the version number in `candy version`).
pub fn style_green() -> Style {
    Style::new().fg_color(Some(anstyle::Color::Ansi(anstyle::AnsiColor::Green)))
}

/// Plain blue (used for the git hash in `candy version`).
pub fn style_blue() -> Style {
    Style::new().fg_color(Some(anstyle::Color::Ansi(anstyle::AnsiColor::Blue)))
}

/// Plain yellow (used for the release codename in `candy version`).
pub fn style_yellow() -> Style {
    Style::new().fg_color(Some(anstyle::Color::Ansi(anstyle::AnsiColor::Yellow)))
}

/// Plain red (used for a dirty git hash in `candy version`).
pub fn style_red() -> Style {
    Style::new().fg_color(Some(anstyle::Color::Ansi(anstyle::AnsiColor::Red)))
}

/// Bold + colored style for a caret (red errors, yellow warnings, …).
fn style_caret(c: Color) -> Style {
    Style::new().bold().fg_color(Some(anstyle::Color::Ansi(c)))
}

/// Bold + green (cargo `Finished` / `Compiling` verb style).
pub fn style_green_bold() -> Style {
    Style::new()
        .bold()
        .fg_color(Some(anstyle::Color::Ansi(anstyle::AnsiColor::Green)))
}

use crate::core::ast::Label;

// ============================== SourceLoc ==============================
// Every diagnostic that originates from a specific piece of user source
// (a duplicate name, an unknown label, a syntax problem) carries a `SourceLoc`
// so the reporter can point the user at the *exact* file:line:col and the
// offending code — not just a free-text message. This is what turns
// "mobject name 'a' is redefined…" into something you can actually act on.

/// A source-code location: a file path plus the byte range of the offending
/// snippet, from which a `path:line:col` header and a caret-annotated source
/// line are rendered. Optional on a diagnostic (some errors, e.g. an I/O
/// failure, have no user-source location to point at).
#[derive(Debug, Clone)]
pub struct SourceLoc {
    /// Absolute path of the source file.
    pub path: PathBuf,
    /// 1-based line number of `start`.
    pub line: usize,
    /// 1-based column (in characters) of `start`.
    pub col: usize,
    /// The full text of the line containing `start` (for display).
    pub line_text: String,
    /// Byte offset of the start of the offending span.
    pub start: usize,
    /// Byte offset of the end of the offending span.
    pub end: usize,
    /// Character length of the offending span (number of Unicode scalar
    /// values covered by `[start, end)`). This is what the caret (`^^^`)
    /// uses — not `end - start` (byte length), which would be wrong for
    /// multi-byte characters (Chinese, Emoji, …).
    pub char_span: usize,
    /// When this location is the deepest frame of a nested-include diagnostic
    /// (i.e. the *actual* error inside the deepest included file), this holds
    /// the chain of *outer* includers (from the immediate parent up to the root
    /// document), each as a `SourceLoc` of that includer's `#include` line. The
    /// reporter prints these as a layer-by-layer "included from …" trace so an
    /// error that originates deep inside an include chain shows the full path
    /// back to the root rather than a single line. Empty for errors that are
    /// not inside a nested include.
    pub include_trace: Vec<SourceLoc>,
}

impl SourceLoc {
    /// Build a `SourceLoc` from a `path`, the full `raw` source text, and the
    /// byte `range` of the offending snippet. Computes the 1-based line/column
    /// and captures the offending line's text so it can be rendered later
    /// without holding the whole source alive.
    pub fn at(path: &std::path::Path, raw: &str, range: std::ops::Range<usize>) -> SourceLoc {
        let mut line = 1usize;
        let mut col = 1usize;
        let mut line_start = 0usize;
        for (i, ch) in raw.char_indices() {
            if i >= range.start {
                break;
            }
            if ch == '\n' {
                line += 1;
                col = 1;
                line_start = i + 1;
            } else {
                col += 1;
            }
        }
        let line_text = raw[line_start..].lines().next().unwrap_or("").to_string();
        // Character length of the span (not byte length) so the caret covers
        // multi-byte characters (Chinese, Emoji, …) correctly.
        let char_span = raw[range.start..range.end.min(raw.len())]
            .chars()
            .count()
            .max(1);
        SourceLoc {
            path: path.to_path_buf(),
            line,
            col,
            line_text,
            start: range.start,
            end: range.end,
            char_span,
            include_trace: Vec::new(),
        }
    }

    /// Build a `SourceLoc` at `range` in `raw` (path `path`) and attach the
    /// *outer* frames of an include chain as its `include_trace`. `chain` is the
    /// full includer stack (outermost → innermost); the last entry is the
    /// immediate includer (this location itself), so only `chain[..len-1]`
    /// becomes trace frames. Used when a diagnostic points at an `#include`
    /// call-site so the reporter can expand the nested include path.
    pub fn at_with_chain(
        path: &std::path::Path,
        raw: &str,
        range: std::ops::Range<usize>,
        chain: &[(std::path::PathBuf, String, std::ops::Range<usize>)],
    ) -> SourceLoc {
        let mut loc = SourceLoc::at(path, raw, range);
        loc.include_trace = chain
            .iter()
            .take(chain.len().saturating_sub(1))
            .map(|(p, r, rg)| SourceLoc::at(p, r, rg.clone()))
            .collect();
        loc
    }

    /// Build the *deepest* frame of an include-chain diagnostic: point directly
    /// at the **real error inside the included file** (path `inc_path`, original
    /// text `inc_raw`, byte `range` within it), and attach the *full* includer
    /// chain as an `include_trace` so the reporter prints every outer includer's
    /// `#include` line as an "included from …" frame. Unlike [`SourceLoc::at`],
    /// the deepest frame is the genuine error site, not an includer's
    /// `#include` call-site.
    pub fn at_included_error(
        inc_path: &std::path::Path,
        inc_raw: &str,
        range: std::ops::Range<usize>,
        chain: &[(std::path::PathBuf, String, std::ops::Range<usize>)],
    ) -> SourceLoc {
        let mut loc = SourceLoc::at(inc_path, inc_raw, range);
        loc.include_trace = chain
            .iter()
            .map(|(p, r, rg)| SourceLoc::at(p, r, rg.clone()))
            .collect();
        loc
    }

    /// Render the location in the cargo/rustc snippet style:
    /// ```text
    ///  --> path:line:col
    ///   |
    /// 5 |   <line_text>
    ///   |   ^^^^^
    /// ```
    /// The caret column is computed from `char_span` (Unicode scalar count),
    /// not byte length, so multi-byte characters (Chinese, Emoji, …) are
    /// underlined correctly.
    /// Render with color, mimicking rustc: the ` --> ` arrow and the `|` gutter
    /// are dim (bright-black), the line number is dim + bold, and the caret is
    /// drawn in `caret_color` (red for errors, yellow for warnings). The returned
    /// string always carries ANSI codes; the [`crate::core::diag::eprint_styled`]
    /// / [`crate::core::diag::print_styled`] writers (backed by `anstream`) strip
    /// them automatically when the output is not a terminal or `NO_COLOR`
    /// (https://no-color.org) is set, so piped / captured output stays clean.
    pub fn render_colored(&self, caret_color: Color) -> String {
        self.render_block(caret_color)
    }

    /// Shared implementation. The arrow, gutter, and line number are always dim
    /// and the caret is drawn in `caret_color`. The codes are emitted
    /// unconditionally — the `anstream`-backed writers decide whether to keep or
    /// strip them based on the destination's terminal / `NO_COLOR` state.
    fn render_block(&self, caret_color: Color) -> String {
        let line_str = self.line.to_string();
        let line_len = self.line_text.chars().count();
        let avail = line_len.saturating_sub(self.col.saturating_sub(1)).max(1);
        let caret_len = self.char_span.clamp(1, avail);
        let indent = " ".repeat(self.col.saturating_sub(1));
        let caret = "^".repeat(caret_len);

        let (arrow, bar, lineno, caret_str) = (
            paint(style_dim(), " -->"),
            paint(style_dim(), "  |"),
            paint(style_dim(), &line_str),
            paint(style_caret(caret_color), &caret),
        );
        format!(
            "{} {}:{}:{}\n{}\n{} | {}\n{} {}{}",
            arrow,
            self.path.display(),
            self.line,
            self.col,
            bar,
            lineno,
            self.line_text,
            bar,
            indent,
            caret_str
        )
    }
}

// ============================== Error (E) ==============================

/// Candy's unified error type. The [`CandyError::code`] method maps each
/// variant to the mandatory error codes E001–E011 (E008 is the fixed easter-egg slot).
#[derive(Debug)]
pub enum CandyError {
    /// E001 — `.tyx` file not found / generic I/O failure.
    Io(std::io::Error),
    /// E002 — Invalid `.tyx` syntax / API-format error. This is the *uniform*
    /// home for every malformed user-facing API call: invalid syntax, an
    /// **unknown named argument**, or a **wrong-typed argument** (e.g. passing
    /// a non-string where a string/label is required). This includes the
    /// `#scene` argument mistakes — `width` / `height` / `bg` (and any other
    /// unknown argument) are reported here as "not a valid argument for
    /// #scene", and a non-string `name` is reported as a type error — so all
    /// argument-format problems across the candy API share one code.
    /// NOTE: a key argument that *is* a string but is **not a valid Typst
    /// identifier** (e.g. it contains a space, or starts with a digit) is
    /// reported by `E007 InvalidKey` instead, not here — E002 only covers
    /// non-string/wrong-typed arguments and unknown names. Carries the
    /// offending source location when the failure can be tied to a span.
    Parse(String, Option<SourceLoc>),
    /// E003 — `candy-json` missing/invalid (SVG extraction).
    Svg(String),
    /// E004 — `@label` not found in the Typst layout. Carries the label's
    /// declaration location when known (so the user sees where it was defined).
    LabelNotFound(Label, Option<SourceLoc>),
    /// E005 — Typst render failure. Carries the offending source location
    /// (file:line:col + the offending line) when the failure can be tied to a
    /// specific span in the compiled Typst source, so the user is pointed at the
    /// exact code that failed to compile — just like the parser-level errors
    /// (E002/E004/…). Without a resolvable span (e.g. an internal Typst panic)
    /// the location is `None`.
    Typst(String, Option<SourceLoc>),
    /// E006 — A key reference (`@label`, `target:`, `animate(target:)`, etc.)
    /// points to a mobject that was never registered via `#mobject`. Also used
    /// when `ecval(...)` or lifecycle events (`ecpause`, `ecdestroy`,
    /// …) reference an unknown counter name. The first field is the kind
    /// (`"mobject"` / `"ecnew"` / `"scene"`) and the second is the offending
    /// key name.
    UnknownKey(String, String, Option<SourceLoc>),
    /// E007 — A key (label/name) used to identify a mobject, scene, easing
    /// counter, or keyframe counter failed validation. Two distinct failures
    /// are covered, each with its own message:
    ///   - `not_ident == false`: the argument did **not** resolve to a string
    ///     literal (e.g. a number, boolean, or array was passed), so it can't
    ///     serve as a key at all;
    ///   - `not_ident == true`: the argument *was* a string, but it is not a
    ///     valid Typst identifier (e.g. it contains a space or starts with a
    ///     digit / `-`), which candy's renderer cannot look up.
    ///
    /// The `what` field names the role (e.g. "mobject label", "scene name",
    /// "easing-counter name") and `value` is the offending value (or its type
    /// description) exactly as written.
    InvalidKey {
        what: String,
        value: String,
        not_ident: bool,
        loc: Option<SourceLoc>,
    },
    /// E008 — The `.tyx` does not import the candy package (or imports it
    /// with a version incompatible with the installed candy CLI), so its
    /// static content has no scene to own it. Candy can only render documents
    /// that import `@<namespace>/candy:<version>` where the version satisfies
    /// at least one semver requirement from the CLI's compatibility list
    /// (`[package.metadata.tyx].compatible_versions` in the Rust `Cargo.toml`,
    /// baked in by `build.rs` and matched via the `semver` crate). A bare
    /// Typst document, a file-style import (`#import "candy"`), or an
    /// incompatible version all trigger this error. Pass `--ignore-version`
    /// to skip the version check (useful for development).
    CandyDumpedYou(String, Option<SourceLoc>),
    /// E009 — Rav1e / codec / mux encoding failure.
    Encode(String),
    /// E010 — SVG frame **rasterization** failure. This is the *render* stage
    /// (usvg parse, wgpu adapter/device, vello scene render/poll) that turns a
    /// compiled SVG frame into RGBA pixels — distinct from `Encode` (E009), which
    /// is the later codec/mux stage. Mislabeling a raster failure as `Encode`
    /// sent users hunting in the wrong subsystem (codec vs. rasterizer) and
    /// printed a misleading `encode:` prefix, exactly the kind of code/message
    /// mismatch that was fixed for `validate()` (E002 vs E006).
    Raster(String),
    /// E011 — The `#scene`-specific **structural** errors that are *not*
    /// argument-format mistakes (those are `E002`). This is the dedicated home
    /// for mistakes that are purely about *how scenes are declared*, kept
    /// separate from the catch-all E008 so the two never collide:
    ///   (a) a `#scene(...)` call appears **inside another scene's body** —
    ///       scenes are flat, there is only "switch scene", never "enter a
    ///       sub-scene";
    ///   (b) the document mixes explicit `#scene(...)` calls with content at
    ///       the **document root** — it must be either parallel scenes with an
    ///       empty root, or root content with no scene call at all.
    /// Argument-format mistakes go to other codes: an unknown argument or a
    /// non-string `name` is `E002 Parse`, while a `name` that *is* a string but
    /// not a valid Typst identifier is `E007 InvalidKey`.
    Scene(String, Option<SourceLoc>),
    /// EYEE — Batch partial failure: `candy build a.tyx b.tyx …` ran every
    /// input but at least one failed midway. Surfaced as the "yee~ Batch
    /// failed. \\(!_!)/" marker. **Deliberately does NOT follow** the `ERROR_EXIT_BASE +
    /// n - 1` scheme used by E001–E011 — its process exit code is the dedicated
    /// [`BATCH_ERROR_EXIT`] (111) instead, so a CI pipeline / shell script can
    /// detect "some inputs failed" without aborting the remaining inputs.
    Yee(String),
}

impl CandyError {
    /// Mandatory error code (E001–E011, with `EYEE` as the non-numeric batch marker).
    pub fn code(&self) -> &'static str {
        match self {
            CandyError::Yee(_) => "EYEE",
            CandyError::Io(_) => "E001",
            CandyError::Parse(_, _) => "E002",
            CandyError::Svg(_) => "E003",
            CandyError::LabelNotFound(_, _) => "E004",
            CandyError::Typst(_, _) => "E005",
            CandyError::Encode(_) => "E009",
            CandyError::Raster(_) => "E010",
            CandyError::Scene(_, _) => "E011",
            CandyError::CandyDumpedYou(_, _) => "E008",
            CandyError::UnknownKey(_, _, _) => "E006",
            CandyError::InvalidKey { .. } => "E007",
        }
    }

    /// Numeric part of the code (1–11), used to build the process exit code for
    /// the E001–E011 family. `EYEE` is excluded here on purpose — it carries no
    /// `64`-based number (see [`CandyError::exit_code`]).
    pub fn number(&self) -> u8 {
        match self {
            CandyError::Yee(_) => 111,
            CandyError::Io(_) => 1,
            CandyError::Parse(_, _) => 2,
            CandyError::Svg(_) => 3,
            CandyError::LabelNotFound(_, _) => 4,
            CandyError::Typst(_, _) => 5,
            CandyError::Encode(_) => 9,
            CandyError::Raster(_) => 10,
            CandyError::Scene(_, _) => 11,
            CandyError::CandyDumpedYou(_, _) => 8,
            CandyError::UnknownKey(_, _, _) => 6,
            CandyError::InvalidKey { .. } => 7,
        }
    }

    /// Process exit code for this error.
    /// The E001–E011 family follows `ERROR_EXIT_BASE + n - 1` (`E001` → `64` …
    /// `E011` → `74`). `EYEE` is the **one exception**: it bypasses that scheme
    /// and returns the dedicated [`BATCH_ERROR_EXIT`] (111) — the batch
    /// partial-failure marker ("yee~ Batch failed") must not be re-encoded into
    /// the `64` range.
    pub fn exit_code(&self) -> i32 {
        match self {
            CandyError::Yee(_) => BATCH_ERROR_EXIT,
            other => ERROR_EXIT_BASE + other.number() as i32 - 1,
        }
    }

    /// The human-readable message, WITHOUT the `[Exxx]` / `[EYEE]` code prefix.
    /// The `error!` macro renders this separately from the code so the code can
    /// be shown bold + colored while the message stays plain.
    pub fn message(&self) -> String {
        match self {
            CandyError::Io(e) => format!("I/O error: {e}"),
            CandyError::Parse(e, _) => format!("parse: Invalid .tyx syntax: {e}"),
            CandyError::Svg(e) => format!("svg: candy-json missing/invalid: {e}"),
            CandyError::LabelNotFound(l, _) => {
                format!("render: Label @{} not found in Typst layout", l.0)
            }
            CandyError::Typst(e, _) => format!("typst: {e}"),
            CandyError::Encode(e) => format!("encode: {e}"),
            CandyError::Raster(e) => format!("raster: {e}"),
            CandyError::Scene(e, _) => format!("scene: {e}"),
            CandyError::CandyDumpedYou(e, _) => format!("candy: {e}. She dumped you! (-_-)"),
            CandyError::UnknownKey(kind, key, _) => {
                format!(
                    "parse: {kind} \"{key}\" does not exist (never declared or already destroyed)"
                )
            }
            CandyError::InvalidKey {
                what,
                value,
                not_ident,
                ..
            } => {
                if *not_ident {
                    format!(
                        "parse: {what} \"{value}\" is not a valid Typst identifier; \
                         it must start with a letter or `_` and contain only letters, \
                         digits, `_`, or `-` (no spaces, no leading digit or `-`)"
                    )
                } else {
                    format!("parse: {what} must be a string literal, got {value}")
                }
            }
            CandyError::Yee(e) => e.to_string(),
        }
    }

    /// An optional one-line `hint:` for this error, surfaced after the source
    /// snippet (rustc / `hint:` style). Only the **source-localizable,
    /// non-Typst** errors carry a hint here — Typst errors (E005) already embed
    /// their own `hint:` lines inside [`CandyError::message`] (see
    /// [`crate::core::diag::format_typst_errors`]), so they return `None` to
    /// avoid a duplicate. A hint points the user at the *most likely fix* for an
    /// error that already carries a source location, turning "something is
    /// wrong here" into "here is what to check".
    pub fn hint(&self) -> Option<&'static str> {
        match self {
            CandyError::Parse(_, _) => Some(
                "check the .tyx syntax near the marked location; the caret points at the \
                 offending token",
            ),
            CandyError::LabelNotFound(_, _) => Some(
                "the label may be declared in a different scene, already removed, or never \
                 created with #mobject",
            ),
            CandyError::UnknownKey(_, _, _) => {
                Some("declare the key with #mobject / #ecnew, or check the name for a typo")
            }
            CandyError::InvalidKey {
                not_ident, what, ..
            } => {
                if *not_ident {
                    Some(
                        "use a valid Typst identifier: start with a letter or `_`, and only \
                         letters, digits, `_`, or `-` (no spaces, no leading digit or `-`)",
                    )
                } else if what.contains("must be a ratio") {
                    Some("use a Typst ratio literal such as `50%` (a bare number is not a ratio)")
                } else if what.contains("must be an angle") {
                    Some(
                        "use a Typst angle literal such as `90deg` or `1.5rad` (a bare number is not an angle)",
                    )
                } else {
                    Some("wrap the value in quotes or use a string literal")
                }
            }
            CandyError::Scene(_, _) => Some(
                "a scene takes only `name:`; set the canvas via `#show: candy` and the background via `#set page(fill: ...)` inside the scene body",
            ),
            // E008 covers two distinct mistakes: a missing/incompatible import
            // and a missing `#show: candy` rule. Suggesting "add the import" to
            // someone who already imported candy is actively misleading, so
            // pick the hint that matches the failure.
            CandyError::CandyDumpedYou(msg, _) if msg.contains("show rule") => Some(
                "add `#show: candy` right after the candy import — it sets the global canvas; \
                 use `#show: candy.with(width: .., height: .., ppi: .., fps: ..)` to override \
                 the defaults (13.33in x 7.5in, ppi 144, fps 30)",
            ),
            CandyError::CandyDumpedYou(_, _) => Some(concat!(
                "add `#import \"@preview/candy:",
                env!("CARGO_PKG_VERSION"),
                "\": *` at the top (matching the installed candy CLI v",
                env!("CARGO_PKG_VERSION"),
                "), or pass --ignore-version",
            )),
            _ => None,
        }
    }

    /// The source location tied to this error, if any. Rendered by the `error!`
    /// reporter after the message so the user is pointed at the offending code.
    pub fn loc(&self) -> Option<&SourceLoc> {
        match self {
            CandyError::LabelNotFound(_, l) => l.as_ref(),
            CandyError::Parse(_, l) => l.as_ref(),
            CandyError::Scene(_, l) => l.as_ref(),
            CandyError::CandyDumpedYou(_, l) => l.as_ref(),
            CandyError::UnknownKey(_, _, l) => l.as_ref(),
            CandyError::InvalidKey { loc: l, .. } => l.as_ref(),
            CandyError::Typst(_, l) => l.as_ref(),
            _ => None,
        }
    }
}

impl fmt::Display for CandyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}] {}", self.code(), self.message())
    }
}

impl std::error::Error for CandyError {}

impl From<std::io::Error> for CandyError {
    fn from(e: std::io::Error) -> Self {
        // A missing file is the canonical E001 trigger.
        CandyError::Io(e)
    }
}

impl From<serde_json::Error> for CandyError {
    fn from(e: serde_json::Error) -> Self {
        CandyError::Svg(e.to_string())
    }
}

// ===================== Typst Error capture (E005) ======================
// A Typst compile yields `typst::ecow::EcoVec<typst::diag::SourceDiagnostic>`
// (the error half of `typst::SourceResult<T>`). This `From` impl lets any
// `?` on a Typst result be captured uniformly as `CandyError::Typst` and thus
// assigned the mandatory `E005` code, instead of every call site hand-rolling
// `format!("{:?}", errs)`.

/// The error type produced by `typst::compile` / `typst::SourceResult<T>`.
pub type TypstErrors = typst::ecow::EcoVec<typst::diag::SourceDiagnostic>;

impl From<TypstErrors> for CandyError {
    fn from(errs: TypstErrors) -> Self {
        // Without a `World` we cannot resolve the span to a `SourceLoc`
        // (line/col requires the source text). Callers with access to a
        // `World` should use `typst_diag_loc` instead (see
        // `Renderer::compile` / `compile_file_for_test`). This `From` impl is
        // a last-resort fallback for sites that only have the errors.
        CandyError::Typst(format_typst_errors(&errs), None)
    }
}

/// Render a collection of Typst [`typst::diag::SourceDiagnostic`] into a
/// single human-readable message (message + any `hint:` lines).
pub(crate) fn format_typst_errors(errs: &TypstErrors) -> String {
    errs.iter()
        .map(|d| {
            let mut s = d.message.to_string();
            for hint in &d.hints {
                s.push_str(&format!("\n  hint: {}", hint.v));
            }
            s
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// ============================== Warn (W) ===============================

/// Candy's unified **warning** type. Warnings are non-fatal: they describe
/// conditions that are recoverable or merely undesirable (e.g. a non-
/// reproducible render, a transparent codec fallback) and are surfaced via
/// `warn!` / [`CandyWarn::code`] / [`fmt::Display`] with a `W` prefix.
#[derive(Debug, Clone)]
pub enum CandyWarn {
    /// W001 — `.tyx` uses the current date/time (`datetime.today()`), so the
    /// render depends on the wall clock and is not reproducible.
    TimeDependent,
    /// W002 — GPU rasterization was requested but the adapter/device could not
    /// be initialized; candy falls back to CPU rasterization.
    GpuUnavailable(String),
    /// W003 — `--gpu` was passed but candy was built without the `gpu` feature;
    /// falling back to CPU rasterization.
    GpuFeatureDisabled,
    /// W004 — Video encoding failed; an SVG draft was written under `.candy/`.
    EncodeFallback(String),
    /// W005 — A codec encode failed and candy transparently fell back to
    /// another self-contained codec.
    CodecFallback(String),
    /// W006 — An audio track was dropped (unsupported format or codec mismatch).
    AudioDropped(String),
    /// W007 — `rav1e` inter-prediction panicked; retrying AV1 in all-intra mode.
    EncodeRetry,
    /// W008 — MP4 only muxes AAC audio; a non-AAC track was ignored.
    AudioIgnored,
    /// W009 — An unknown easing name was given; falling back to `linear`.
    /// Fields: the offending easing name, and the source location of the
    /// directive that carried it (for the source-trace / `hint:` output).
    UnknownEasing(String, SourceLoc),
    /// W010 — A `#reveal` body was not a string literal; falling back to
    /// `FadeIn`.
    /// Fields: the offending label, and the source location of the directive
    /// that carried it (for the source-trace / `hint:` output).
    RevealFallback(String, SourceLoc),
    /// W011 — An intermediate directory could not be removed after a build.
    CleanupFailed(String),
    /// W012 — The number of `--output` names does not match the number of
    /// inputs, so the custom names are ignored and the default
    /// `dist/<stem>.<ext>` names are used for every input.
    OutputNameCountMismatch(String),
    /// W013 — A `--output` name contains a path separator (a multi-level /
    /// directory path) or is otherwise not a plain file name, so it is ignored
    /// for that input and the default `dist/<stem>.<ext>` name is used instead.
    OutputNameInvalid(String),
    /// W014 — A mobject label, ecnew, or scene name was redefined in the
    /// *same* lexical scope. Candy keeps the later definition (it shadows the
    /// earlier one) but warns, because an accidental duplicate usually indicates
    /// a typo. Redefining a name inside a *nested* scope is legitimate Typst
    /// shadowing and does not warn. The first field is the kind (`"mobject"` /
    /// `"ecnew"` / `"scene"`), the second the offending name, the third the
    /// source location of the *redefining* (later) declaration so the user is
    /// pointed at the exact code.
    DuplicateName(String, String, SourceLoc),

    /// W015 — The user called a Candy private function (name starts with `_`).
    /// These are internal helpers, not part of the public API.
    /// Fields: the private function name (e.g. `"_assert_str"`), and the
    /// source location of the call (for the source-trace / `hint:` output).
    CallingPrivate(String, SourceLoc),

    /// W016 — Opacity went out of the valid `[0, 1]` interpolation range and
    /// was clamped during interpolation (`interpolator::interpolate_with`).
    /// Non-fatal: the interpolator clamps and continues, but warns so the user
    /// knows their keyframes / easing produced an out-of-range opacity.
    Interpolation(String),

    /// W017 — A `kcpush` offset made a keyframe "pierce through" a neighbouring
    /// keyframe: its effective time (`push_cursor + offset`) fell at or before the
    /// previous keyframe or at or after the next one, which would break the
    /// monotonic ordering of the keyframe track. Candy clamps the keyframe into
    /// the valid interval between neighbours and continues (non-fatal); the
    /// offending `kcpush` is effectively repositioned rather than errored.
    KeyframeOffsetClamp(String, SourceLoc),

    /// W019 — The user called an unknown Candy directive (a name that is not in
    /// the `CANDY` registry). Previously such calls were silently ignored; now
    /// they emit a warning so the user knows their typo or stale call is doing
    /// nothing.
    UnknownDirective(String, SourceLoc),

    /// W018 — A scene's content overflowed the single-page viewport: the flow
    /// layout spilled past the declared/page height. The overflow content is
    /// still rendered, but clipped at rasterization by the fixed viewport
    /// `viewBox` (content may overflow in any direction; the viewport stays in
    /// place), so it isn't shown. This is usually unintentional (the content was
    /// expected to fit one screen), so candy warns with the scene name.
    /// Field: a description like `scene 'intro' content overflows the viewport`.
    ContentOverflow(String),
}

impl CandyWarn {
    /// Mandatory warning code (W001–W018).
    pub fn code(&self) -> &'static str {
        match self {
            CandyWarn::TimeDependent => "W001",
            CandyWarn::GpuUnavailable(_) => "W002",
            CandyWarn::GpuFeatureDisabled => "W003",
            CandyWarn::EncodeFallback(_) => "W004",
            CandyWarn::CodecFallback(_) => "W005",
            CandyWarn::AudioDropped(_) => "W006",
            CandyWarn::EncodeRetry => "W007",
            CandyWarn::AudioIgnored => "W008",
            CandyWarn::UnknownEasing(_, _) => "W009",
            CandyWarn::RevealFallback(_, _) => "W010",
            CandyWarn::CleanupFailed(_) => "W011",
            CandyWarn::OutputNameCountMismatch(_) => "W012",
            CandyWarn::OutputNameInvalid(_) => "W013",
            CandyWarn::DuplicateName(_, _, _) => "W014",
            CandyWarn::CallingPrivate(_, _) => "W015",
            CandyWarn::Interpolation(_) => "W016",
            CandyWarn::KeyframeOffsetClamp(_, _) => "W017",
            CandyWarn::ContentOverflow(_) => "W018",
            CandyWarn::UnknownDirective(_, _) => "W019",
        }
    }

    /// The human-readable message, WITHOUT the `[Wxxx]` code prefix. The `warn!`
    /// macro renders this separately from the code so the code can be shown bold
    /// + colored while the message stays plain.
    pub fn message(&self) -> String {
        match self {
            CandyWarn::TimeDependent => "render: .tyx uses the current date/time \
                 (datetime.today()); the render depends on the wall clock and is \
                 not reproducible"
                .into(),
            CandyWarn::GpuUnavailable(e) => {
                format!("gpu: unavailable, falling back to CPU: {e}")
            }
            CandyWarn::GpuFeatureDisabled => "gpu: --gpu requested but candy was built \
                 without the 'gpu' feature; using CPU"
                .into(),
            CandyWarn::EncodeFallback(d) => {
                format!("encode: failed, wrote SVG draft to .candy: {d}")
            }
            CandyWarn::CodecFallback(d) => {
                format!("encode: codec encode failed, falling back: {d}")
            }
            CandyWarn::AudioDropped(d) => format!("audio: dropping audio track: {d}"),
            CandyWarn::EncodeRetry => "encode: rav1e inter-prediction panicked; retrying \
                 AV1 in all-intra mode (valid but no temporal compression)"
                .into(),
            CandyWarn::AudioIgnored => {
                "audio: MP4 only muxes AAC audio; ignoring non-AAC track".into()
            }
            CandyWarn::UnknownEasing(d, _) => {
                format!("parse: unknown easing {d}; falling back to linear")
            }
            CandyWarn::RevealFallback(d, _) => {
                format!("parse: #reveal body is not a string literal; falling back to FadeIn: {d}")
            }
            CandyWarn::CleanupFailed(d) => {
                format!("build: could not remove intermediate dir {d}")
            }
            CandyWarn::OutputNameCountMismatch(d) => {
                format!(
                    "build: {d}; ignoring custom --output names and using the default \
                     dist/<stem>.<ext> for every input"
                )
            }
            CandyWarn::OutputNameInvalid(d) => {
                format!(
                    "build: --output name '{d}' is not a plain file name (contains a path \
                     separator / multi-level directory); using the default \
                     dist/<stem>.<ext>"
                )
            }
            CandyWarn::DuplicateName(kind, name, _) => {
                format!(
                    "parse: {kind} name '{name}' is redefined in the same lexical scope; the \
                     later definition shadows the earlier one (redefining inside a \
                     nested scope is legitimate Typst shadowing and is not warned)"
                )
            }
            CandyWarn::CallingPrivate(name, _) => {
                format!("parse: `#{name}` is a private Candy helper, not part of the public API")
            }
            CandyWarn::Interpolation(e) => format!("interpolator: {e}"),
            CandyWarn::KeyframeOffsetClamp(d, _) => {
                format!("parse: {d}")
            }
            CandyWarn::ContentOverflow(d) => {
                format!(
                    "render: {d}; content beyond the viewport is still rendered \
                     but clipped at rasterization by the fixed `viewBox`, so it \
                     isn't shown"
                )
            }
            CandyWarn::UnknownDirective(name, _) => {
                format!("parse: unknown directive `#{name}`; candy does not recognize this name")
            }
        }
    }

    /// The source location tied to this warning, if any. Rendered by the `warn!`
    /// reporter after the message so the user is pointed at the offending code.
    pub fn loc(&self) -> Option<&SourceLoc> {
        match self {
            CandyWarn::DuplicateName(_, _, l) => Some(l),
            CandyWarn::CallingPrivate(_, l) => Some(l),
            CandyWarn::UnknownEasing(_, l) => Some(l),
            CandyWarn::RevealFallback(_, l) => Some(l),
            CandyWarn::KeyframeOffsetClamp(_, l) => Some(l),
            CandyWarn::UnknownDirective(_, l) => Some(l),
            _ => None,
        }
    }

    /// An optional one-line `hint:` for this warning, surfaced after the source
    /// snippet (rustc / `hint:` style). Mirrors [`CandyError::hint`]: only the
    /// source-localizable warnings carry one, pointing at the most likely fix.
    pub fn hint(&self) -> Option<&'static str> {
        match self {
            CandyWarn::DuplicateName(_, _, _) => Some(
                "rename one of the definitions, or move the later one into a nested scope if \
                 shadowing is intended",
            ),
            CandyWarn::CallingPrivate(_, _) => Some(
                "use the public API instead — drop the leading `_` and call the documented \
                 helper, or import it from the candy package",
            ),
            CandyWarn::UnknownEasing(_, _) => Some(
                "see the candy docs for the list of supported easing names (matched \
                 case-insensitively); defaulting to linear",
            ),
            CandyWarn::RevealFallback(_, _) => Some(
                "pass a string literal (\"...\") as the body so char/word reveal can measure its \
                 length; falling back to FadeIn",
            ),
            CandyWarn::ContentOverflow(_) => Some(
                "shrink the content or position mobjects with absolute `to:` coordinates so it \
                fits the viewport, or split it into multiple #scene blocks",
            ),
            CandyWarn::UnknownDirective(_, _) => Some(
                "check the spelling — candy directives use kebab-case (e.g. `save-state`, \
                 `set-color`, `fade-transform`); see the docs for the full list",
            ),
            _ => None,
        }
    }
}
impl fmt::Display for CandyWarn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}] {}", self.code(), self.message())
    }
}

// ============================ Reporters (macros) =========================
// All four reporters are **macros** (not functions) so call sites read like
// `eprintln!`/`println!` without wrapping every message in `format!`. Each is
// `#[macro_export]`ed, so it is available at the crate root: `crate::error!`,
// `crate::warn!`, `crate::debug!`, `crate::info!` from within the lib, and
// `candy::error!` etc. from the `candy` binary.

/// Base for fatal-error exit codes.
/// On Unix the process exit status is an 8-bit value (0–255); any code above
/// 255 is truncated (`code & 0xFF`), which is why the old `1000 + n` scheme was
/// unusable on Linux (our primary platform). Every fatal code is therefore kept
/// ≤ 255.
/// Allocation (must not collide with anything else candy emits):
///   - `0`     success
///   - `1`     generic / catch-all error
///   - `2`     clap usage error (argument parsing)
///   - `101`   Rust panic — also avoided (not in our range)
///   - `64..`  candy fatal errors: `ERROR_EXIT_BASE + number() - 1`
///     (`E001` → `64` … `E009` → `72`, `E010` → `73`; the `64` prefix is the
///     requested segment; room up to ~`E014` before 78)
///   - `111`   batch failure: `candy build a.tyx b.tyx …` ran every input but at
///     least one failed midway. Individual failures keep their own `E00x`
///     code for logging, but the overall process exit code is forced to
///     `111` so a CI pipeline / shell script can detect "some inputs
///     failed" without aborting the remaining inputs.
pub const ERROR_EXIT_BASE: i32 = 64;

/// Process exit code used when a **batch** of inputs was attempted but at least
/// one input failed partway through. See [`ERROR_EXIT_BASE`] for the full
/// allocation table.
pub const BATCH_ERROR_EXIT: i32 = 111;

/// Color a level label with the given caret color. The returned string always
/// carries ANSI codes; the `anstream`-backed writers strip them off a TTY or
/// under `NO_COLOR`, so piped / captured output stays ANSI-free.
fn paint_level(label: &str, color: Color) -> String {
    paint(style_caret(color), label)
}

/// Colored `info` level prefix (stdout).
pub fn level_info() -> String {
    paint_level("info", Color::Green)
}
/// Colored `debug` level prefix (stdout).
pub fn level_debug() -> String {
    paint_level("debug", Color::BrightBlack)
}

/// Build the cargo/rustc-style level+code head token, e.g. `error[E002]:`
/// (red + bold) or `warn[W014]:` (yellow + bold). Mirrors the way rustc prints
/// `error[E0308]:` / `warning: ...`. The returned string always carries ANSI
/// codes; the `anstream`-backed writers strip them off a TTY or under `NO_COLOR`
/// so piped / captured output stays ANSI-free. Used by the `error!` / `warn!`
/// macros and by the batch-failure summary in `main`.
pub fn paint_err_head(level: &str, code: &str, color: Color) -> String {
    let token = format!("{}[{}]:", level, code);
    paint(style_caret(color), &token)
}

/// Render an error's source location with a **red** caret (used by `error!`).
/// The returned string always carries ANSI codes; the `anstream`-backed writers
/// strip them when the destination isn't a terminal or `NO_COLOR` is set.
pub fn render_error_loc(loc: &SourceLoc) -> String {
    let mut s = loc.render_colored(Color::Red);
    if !loc.include_trace.is_empty() {
        s.push_str(&render_include_trace(&loc.include_trace));
    }
    s
}

/// Render the outer frames of a nested-include trace as a sequence of
/// `= included from \`path:line:col\`:` notes, each followed by a cargo-style
/// caret block pointing at that includer's `#include` line. Used by
/// [`render_error_loc`] so an error that originates deep inside a `a → b → c`
/// include chain is not collapsed to a single line.
fn render_include_trace(trace: &[SourceLoc]) -> String {
    let mut out = String::new();
    for frame in trace {
        out.push_str(&format!(
            "\n   = included from `{}:{}:{}`:",
            frame.path.display(),
            frame.line,
            frame.col
        ));
        out.push('\n');
        out.push_str(&frame.render_colored(Color::Red));
    }
    out
}

/// Print a cargo/rustc-style build-status line to **stdout**:
/// ```text
///    Compiling candy v0.1.0 (/path)
///     Running `ffmpeg ...`
///    Building example.tyx
///    Checking example.tyx
/// ```
/// The verb is right-aligned in a 12-column field so the message text always
/// starts at the same column, exactly like cargo build. On a TTY the verb is
/// colored green and bold; otherwise, or when NO_COLOR is set, the output
/// stays free of ANSI codes. The build, check, migrate, and encode progress
/// all surface through these lines, so the build output reads like cargo build.
/// Defined as a macro (like `println!`) so call sites pass args directly; the
/// verb is right-aligned to 12 columns and painted green+bold on a TTY.
#[macro_export]
macro_rules! cargo_status {
    ($verb:expr, $($message:tt)*) => {{
        let __padded = ::std::format!("{:>12}", $verb);
        let __verb_str = $crate::core::diag::paint(
            $crate::core::diag::style_green_bold(),
            &__padded,
        );
        let __msg = ::std::format!($($message)*);
        $crate::print_styled!("{} {}", __verb_str, __msg);
    }};
}

/// Print the cargo/rustc-style final summary line to **stdout**:
/// ```text
///     Finished `dev` profile [unoptimized + debuginfo] target(s) in 1.26s
///     Finished example.mp4 in 2.34s
/// ```
/// `label` is the thing that finished (e.g. the output path or `N animations`);
/// `elapsed` is the wall-clock time, rendered as `in X.XXs`. Only the
/// `Finished` verb is colored **green** + bold on a TTY; the rest of the line
/// (including the `in X.XXs` timing) stays plain — exactly how cargo prints
/// `Finished \`dev\` profile … target(s) in 1.26s`. Off a TTY or under
/// `NO_COLOR` the whole line is plain ANSI-free text. Mirrors the `Finished`
/// line cargo prints at the end of every build. Defined as a macro (like
/// `println!`) so call sites pass args directly.
#[macro_export]
macro_rules! cargo_finished {
    ($elapsed:expr, $($label:tt)*) => {{
        let __secs = ($elapsed).as_secs_f64();
        // Pad the visible verb to 12 columns *before* coloring (same reason as
        // [`cargo_status`]).
        let __padded = ::std::format!("{:>12}", "Finished");
        let __fin = $crate::core::diag::paint($crate::core::diag::style_green_bold(), &__padded);
        let __label = ::std::format!($($label)*);
        $crate::print_styled!("{} {} in {:.2}s", __fin, __label, __secs);
    }};
}

/// Print an in-place cargo/rustc-style progress line to **stdout** (carriage
/// return, no trailing newline):
/// ```text
///     Frame scene 1/3  frame 12/45  (12/61)
/// ```
/// Mirrors the way cargo refreshes its progress bars / spinners: the verb is
/// right-aligned in a 12-column field (green + bold on a TTY, like
/// [`cargo_status`]) and the cursor returns to column 0 on the *same* line, so
/// the next call overwrites it. The caller must print a newline (`println!()`)
/// once the unit is done, so the next discrete status line (and the final
/// `Finished`) starts on a fresh line. This helper is a no-op when stdout is not
/// a TTY or `NO_COLOR` is set, so piped / captured output stays clean — cargo
/// likewise suppresses progress bars off a TTY.
pub fn cargo_progress(verb: &str, message: &str) {
    if !std::io::stdout().is_terminal() {
        return;
    }
    // Right-align the *visible* verb first, then color (same reason as
    // [`cargo_status`]); `\r` returns to column 0 of the current line so the
    // next `cargo_progress` call overwrites this one. `anstream` additionally
    // strips the color under `NO_COLOR`; the line is flushed so the in-place
    // refresh is visible immediately.
    let padded = format!("{verb:>12}");
    let verb_str = paint(style_green_bold(), &padded);
    let mut s = AutoStream::new(std::io::stdout(), ColorChoice::Auto);
    let _ = write!(s, "\r{verb_str} {message}");
    let _ = s.flush();
}

/// Render a warning's source location with a **yellow** caret (used by `warn!`).
/// The returned string always carries ANSI codes; the `anstream`-backed writers
/// strip them when the destination isn't a terminal or `NO_COLOR` is set.
pub fn render_warn_loc(loc: &SourceLoc) -> String {
    loc.render_colored(Color::Yellow)
}

// ===================== Stream writers (anstream) =====================
// Every diagnostic string we build carries ANSI codes unconditionally. These
// two helpers write such a string through an `anstream::AutoStream` with
// `ColorChoice::Auto`, which strips the codes automatically when the
// destination is not a terminal or `NO_COLOR` (https://no-color.org) is set —
// replacing the old per-call `is_tty && NO_COLOR.is_none()` checks.

/// Write a fully-styled line to **stderr**, stripping ANSI when not a TTY /
/// `NO_COLOR`. Flushes so a following `process::exit` cannot drop the bytes.
/// Defined as a macro (like `eprintln!`) so call sites pass format args
/// directly without wrapping every message in `format!`. The `AutoStream`
/// writer strips the ANSI codes when the destination isn't a terminal or
/// `NO_COLOR` (https://no-color.org) is set, so the same code path serves both
/// colored (TTY) and plain (piped / CI) output.
#[macro_export]
macro_rules! eprint_styled {
    ($($arg:tt)*) => {{
        use ::std::io::Write as _;
        let mut __w = $crate::core::diag::AutoStream::new(
            ::std::io::stderr(),
            $crate::core::diag::ColorChoice::Auto,
        );
        let _ = ::std::writeln!(__w, "{}", ::std::format!($($arg)*));
        let _ = ::std::io::Write::flush(&mut __w);
    }};
}

/// Write a fully-styled line to **stdout**, stripping ANSI when not a TTY /
/// `NO_COLOR`. See [`eprint_styled`] for the styling/ANSI semantics.
#[macro_export]
macro_rules! print_styled {
    ($($arg:tt)*) => {{
        use ::std::io::Write as _;
        let mut __w = $crate::core::diag::AutoStream::new(
            ::std::io::stdout(),
            $crate::core::diag::ColorChoice::Auto,
        );
        let _ = ::std::writeln!(__w, "{}", ::std::format!($($arg)*));
        let _ = ::std::io::Write::flush(&mut __w);
    }};
}

/// Bold style (ANSI codes always emitted; stripped off-TTY by the writers).
pub fn style_bold() -> Style {
    Style::new().bold()
}

/// Bold a string (ANSI codes always emitted; stripped off-TTY by the writers).
/// Defined as a macro (like `format!`) so call sites pass args directly without
/// pre-wrapping in `format!`; the `AutoStream` writers strip the codes off a
/// TTY / under `NO_COLOR`.
#[macro_export]
macro_rules! bold {
    ($($arg:tt)*) => {
        $crate::core::diag::paint($crate::core::diag::style_bold(), &::std::format!($($arg)*))
    };
}

/// Green style (ANSI always emitted; stripped off-TTY by the writers).
#[macro_export]
macro_rules! green {
    ($($arg:tt)*) => {
        $crate::core::diag::paint($crate::core::diag::style_green(), &::std::format!($($arg)*))
    };
}

/// Blue style (ANSI always emitted; stripped off-TTY by the writers).
#[macro_export]
macro_rules! blue {
    ($($arg:tt)*) => {
        $crate::core::diag::paint($crate::core::diag::style_blue(), &::std::format!($($arg)*))
    };
}

/// Yellow style (ANSI always emitted; stripped off-TTY by the writers).
#[macro_export]
macro_rules! yellow {
    ($($arg:tt)*) => {
        $crate::core::diag::paint($crate::core::diag::style_yellow(), &::std::format!($($arg)*))
    };
}

/// Red style (ANSI always emitted; stripped off-TTY by the writers).
#[macro_export]
macro_rules! red {
    ($($arg:tt)*) => {
        $crate::core::diag::paint($crate::core::diag::style_red(), &::std::format!($($arg)*))
    };
}

/// Dim (bright-black) style (ANSI always emitted; stripped off-TTY by the writers).
#[macro_export]
macro_rules! dim {
    ($($arg:tt)*) => {
        $crate::core::diag::paint($crate::core::diag::style_dim(), &::std::format!($($arg)*))
    };
}

/// Print a fatal-style error to stderr **without exiting**. Used by batch mode
/// to surface each input's failure in real time while the remaining inputs keep
/// building; the final batch summary (cargo-style) is printed once at the end.
pub fn report_error(e: &CandyError) {
    let head = paint_err_head("error", e.code(), Color::Red);
    let mut line = format!("{} {}", head, bold!("{}", e.message()));
    if let Some(loc) = e.loc() {
        line.push('\n');
        line.push_str(&render_error_loc(loc));
    }
    if let Some(h) = e.hint() {
        line.push('\n');
        line.push_str(&format!("  {} {}", bold!("{}:", "hint"), h));
    }
    eprint_styled!("{}", line);
}

/// Fatal error — the "panic" path. Prints `error[Exxx]: <message>` to
/// **stderr** in the cargo/rustc style (the `error[Exxx]:` head is red + bold on
/// a TTY) and terminates the process with the error's exit code
/// ([`CandyError::exit_code`]: `E001` → `64` … `E011` → `74`, with `E008` → `71`
/// as the fixed easter-egg slot, and the special `EYEE` → `111`). Invoked
/// exactly once at the process boundary (see `main`).
#[macro_export]
macro_rules! error {
    ($err:expr $(,)?) => {{
        let __e = &$err;
        let __head =
            $crate::core::diag::paint_err_head("error", __e.code(), $crate::core::diag::Color::Red);
        let mut __line = ::std::format!("{} {}", __head, $crate::bold!("{}", __e.message()));
        if let Some(__loc) = __e.loc() {
            __line.push('\n');
            __line.push_str(&$crate::core::diag::render_error_loc(__loc));
        }
        if let Some(__h) = __e.hint() {
            __line.push('\n');
            __line.push_str(&::std::format!(
                "  {} {}",
                $crate::bold!("{}:", "hint"),
                __h
            ));
        }
        $crate::core::diag::eprint_styled!("{}", __line);
        ::std::process::exit($crate::core::diag::CandyError::exit_code(__e));
    }};
}

/// Non-fatal warning. Prints `warn[Wxxx]: <message>` to **stderr** in the
/// cargo/rustc style (the `warn[Wxxx]:` head is yellow + bold on a TTY) and
/// returns normally so the render continues.
#[macro_export]
macro_rules! warn {
    ($w:expr $(,)?) => {{
        let __w: $crate::core::diag::CandyWarn = $w;
        let __head = $crate::core::diag::paint_err_head(
            "warn",
            __w.code(),
            $crate::core::diag::Color::Yellow,
        );
        let mut __line = ::std::format!("{} {}", __head, $crate::bold!("{}", __w.message()));
        if let Some(__loc) = __w.loc() {
            __line.push('\n');
            __line.push_str(&$crate::core::diag::render_warn_loc(__loc));
        }
        if let Some(__h) = __w.hint() {
            __line.push('\n');
            __line.push_str(&::std::format!(
                "  {} {}",
                $crate::bold!("{}:", "hint"),
                __h
            ));
        }
        $crate::core::diag::eprint_styled!("{}", __line);
    }};
}

/// Developer diagnostic. Prints `debug: <message>` to **stdout** (the `debug`
/// prefix is colored dim on a TTY; no code).
#[macro_export]
macro_rules! debug {
    ($($arg:tt)*) => {{
        $crate::core::diag::print_styled!(
            "{}: {}",
            $crate::core::diag::level_debug(),
            format_args!($($arg)*)
        );
    }};
}

/// User-facing progress. Prints `info: <message>` to **stdout** (the `info`
/// prefix is colored green on a TTY; no code).
#[macro_export]
macro_rules! info {
    ($($arg:tt)*) => {{
        $crate::core::diag::print_styled!(
            "{}: {}",
            $crate::core::diag::level_info(),
            format_args!($($arg)*)
        );
    }};
}
