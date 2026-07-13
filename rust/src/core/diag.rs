//! Unified diagnostics. **All** diagnostic output in candy flows through this
//! module so it can be routed and coded consistently:
//!
//! | Level  | Stream  | Code | Behavior                                            |
//! |--------|---------|------|-----------------------------------------------------|
//! | Error  | stderr  | `E`  | print, then terminate (exit code `64`–`70`, e.g. `E001` → `64`) |
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
}

impl CandyError {
    /// Mandatory error code (E001–E007).
    pub fn code(&self) -> &'static str {
        match self {
            CandyError::Io(_) => "E001",
            CandyError::Parse(_) => "E002",
            CandyError::Svg(_) => "E003",
            CandyError::LabelNotFound(_) => "E004",
            CandyError::Interp(_) => "E005",
            CandyError::Typst(_) => "E006",
            CandyError::Encode(_) => "E007",
        }
    }

    /// Numeric part of the code (1–7), used to build the process exit code.
    pub fn number(&self) -> u8 {
        match self {
            CandyError::Io(_) => 1,
            CandyError::Parse(_) => 2,
            CandyError::Svg(_) => 3,
            CandyError::LabelNotFound(_) => 4,
            CandyError::Interp(_) => 5,
            CandyError::Typst(_) => 6,
            CandyError::Encode(_) => 7,
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
pub const ERROR_EXIT_BASE: i32 = 64;

/// Fatal error — the "panic" path. Prints `error: [Exxx] <message>` to
/// **stderr** and terminates the process with exit code
/// `ERROR_EXIT_BASE + n - 1` (`E001` → `64`, `E007` → `70`). Invoked exactly
/// once at the process boundary (see `main`).
#[macro_export]
macro_rules! error {
    ($err:expr $(,)?) => {{
        let __e = &$err;
        ::std::eprintln!("error: {}", __e);
        ::std::process::exit($crate::core::diag::ERROR_EXIT_BASE + __e.number() as i32 - 1);
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
