//! Direct VAAPI (libva) hardware encoding — **no FFmpeg subprocess**.
//!
//! This module drives the GPU encoder entirely in-process through the system
//! `libva` 1.x library, whose raw FFI is generated at build time by `bindgen`
//! (see `build.rs`, gated behind the `libva` cargo feature). Raw RGBA frames are
//! converted to NV12, uploaded to a VAAPI surface, encoded, and the resulting
//! coded samples are written to a temp file. The codec-private configuration
//! (`avcC` / `hvcC` / `av1C`) is extracted from the first coded frame, and the
//! finished stream is muxed by candy's self-contained [`container`] module —
//! FFmpeg is never spawned.
//!
//! # Build / runtime requirements
//!
//! * **Build:** the `libva` feature must be enabled, and `libva-devel` (or
//!   equivalent) + `libclang` (for bindgen) must be present. The standard
//!   `cargo build` / CI leaves the feature off so the build stays self-contained.
//! * **Runtime:** a VAAPI-capable GPU exposed as `/dev/dri/renderD128`. If it is
//!   missing, [`is_available`] is `false` and the caller transparently falls
//!   back to a self-contained software codec.
//!
//! # Codecs
//!
//! `h264-libva`, `h265-libva` and `av1-libva` are all supported. The coded
//! samples are normalised before muxing: H.264/HEVC Annex-B byte streams are
//! converted to length-prefixed (AVCC / HEVC) form, and AV1 OBUs have their
//! leading Temporal Delimiter stripped so the MP4/Matroska sample is a proper
//! temporal unit.
//!
//! # Note on verification
//!
//! The VAAPI encode parameter structs are filled from the libva 1.23 headers
//! and the code compiles against the generated bindings, but the exact
//! rate-control / picture-parameter tuning has not been exercised on real
//! hardware in this environment (no GPU). Field values follow the VA-API
//! encode examples; expect to validate on a target GPU.

#![cfg(target_os = "linux")]

#[cfg(feature = "libva")]
mod imp;

#[cfg(feature = "libva")]
pub use imp::LibvaStream;

/// Stub used when candy is built *without* the `libva` feature. The codecs are
/// still declared (so `--help` lists them), but constructing the encoder fails
/// fast with a clear message telling the user to rebuild with `--features libva`.
#[cfg(not(feature = "libva"))]
pub struct LibvaStream;

#[cfg(not(feature = "libva"))]
impl LibvaStream {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        _codec: crate::renderer::encode::Codec,
        _w: usize,
        _h: usize,
        _fps: u32,
        _container: crate::renderer::encode::Container,
        _meta: &crate::core::meta::PrivateMeta,
    ) -> Result<Self, crate::core::diag::CandyError> {
        Err(crate::core::diag::CandyError::Libva(
            "candy was built without the `libva` feature. Rebuild with \
             `--features libva` (and install libva-devel + a C compiler) to use \
             the direct VAAPI hardware encoders."
                .into(),
        ))
    }

    #[allow(dead_code)]
    pub fn push(&mut self, _frame: &crate::renderer::RenderedFrame) -> Result<(), crate::core::diag::CandyError> {
        unreachable!("LibvaStream stub cannot encode")
    }

    #[allow(dead_code)]
    pub fn finish(
        self,
        _output: &std::path::Path,
        _audio: Option<&crate::renderer::audio::AudioData>,
    ) -> Result<(), crate::core::diag::CandyError> {
        unreachable!("LibvaStream stub cannot finish")
    }
}

/// Returns `true` if a VAAPI render device exists. The actual encoder init may
/// still fail (driver lacks the requested profile), but this is the cheap
/// upfront gate used to decide whether to attempt the libva path at all.
#[cfg(feature = "libva")]
pub fn is_available() -> bool {
    std::path::Path::new("/dev/dri/renderD128").exists()
}

#[cfg(not(feature = "libva"))]
pub fn is_available() -> bool {
    std::path::Path::new("/dev/dri/renderD128").exists()
}
