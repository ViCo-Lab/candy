//! Build script: extract the release codename from `Cargo.toml` and expose it
//! to the binary as the `CANDY_CODENAME` compile-time environment variable.
//!
//! Also enables architecture-specific ISA extensions for native builds:
//! - x86_64: x86-64-v3 (AVX2, BMI1/2, FMA, MOVBE, F16C)
//! - aarch64: NEON is always on in AAPCS64 (no extra flags needed)

use std::fs;

fn main() {
    println!("cargo:rerun-if-changed=Cargo.toml");

    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".to_string());
    let toml = fs::read_to_string(format!("{manifest_dir}/Cargo.toml")).unwrap_or_default();
    let codename = extract_codename(&toml).unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=CANDY_CODENAME={codename}");

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

fn extract_codename(toml: &str) -> Option<String> {
    let mut in_section = false;
    for line in toml.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            in_section = t == "[package.metadata.candy]";
            continue;
        }
        if in_section && t.starts_with("codename") {
            if let Some(eq) = t.find('=') {
                let v = t[eq + 1..].trim().trim_matches('"').to_string();
                if !v.is_empty() {
                    return Some(v);
                }
            }
        }
    }
    None
}
