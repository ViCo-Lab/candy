//! Build script: extract build-time metadata from `Cargo.toml` and expose it
//! to the binary as compile-time environment variables.
//!
//! * `CANDY_CODENAME` — release codename from `[package.metadata.candy]`.
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

fn main() {
    println!("cargo:rerun-if-changed=Cargo.toml");

    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".to_string());
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
