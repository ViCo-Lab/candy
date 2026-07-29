//! Build script: extract build-time metadata from `Cargo.toml` and expose it
//! to the binary as compile-time environment variables.
//!
//! * `CANDY_CODENAME` — release codename from `[package.metadata.candy]`.
//! * `CANDY_GIT_HASH` — short (7-char) git commit hash of the working tree at
//!   build time, with a trailing `*` when the tree has uncommitted changes
//!   (staged or not). Falls back to `unknown` when the sources are not inside
//!   a git repository (e.g. building from a crates.io tarball) or `git` is
//!   not installed. Surfaced to users via `candy --version` as
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

/// Short git hash of `HEAD` (7 chars, matching `git rev-parse --short=7`),
/// suffixed with `*` when the working tree is dirty. Returns `"unknown"`
/// when not inside a git repository or when `git` cannot be run at all.
fn git_hash(manifest_dir: &str) -> String {
    let run = |args: &[&str]| -> Option<std::process::Output> {
        Command::new("git")
            .args(args)
            .current_dir(manifest_dir)
            .output()
            .ok()
    };

    let hash = match run(&["rev-parse", "--short=7", "HEAD"]) {
        Some(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        // Not a repo / no commits yet / git missing → no hash to report.
        _ => return "unknown".to_string(),
    };

    // Re-run the build script when HEAD moves (new commit / branch switch),
    // so the baked-in hash never goes stale. Dirty-flag drift between
    // rebuilds is inherently best-effort: touching a tracked source file
    // recompiles the crate anyway, which re-runs this script.
    if let Some(o) = run(&["rev-parse", "--git-dir"]) {
        if o.status.success() {
            let git_dir = String::from_utf8_lossy(&o.stdout).trim().to_string();
            println!("cargo:rerun-if-changed={git_dir}/HEAD");
            println!("cargo:rerun-if-changed={git_dir}/index");
        }
    }

    // Any output from `status --porcelain` means uncommitted changes
    // (staged, unstaged, or untracked-but-not-ignored files).
    let dirty = run(&["status", "--porcelain"])
        .map(|o| o.status.success() && !o.stdout.is_empty())
        .unwrap_or(false);
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
}
