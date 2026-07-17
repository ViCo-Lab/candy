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

    // Direct VAAPI (libva) bindings: when the `libva` feature is enabled on
    // Linux, run bindgen against the system libva 1.x headers and link `libva`.
    // This is a *build-time* binding (no runtime `dlopen`), so the build needs
    // libva development headers + libclang; the feature is off by default so the
    // standard build / CI stays self-contained.
    if std::env::var("CARGO_FEATURE_LIBVA").is_ok() && target.contains("linux") {
        gen_libva_bindings();
    }
}

/// Generate `libva` FFI bindings from the system headers and emit the link
/// directive for `libva`. Panics with a clear message if the headers are
/// missing, so a Linux build without `libva-devel` fails fast and explainably.
fn gen_libva_bindings() {
    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR not set");
    let out_path = std::path::Path::new(&out_dir).join("libva_bindings.rs");

    let bindings = bindgen::Builder::default()
        .header("/usr/include/va/va.h")
        .header("/usr/include/va/va_drm.h")
        .header("/usr/include/va/va_enc_h264.h")
        .header("/usr/include/va/va_enc_hevc.h")
        .header("/usr/include/va/va_enc_av1.h")
        .header("/usr/include/va/va_vpp.h")
        .clang_arg("-I/usr/include")
        // Keep the generated file focused on the libva API surface.
        .allowlist_type("VA.*")
        .allowlist_var("VA_.*")
        .allowlist_var("VAEnc.*")
        .allowlist_var("VAProfile.*")
        .allowlist_var("VAEntrypoint.*")
        .allowlist_var("VAConfigAttrib.*")
        .allowlist_var("VARateControl.*")
        .allowlist_var("VA_RT_FORMAT.*")
        .allowlist_var("VA_FOURCC.*")
        .allowlist_var("VA_STATUS_.*")
        .allowlist_var("VA_INVALID.*")
        .allowlist_var("VAEncMiscParameterType.*")
        .allowlist_var("VAEncSliceFlag.*")
        .allowlist_var("VAH264.*")
        .allowlist_var("VAHEVC.*")
        .allowlist_var("VAAV1.*")
        .allowlist_var("VAEncPictureType.*")
        .allowlist_function("va.*")
        .generate()
        .expect(
            "failed to generate libva bindings (is libva-devel / libva-dev installed, \
             and is libclang available for bindgen?)",
        );

    bindings
        .write_to_file(&out_path)
        .expect("failed to write libva bindings");

    // Link the system libva at build time.
    println!("cargo:rustc-link-lib=va");
    // Re-run if this build script changes.
    println!("cargo:rerun-if-changed=build.rs");
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
