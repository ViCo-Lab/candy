//! Unified diagnostics. **All** diagnostic output in candy flows through this
//! module so it can be routed and coded consistently:
//!
//! | Level  | Stream  | Code | Behavior                                            |
//! |--------|---------|------|-----------------------------------------------------|
//! | Error  | stderr  | `E`  | print, then terminate (exit code `64`–`70`, e.g. `E001` → `64`) |
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
use std::io::IsTerminal;
use std::path::PathBuf;

/// `Color` is re-exported (pub) so the `error!` / `warn!` macros can refer to
/// the caret color (`$crate::core::diag::Color::Red` / `::Yellow`) without
/// naming the `colored` crate directly at every call site.
pub use colored::{Color, Colorize};

use crate::core::ast::Label;

// ============================== SourceLoc ==============================
//
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
        }
    }

    /// Render as:
    /// ```text
    /// path:line:col
    ///   <line_text>
    ///   <spaces>^^^^^
    /// ```
    pub fn render(&self) -> String {
        let line_len = self.line_text.chars().count();
        let avail = line_len.saturating_sub(self.col.saturating_sub(1)).max(1);
        let caret_len = self.char_span.clamp(1, avail);
        let indent = " ".repeat(self.col.saturating_sub(1));
        let caret = "^".repeat(caret_len);
        format!(
            "{}:{}:{}\n  {}\n  {}{}",
            self.path.display(),
            self.line,
            self.col,
            self.line_text,
            indent,
            caret
        )
    }

    /// Render with color: the `path:line:col` header in **cyan** and the caret
    /// in `caret_color` (the level color — red for errors, yellow for warnings).
    /// Only applies when `is_tty` is true and `NO_COLOR` (https://no-color.org)
    /// is unset; otherwise falls back to the plain [`SourceLoc::render`] so
    /// piped / captured output stays ANSI-free (and matches the uncolored
    /// `error!` / `warn!` behavior on non-terminals).
    pub fn render_colored(&self, caret_color: Color, is_tty: bool) -> String {
        if !is_tty || std::env::var_os("NO_COLOR").is_some() {
            return self.render();
        }
        let line_len = self.line_text.chars().count();
        let avail = line_len.saturating_sub(self.col.saturating_sub(1)).max(1);
        let caret_len = self.char_span.clamp(1, avail);
        let indent = " ".repeat(self.col.saturating_sub(1));
        let caret = "^".repeat(caret_len);
        let header = format!("{}:{}:{}", self.path.display(), self.line, self.col)
            .color(Color::Cyan)
            .bold()
            .to_string();
        format!(
            "{}\n  {}\n  {}{}",
            header,
            self.line_text,
            indent,
            caret.color(caret_color).bold()
        )
    }
}

// ============================== Error (E) ==============================

/// Candy's unified error type. The [`CandyError::code`] method maps each
/// variant to the mandatory error codes E001–E010.
#[derive(Debug)]
pub enum CandyError {
    /// E001 — `.tyx` file not found / generic I/O failure.
    Io(std::io::Error),
    /// E002 — Invalid `.tyx` syntax. Carries the offending source location
    /// when the failure can be tied to a specific span.
    Parse(String, Option<SourceLoc>),
    /// E003 — `candy-json` missing/invalid (SVG extraction).
    Svg(String),
    /// E004 — `@label` not found in the Typst layout. Carries the label's
    /// declaration location when known (so the user sees where it was defined).
    LabelNotFound(Label, Option<SourceLoc>),
    /// E005 — Invalid interpolation range (clamped, not fatal).
    Interpolation(String),
    /// E006 — Typst render failure. Carries the offending source location
    /// (file:line:col + the offending line) when the failure can be tied to a
    /// specific span in the compiled Typst source, so the user is pointed at the
    /// exact code that failed to compile — just like the parser-level errors
    /// (E002/E004/…). Without a resolvable span (e.g. an internal Typst panic)
    /// the location is `None`.
    Typst(String, Option<SourceLoc>),
    /// E007 — Rav1e / codec / mux encoding failure.
    Encode(String),
    /// E008 — The `.tyx` does not import the candy package (or
    /// imports it with a version that does not match the installed candy CLI
    /// version), so its static content has no scene to own it. Candy can only
    /// render documents that import `@<namespace>/candy:<version>` with a
    /// matching version (compiled in from `CARGO_PKG_VERSION`). A bare Typst
    /// document, a file-style import (`#import "candy"`), or a version mismatch
    /// all trigger this error. Pass `--ignore-version` to skip the version
    /// check (useful for development).
    CandyDumpedYou(String, Option<SourceLoc>),
    /// E009 — A key reference (`@label`, `target:`, `animate(target:)`, etc.)
    /// points to a mobject that was never registered via `#mobject`. Also used
    /// when `ecval(...)` or lifecycle events (`ecpause`, `ecdestroy`,
    /// …) reference an unknown counter name. The first field is the kind
    /// (`"mobject"` / `"ecnew"` / `"scene"`) and the second is the offending
    /// key name.
    UnknownKey(String, String, Option<SourceLoc>),
    /// E010 — A key parameter evaluated to a non-string type (e.g., number,
    /// boolean, array). Keys must always resolve to strings.
    InvalidKey(String, Option<SourceLoc>),
    /// EYEE — Batch partial failure: `candy build a.tyx b.tyx …` ran every
    /// input but at least one failed midway. Surfaced as the "yee~ Batch
    /// failed. \\(!_!)/" marker. **Deliberately does NOT follow** the `ERROR_EXIT_BASE +
    /// n - 1` scheme used by E001–E007 — its process exit code is the dedicated
    /// [`BATCH_ERROR_EXIT`] (111) instead, so a CI pipeline / shell script can
    /// detect "some inputs failed" without aborting the remaining inputs.
    Yee(String),
}

impl CandyError {
    /// Mandatory error code (E001–E010).
    pub fn code(&self) -> &'static str {
        match self {
            CandyError::Yee(_) => "EYEE",
            CandyError::Io(_) => "E001",
            CandyError::Parse(_, _) => "E002",
            CandyError::Svg(_) => "E003",
            CandyError::LabelNotFound(_, _) => "E004",
            CandyError::Interpolation(_) => "E005",
            CandyError::Typst(_, _) => "E006",
            CandyError::Encode(_) => "E007",
            CandyError::CandyDumpedYou(_, _) => "E008",
            CandyError::UnknownKey(_, _, _) => "E009",
            CandyError::InvalidKey(_, _) => "E010",
        }
    }

    /// Numeric part of the code (1–11), used to build the process exit code for
    /// the E001–E010 family. `EYEE` is excluded here on purpose — it carries no
    /// `64`-based number (see [`CandyError::exit_code`]).
    pub fn number(&self) -> u8 {
        match self {
            CandyError::Yee(_) => 111,
            CandyError::Io(_) => 1,
            CandyError::Parse(_, _) => 2,
            CandyError::Svg(_) => 3,
            CandyError::LabelNotFound(_, _) => 4,
            CandyError::Interpolation(_) => 5,
            CandyError::Typst(_, _) => 6,
            CandyError::Encode(_) => 7,
            CandyError::CandyDumpedYou(_, _) => 8,
            CandyError::UnknownKey(_, _, _) => 9,
            CandyError::InvalidKey(_, _) => 10,
        }
    }

    /// Process exit code for this error.
    ///
    /// The E001–E010 family follows `ERROR_EXIT_BASE + n - 1` (`E001` → `64` …
    /// `E007` → `70`). `EYEE` is the **one exception**: it bypasses that scheme
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
            CandyError::Parse(e, _) => format!("Invalid .tyx syntax: {e}"),
            CandyError::Svg(e) => format!("candy-json missing/invalid: {e}"),
            CandyError::LabelNotFound(l, _) => {
                format!("label @{} not found in Typst layout", l.0)
            }
            CandyError::Interpolation(e) => format!("interpolation range: {e}"),
            CandyError::Typst(e, _) => format!("Typst render failure: {e}"),
            CandyError::Encode(e) => format!("encode failure: {e}"),
            CandyError::CandyDumpedYou(e, _) => format!("candy package not imported: {e}"),
            CandyError::UnknownKey(kind, key, _) => {
                format!("{kind} \"{key}\" does not exist (never declared or already destroyed)")
            }
            CandyError::InvalidKey(val_type, _) => {
                format!("key must be a string, got {val_type}")
            }
            CandyError::Yee(e) => e.to_string(),
        }
    }

    /// The source location tied to this error, if any. Rendered by the `error!`
    /// reporter after the message so the user is pointed at the offending code.
    pub fn loc(&self) -> Option<&SourceLoc> {
        match self {
            CandyError::LabelNotFound(_, l) => l.as_ref(),
            CandyError::Parse(_, l) => l.as_ref(),
            CandyError::CandyDumpedYou(_, l) => l.as_ref(),
            CandyError::UnknownKey(_, _, l) => l.as_ref(),
            CandyError::InvalidKey(_, l) => l.as_ref(),
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

// ===================== Typst Error capture (E006) ======================
//
// A Typst compile yields `typst::ecow::EcoVec<typst::diag::SourceDiagnostic>`
// (the error half of `typst::SourceResult<T>`). This `From` impl lets any
// `?` on a Typst result be captured uniformly as `CandyError::Typst` and thus
// assigned the mandatory `E006` code, instead of every call site hand-rolling
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
    UnknownEasing(String),
    /// W010 — A `#reveal` body was not a string literal; falling back to
    /// `FadeIn`.
    RevealFallback(String),
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
    ///
    /// Field: the private function name (e.g. `"_assert_str"`).
    CallingPrivate(String),
}

impl CandyWarn {
    /// Mandatory warning code (W001–W015).
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
            CandyWarn::UnknownEasing(_) => "W009",
            CandyWarn::RevealFallback(_) => "W010",
            CandyWarn::CleanupFailed(_) => "W011",
            CandyWarn::OutputNameCountMismatch(_) => "W012",
            CandyWarn::OutputNameInvalid(_) => "W013",
            CandyWarn::DuplicateName(_, _, _) => "W014",
            CandyWarn::CallingPrivate(_) => "W015",
        }
    }

    /// The human-readable message, WITHOUT the `[Wxxx]` code prefix. The `warn!`
    /// macro renders this separately from the code so the code can be shown bold
    /// + colored while the message stays plain.
    pub fn message(&self) -> String {
        match self {
            CandyWarn::TimeDependent => ".tyx uses the current date/time \
                 (datetime.today()); the render depends on the wall clock and is \
                 not reproducible"
                .into(),
            CandyWarn::GpuUnavailable(e) => {
                format!("GPU unavailable, falling back to CPU: {e}")
            }
            CandyWarn::GpuFeatureDisabled => "--gpu requested but candy was built \
                 without the 'gpu' feature; using CPU"
                .into(),
            CandyWarn::EncodeFallback(d) => {
                format!("encode failed, wrote SVG draft to .candy: {d}")
            }
            CandyWarn::CodecFallback(d) => {
                format!("codec encode failed, falling back: {d}")
            }
            CandyWarn::AudioDropped(d) => format!("dropping audio track: {d}"),
            CandyWarn::EncodeRetry => "rav1e inter-prediction panicked; retrying \
                 AV1 in all-intra mode (valid but no temporal compression)"
                .into(),
            CandyWarn::AudioIgnored => "MP4 only muxes AAC audio; ignoring non-AAC track".into(),
            CandyWarn::UnknownEasing(d) => {
                format!("unknown easing {d}; falling back to linear")
            }
            CandyWarn::RevealFallback(d) => {
                format!("#reveal body is not a string literal; falling back to FadeIn: {d}")
            }
            CandyWarn::CleanupFailed(d) => {
                format!("could not remove intermediate dir {d}")
            }
            CandyWarn::OutputNameCountMismatch(d) => {
                format!(
                    "{d}; ignoring custom --output names and using the default \
                     dist/<stem>.<ext> for every input"
                )
            }
            CandyWarn::OutputNameInvalid(d) => {
                format!(
                    "--output name '{d}' is not a plain file name (contains a path \
                     separator / multi-level directory); using the default \
                     dist/<stem>.<ext>"
                )
            }
            CandyWarn::DuplicateName(kind, name, _) => {
                format!(
                    "{kind} name '{name}' is redefined in the same lexical scope; the \
                     later definition shadows the earlier one (redefining inside a \
                     nested scope is legitimate Typst shadowing and is not warned)"
                )
            }
            CandyWarn::CallingPrivate(name) => {
                format!("`#{name}` is a private Candy helper, not part of the public API")
            }
        }
    }

    /// The source location tied to this warning, if any. Rendered by the `warn!`
    /// reporter after the message so the user is pointed at the offending code.
    pub fn loc(&self) -> Option<&SourceLoc> {
        match self {
            CandyWarn::DuplicateName(_, _, l) => Some(l),
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
//
// All four reporters are **macros** (not functions) so call sites read like
// `eprintln!`/`println!` without wrapping every message in `format!`. Each is
// `#[macro_export]`ed, so it is available at the crate root: `crate::error!`,
// `crate::warn!`, `crate::debug!`, `crate::info!` from within the lib, and
// `candy::error!` etc. from the `candy` binary.

/// Base for fatal-error exit codes.
///
/// On Unix the process exit status is an 8-bit value (0–255); any code above
/// 255 is truncated (`code & 0xFF`), which is why the old `1000 + n` scheme was
/// unusable on Linux (our primary platform). Every fatal code is therefore kept
/// ≤ 255.
///
/// Allocation (must not collide with anything else candy emits):
///   - `0`     success
///   - `1`     generic / catch-all error
///   - `2`     clap usage error (argument parsing)
///   - `101`   Rust panic — also avoided (not in our range)
///   - `64..`  candy fatal errors: `ERROR_EXIT_BASE + number() - 1`
///     (`E001` → `64` … `E007` → `70`; the `64` prefix is the requested
///     segment; room up to ~`E014` before 78)
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

/// Color a level label for a stream, but only when that stream is a terminal
/// and `NO_COLOR` (https://no-color.org) is unset. Returns the plain label
/// otherwise, so piped / captured output stays ANSI-free (and tests / CI, where
/// the streams are not TTYs, see exactly the old uncolored text).
fn paint_level(label: &str, color: Color, is_tty: bool) -> String {
    if is_tty && std::env::var_os("NO_COLOR").is_none() {
        label.color(color).bold().to_string()
    } else {
        label.to_string()
    }
}

/// Colored `error` level prefix (stderr).
pub fn level_error() -> String {
    paint_level("error", Color::Red, std::io::stderr().is_terminal())
}
/// Colored `warn` level prefix (stderr).
pub fn level_warn() -> String {
    paint_level("warn", Color::Yellow, std::io::stderr().is_terminal())
}
/// Colored `info` level prefix (stdout).
pub fn level_info() -> String {
    paint_level("info", Color::Green, std::io::stdout().is_terminal())
}
/// Colored `debug` level prefix (stdout).
pub fn level_debug() -> String {
    paint_level("debug", Color::BrightBlack, std::io::stdout().is_terminal())
}

/// Color a `[code]` token bold in `color`, but only when the target stream is a
/// terminal and `NO_COLOR` (https://no-color.org) is unset; otherwise returns the
/// plain `[code]`. Shared by the `error!` / `warn!` macros so the `Exxx` / `Wxxx`
/// code is rendered bold in its level color (errors red, warnings yellow).
fn paint_code(code: &str, color: Color, is_tty: bool) -> String {
    if is_tty && std::env::var_os("NO_COLOR").is_none() {
        format!("[{}]", code).color(color).bold().to_string()
    } else {
        format!("[{}]", code)
    }
}

/// Bold error code `[Exxx]` / `[EYEE]` in red (stderr). TTY + NO_COLOR aware.
pub fn code_error(code: &str) -> String {
    paint_code(code, Color::Red, std::io::stderr().is_terminal())
}

/// Bold warning code `[Wxxx]` in yellow (stderr). TTY + NO_COLOR aware.
pub fn code_warn(code: &str) -> String {
    paint_code(code, Color::Yellow, std::io::stderr().is_terminal())
}

/// Render an error's source location with a **red** caret (used by `error!`).
/// TTY + `NO_COLOR` detection happens here (inside this module, where the
/// `IsTerminal` trait is in scope) so call sites don't need to import it.
pub fn render_error_loc(loc: &SourceLoc) -> String {
    loc.render_colored(Color::Red, std::io::stderr().is_terminal())
}

/// Render a warning's source location with a **yellow** caret (used by `warn!`).
/// See [`render_error_loc`] for why the TTY check lives here.
pub fn render_warn_loc(loc: &SourceLoc) -> String {
    loc.render_colored(Color::Yellow, std::io::stderr().is_terminal())
}

/// Fatal error — the "panic" path. Prints `error: [Exxx] <message>` to
/// **stderr** (the `error` prefix and the `[Exxx]` code are both red + bold on a
/// TTY) and terminates the process with the error's exit code
/// ([`CandyError::exit_code`]: `E001` → `64` … `E008` → `71`, and the special
/// `EYEE` → `111`). Invoked exactly once at the process boundary (see `main`).
#[macro_export]
macro_rules! error {
    ($err:expr $(,)?) => {{
        let __e = &$err;
        let mut __line = ::std::format!(
            "{}: {} {}",
            $crate::core::diag::level_error(),
            $crate::core::diag::code_error(__e.code()),
            __e.message()
        );
        if let Some(__loc) = __e.loc() {
            __line.push_str(&::std::format!(
                "\n{}",
                $crate::core::diag::render_error_loc(__loc)
            ));
        }
        ::std::eprintln!("{}", __line);
        ::std::process::exit($crate::core::diag::CandyError::exit_code(__e));
    }};
}

/// Non-fatal warning. Prints `warn: [Wxxx] <message>` to **stderr** (the `warn`
/// prefix and the `[Wxxx]` code are both yellow + bold on a TTY) and returns
/// normally so the render continues.
#[macro_export]
macro_rules! warn {
    ($w:expr $(,)?) => {{
        let __w: $crate::core::diag::CandyWarn = $w;
        let mut __line = ::std::format!(
            "{}: {} {}",
            $crate::core::diag::level_warn(),
            $crate::core::diag::code_warn(__w.code()),
            __w.message()
        );
        if let Some(__loc) = __w.loc() {
            __line.push_str(&::std::format!(
                "\n{}",
                $crate::core::diag::render_warn_loc(__loc)
            ));
        }
        ::std::eprintln!("{}", __line);
    }};
}

/// Developer diagnostic. Prints `debug: <message>` to **stdout** (the `debug`
/// prefix is colored dim on a TTY; no code).
#[macro_export]
macro_rules! debug {
    ($($arg:tt)*) => {{
        ::std::println!(
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
        ::std::println!(
            "{}: {}",
            $crate::core::diag::level_info(),
            format_args!($($arg)*)
        );
    }};
}
