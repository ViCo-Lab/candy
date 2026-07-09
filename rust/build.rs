//! Build script: extract the release codename from `Cargo.toml` and expose it
//! to the binary as the `CANDY_CODENAME` compile-time environment variable, so
//! `candy --version` can print `candy <version> (<codename>)`.
//!
//! The codename lives under `[package.metadata.candy] codename = "..."`. If the
//! key is missing or unparsable we fall back to `"unknown"`.

use std::fs;

fn main() {
    // Re-run the script whenever Cargo.toml changes (so the codename stays in sync).
    println!("cargo:rerun-if-changed=Cargo.toml");

    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".to_string());
    let toml = fs::read_to_string(format!("{manifest_dir}/Cargo.toml")).unwrap_or_default();
    let codename = extract_codename(&toml).unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=CANDY_CODENAME={codename}");
}

/// Pull the `codename` value out of the `[package.metadata.candy]` table.
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
