//! Build script: extract build-time metadata from `Cargo.toml` and expose it
//! to the binary as compile-time environment variables.
//!
//! * `CANDY_CODENAME` — release codename from `[package.metadata.candy]`.
//! * `CANDY_GIT_HASH` — short (7-char) git commit hash of the working tree at
//!   build time, with a trailing `*` when the tree deviates from `HEAD` in any
//!   way: staged changes, unstaged edits to tracked files, or untracked
//!   (non-ignored) files. Falls back to `unknown` when the sources are not
//!   inside a git repository (e.g. building from a crates.io tarball) or `git`
//!   is not installed. Surfaced to users via `candy --version` as
//!   `v<version>@<hash>(<codename>)`.
//! * `CANDY_COMPATIBLE_VERSIONS` — the `.tyx` import version gate from
//!   `[package.metadata.tyx].compatible_versions`: a list of semver
//!   requirements (Cargo syntax: `0.1.*`, `^0.1`, `>=0.1, <0.3`, …), joined
//!   with `;` (a separator that never occurs inside a semver requirement —
//!   `,` does, in multi-comparator requirements). Every entry is validated
//!   with `semver::VersionReq::parse` here, so a typo fails the build instead
//!   of silently rejecting every `.tyx` at runtime. If the table is absent or
//!   empty, the gate falls back to exactly the crate version (`=<version>`).
//!
//! Also enables architecture-specific ISA extensions for native builds:
//! - x86_64: x86-64-v3 (AVX2, BMI1/2, FMA, MOVBE, F16C)
//! - aarch64: NEON is always on in AAPCS64 (no extra flags needed)

use std::fs;
use std::process::Command;

use chrono::Utc;

/// Short git hash of `HEAD` (7 chars, matching `git rev-parse --short=7`),
/// suffixed with `*` when the working tree is dirty. Returns `"unknown"`
/// when not inside a git repository or when `git` cannot be run at all.
///
/// "Dirty" deliberately means *any* deviation from `HEAD`: staged changes,
/// unstaged modifications to tracked files, and untracked-but-not-ignored
/// files all count. A build that cannot prove the tree is clean reports it
/// as dirty — a false `*` is merely noisy, whereas a missing `*` is a lie
/// about build provenance.
fn git_hash(manifest_dir: &str) -> String {
    let run = |args: &[&str]| -> Option<std::process::Output> {
        Command::new("git")
            .args(args)
            .current_dir(manifest_dir)
            // Keep the caller's environment from steering us at another repo
            // or a foreign work tree (a stray GIT_DIR/GIT_WORK_TREE/GIT_INDEX_FILE
            // makes the status query describe something other than our sources).
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .env_remove("GIT_OBJECT_DIRECTORY")
            .output()
            .ok()
    };

    let hash = match run(&["rev-parse", "--short=7", "HEAD"]) {
        Some(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        // Not a repo / no commits yet / git missing → no hash to report.
        _ => return "unknown".to_string(),
    };

    // ---- Invalidation ------------------------------------------------------
    // Cargo only re-runs this script when a declared dependency changes, so
    // every input that can flip the dirty flag must be declared. Watching just
    // `HEAD` + `index` is not enough: editing a tracked file outside `rust/`
    // (docs, typst, examples) or creating an untracked file touches neither,
    // and the previous run's clean hash would be reused — the "sometimes
    // reports clean while dirty" bug. Watch the whole work tree instead.
    if let Some(o) = run(&["rev-parse", "--git-dir", "--show-toplevel"]) {
        if o.status.success() {
            let out = String::from_utf8_lossy(&o.stdout);
            let mut lines = out.lines();
            if let Some(git_dir) = lines.next().map(str::trim).filter(|s| !s.is_empty()) {
                // `HEAD` moves on commit/checkout; `index` moves on stage/unstage.
                println!("cargo:rerun-if-changed={git_dir}/HEAD");
                println!("cargo:rerun-if-changed={git_dir}/index");
            }
            if let Some(toplevel) = lines.next().map(str::trim).filter(|s| !s.is_empty()) {
                // Directory-level watch: cargo walks it and re-runs the script
                // when any file in the repository is added, edited or removed,
                // which is exactly the set of events that can flip dirtiness.
                println!("cargo:rerun-if-changed={toplevel}");
            }
        }
    }

    // ---- Dirty detection ---------------------------------------------------
    // `--porcelain` gives a stable, parseable format; any output at all means
    // the tree deviates from HEAD. `--untracked-files=normal` is git's default
    // but is stated explicitly so a user's `status.showUntrackedFiles=no`
    // config cannot silently hide untracked files and under-report dirtiness.
    // `--ignore-submodules=none` likewise keeps submodule drift visible.
    let status = run(&[
        "status",
        "--porcelain",
        "--untracked-files=normal",
        "--ignore-submodules=none",
    ]);
    let dirty = match status {
        Some(o) if o.status.success() => !o.stdout.is_empty(),
        // Git ran but failed, or could not be spawned. We already know we are
        // in a repository (rev-parse succeeded above), so cleanliness is
        // unproven — fail loud rather than silently claiming a clean tree.
        _ => true,
    };
    if dirty { format!("{hash}*") } else { hash }
}

fn main() {
    println!("cargo:rerun-if-changed=Cargo.toml");

    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".to_string());

    // ---- CANDY_GIT_HASH (build provenance, shown in `candy --version`) ----
    println!("cargo:rustc-env=CANDY_GIT_HASH={}", git_hash(&manifest_dir));
    let raw = fs::read_to_string(format!("{manifest_dir}/Cargo.toml")).unwrap_or_default();
    // Real TOML parsing (no hand-rolled line scanning): robust against
    // formatting, indentation and nesting changes in the manifest.
    let manifest: toml::Table = raw
        .parse()
        .expect("Cargo.toml must be valid TOML (build.rs metadata extraction)");
    let metadata = manifest.get("package").and_then(|p| p.get("metadata"));

    // ---- CANDY_CODENAME (easter-egg metadata, [package.metadata.candy]) ----
    let codename = metadata
        .and_then(|m| m.get("candy"))
        .and_then(|c| c.get("codename"))
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    println!("cargo:rustc-env=CANDY_CODENAME={codename}");

    // ---- CANDY_COMPATIBLE_VERSIONS (version gate, [package.metadata.tyx]) ----
    let mut reqs: Vec<String> = metadata
        .and_then(|m| m.get("tyx"))
        .and_then(|t| t.get("compatible_versions"))
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .map(|x| {
                    x.as_str()
                        .expect(
                            "[package.metadata.tyx] compatible_versions entries must be strings",
                        )
                        .to_string()
                })
                .collect()
        })
        .unwrap_or_default();
    if reqs.is_empty() {
        // No table / empty list: gate on exactly the crate's own version.
        let v = std::env::var("CARGO_PKG_VERSION").expect("cargo always sets CARGO_PKG_VERSION");
        reqs.push(format!("={v}"));
    }
    for r in &reqs {
        // Fail the build on an invalid requirement — never ship a gate that
        // can't match anything.
        if let Err(e) = semver::VersionReq::parse(r) {
            panic!(
                "[package.metadata.tyx] compatible_versions entry `{r}` is not \
                 a valid semver requirement: {e}"
            );
        }
        assert!(
            !r.contains(';'),
            "[package.metadata.tyx] compatible_versions entry `{r}` must not contain `;` \
             (used as the baked-in list separator)"
        );
    }
    println!(
        "cargo:rustc-env=CANDY_COMPATIBLE_VERSIONS={}",
        reqs.join(";")
    );

    // Enable ISA extensions for native builds only (TARGET == HOST).
    // Skip if the user already set target-cpu or target-feature.
    let target = std::env::var("TARGET").unwrap_or_default();
    let host = std::env::var("HOST").unwrap_or_default();
    let rustflags = std::env::var("CARGO_ENCODED_RUSTFLAGS").unwrap_or_default();
    let has_user_flags = rustflags.contains("target-cpu") || rustflags.contains("target-feature");

    if !has_user_flags && target == host && target.starts_with("x86_64") {
        // x86-64-v3: AVX2 + BMI1/2 + FMA + MOVBE + F16C
        println!("cargo:rustc-flag=-C target-feature=+avx2,+bmi1,+bmi2,+fma,+movbe,+f16c");
    }

    // ---- CANDY_ISA_LEVEL (fine-grained instruction-set, shown in `candy version`) ----
    // The "baseline" level is the minimum ISA the target architecture guarantees;
    // the "native" level is what we actually enable above for a same-arch host
    // build (no user-supplied target-cpu/target-feature). Cross-compiles and
    // user-overridden flags fall back to the baseline so the reported string
    // never over-states what the binary was actually compiled for.
    let isa_level = if target.starts_with("x86_64") {
        if !has_user_flags && target == host {
            "x86-64-v3"
        } else {
            "x86-64"
        }
    } else if target.starts_with("aarch64") {
        // AAPCS64 guarantees NEON; no finer vendor level is selected by this
        // build script, so report the architecture baseline.
        "aarch64"
    } else if target.starts_with("arm") {
        "arm"
    } else if target.starts_with("riscv64") {
        "rv64gc"
    } else {
        // Unknown / other target: report the raw target triple so the provenance
        // is still meaningful rather than a misleading fixed string.
        target.as_str()
    };
    println!("cargo:rustc-env=CANDY_ISA_LEVEL={isa_level}");

    // ---- CANDY_BUILD_TIME (UTC, ISO 8601, second precision) ----
    // The instant the build script ran — i.e. when this binary was built. Surfaced
    // in `candy version` as the `Built:` line.
    let build_time = Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    println!("cargo:rustc-env=CANDY_BUILD_TIME={build_time}");

    // ---- CANDY_BUILD_HOST (hostname of the machine that built this binary) ----
    // Cross-platform via the pure-Rust `hostname` crate. Falls back to
    // "unknown" when the hostname cannot be retrieved.
    let build_host = hostname::get()
        .ok()
        .and_then(|h| h.into_string().ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=CANDY_BUILD_HOST={build_host}");
}
