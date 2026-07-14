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

use crate::core::ast::Label;

// ============================== Error (E) ==============================

/// Candy's unified error type. The [`CandyError::code`] method maps each
/// variant to the mandatory error codes E001–E007.
#[derive(Debug)]
pub enum CandyError {
    /// E001 — `.tyx` file not found / generic I/O failure.
    Io(std::io::Error),
    /// E002 — Invalid `.tyx` syntax.
    Parse(String),
    /// E003 — `candy-json` missing/invalid (SVG extraction).
    Svg(String),
    /// E004 — `@label` not found in the Typst layout.
    LabelNotFound(Label),
    /// E005 — Invalid interpolation range (clamped, not fatal).
    Interp(String),
    /// E006 — Typst render failure.
    Typst(String),
    /// E007 — Rav1e / codec / mux encoding failure.
    Encode(String),
    /// E008 — The `.tyx` does not import the candy package, so its static
    /// (non-candy) content has no scene to own it — not even the implicit root
    /// scene. Candy can only render documents that import `@preview/candy`
    /// (whose root scene then owns all static content). A bare Typst document
    /// run through `candy build` without importing candy is therefore rejected
    /// with this dedicated code rather than producing an empty / garbage output.
    NoCandyImport(String),
    /// EYEE — Batch partial failure: `candy build a.tyx b.tyx …` ran every
    /// input but at least one failed midway. Surfaced as the "yee~ Batch
    /// failed" marker. **Deliberately does NOT follow** the `ERROR_EXIT_BASE +
    /// n - 1` scheme used by E001–E007 — its process exit code is the dedicated
    /// [`BATCH_ERROR_EXIT`] (111) instead, so a CI pipeline / shell script can
    /// detect "some inputs failed" without aborting the remaining inputs.
    Yee(String),
}

impl CandyError {
    /// Mandatory error code (E001–E007).
    pub fn code(&self) -> &'static str {
        match self {
            CandyError::Yee(_) => "EYEE",
            CandyError::Io(_) => "E001",
            CandyError::Parse(_) => "E002",
            CandyError::Svg(_) => "E003",
            CandyError::LabelNotFound(_) => "E004",
            CandyError::Interp(_) => "E005",
            CandyError::Typst(_) => "E006",
            CandyError::Encode(_) => "E007",
            CandyError::NoCandyImport(_) => "E008",
        }
    }

    /// Numeric part of the code (1–7), used to build the process exit code for
    /// the E001–E007 family. `EYEE` is excluded here on purpose — it carries no
    /// `64`-based number (see [`CandyError::exit_code`]).
    pub fn number(&self) -> u8 {
        match self {
            CandyError::Yee(_) => 111,
            CandyError::Io(_) => 1,
            CandyError::Parse(_) => 2,
            CandyError::Svg(_) => 3,
            CandyError::LabelNotFound(_) => 4,
            CandyError::Interp(_) => 5,
            CandyError::Typst(_) => 6,
            CandyError::Encode(_) => 7,
            CandyError::NoCandyImport(_) => 8,
        }
    }

    /// Process exit code for this error.
    ///
    /// The E001–E007 family follows `ERROR_EXIT_BASE + n - 1` (`E001` → `64` …
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
}

impl fmt::Display for CandyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CandyError::Io(e) => write!(f, "[E001] I/O error: {e}"),
            CandyError::Parse(e) => write!(f, "[E002] Invalid .tyx syntax: {e}"),
            CandyError::Svg(e) => write!(f, "[E003] candy-json missing/invalid: {e}"),
            CandyError::LabelNotFound(l) => {
                write!(f, "[E004] label @{} not found in Typst layout", l.0)
            }
            CandyError::Interp(e) => write!(f, "[E005] interpolation range: {e}"),
            CandyError::Typst(e) => write!(f, "[E006] Typst render failure: {e}"),
            CandyError::Encode(e) => write!(f, "[E007] encode failure: {e}"),
            CandyError::NoCandyImport(e) => {
                write!(f, "[E008] candy package not imported: {e}")
            }
            CandyError::Yee(e) => write!(f, "[EYEE] {e}"),
        }
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
        CandyError::Typst(format_typst_errors(&errs))
    }
}

/// Render a collection of Typst [`typst::diag::SourceDiagnostic`] into a
/// single human-readable message (message + any `hint:` lines).
fn format_typst_errors(errs: &TypstErrors) -> String {
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
}

impl CandyWarn {
    /// Mandatory warning code (W001–W011).
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
        }
    }
}

impl fmt::Display for CandyWarn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CandyWarn::TimeDependent => write!(
                f,
                "[W001] .tyx uses the current date/time (datetime.today()); \
                 the render depends on the wall clock and is not reproducible"
            ),
            CandyWarn::GpuUnavailable(e) => {
                write!(f, "[W002] GPU unavailable, falling back to CPU: {e}")
            }
            CandyWarn::GpuFeatureDisabled => write!(
                f,
                "[W003] --gpu requested but candy was built without the 'gpu' feature; using CPU"
            ),
            CandyWarn::EncodeFallback(d) => {
                write!(f, "[W004] encode failed, wrote SVG draft to .candy: {d}")
            }
            CandyWarn::CodecFallback(d) => {
                write!(f, "[W005] codec encode failed, falling back: {d}")
            }
            CandyWarn::AudioDropped(d) => write!(f, "[W006] dropping audio track: {d}"),
            CandyWarn::EncodeRetry => write!(
                f,
                "[W007] rav1e inter-prediction panicked; retrying AV1 in all-intra mode \
                 (valid but no temporal compression)"
            ),
            CandyWarn::AudioIgnored => {
                write!(f, "[W008] MP4 only muxes AAC audio; ignoring non-AAC track")
            }
            CandyWarn::UnknownEasing(d) => {
                write!(f, "[W009] unknown easing {d}; falling back to linear")
            }
            CandyWarn::RevealFallback(d) => write!(
                f,
                "[W010] #reveal body is not a string literal; falling back to FadeIn: {d}"
            ),
            CandyWarn::CleanupFailed(d) => {
                write!(f, "[W011] could not remove intermediate dir {d}")
            }
            CandyWarn::OutputNameCountMismatch(d) => {
                write!(
                    f,
                    "[W012] {d}; ignoring custom --output names and using the default \
                     dist/<stem>.<ext> for every input"
                )
            }
            CandyWarn::OutputNameInvalid(d) => {
                write!(
                    f,
                    "[W013] --output name '{d}' is not a plain file name (contains a path \
                     separator / multi-level directory); using the default dist/<stem>.<ext>"
                )
            }
        }
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
///            (`E001` → `64` … `E007` → `70`; the `64` prefix is the requested
///             segment; room up to ~`E014` before 78)
///   - `111`   batch failure: `candy build a.tyx b.tyx …` ran every input but at
///            least one failed midway. Individual failures keep their own `E00x`
///            code for logging, but the overall process exit code is forced to
///            `111` so a CI pipeline / shell script can detect "some inputs
///            failed" without aborting the remaining inputs.
pub const ERROR_EXIT_BASE: i32 = 64;

/// Process exit code used when a **batch** of inputs was attempted but at least
/// one input failed partway through. See [`ERROR_EXIT_BASE`] for the full
/// allocation table.
pub const BATCH_ERROR_EXIT: i32 = 111;

/// Fatal error — the "panic" path. Prints `error: [Exxx] <message>` to
/// **stderr** and terminates the process with the error's exit code
/// ([`CandyError::exit_code`]: `E001` → `64` … `E007` → `70`, and the special
/// `EYEE` → `111`). Invoked exactly once at the process boundary (see `main`).
#[macro_export]
macro_rules! error {
    ($err:expr $(,)?) => {{
        let __e = &$err;
        ::std::eprintln!("error: {}", __e);
        ::std::process::exit($crate::core::diag::CandyError::exit_code(__e));
    }};
}

/// Non-fatal warning. Prints `warn: [Wxxx] <message>` to **stderr** and
/// returns normally so the render continues.
#[macro_export]
macro_rules! warn {
    ($w:expr $(,)?) => {{
        let __w: $crate::core::diag::CandyWarn = $w;
        ::std::eprintln!("warn: {}", __w);
    }};
}

/// Developer diagnostic. Prints `debug: <message>` to **stdout** (no code).
#[macro_export]
macro_rules! debug {
    ($($arg:tt)*) => {{
        ::std::println!("debug: {}", format_args!($($arg)*));
    }};
}

/// User-facing progress. Prints `info: <message>` to **stdout** (no code).
#[macro_export]
macro_rules! info {
    ($($arg:tt)*) => {{
        ::std::println!("info: {}", format_args!($($arg)*));
    }};
}
