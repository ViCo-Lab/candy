//! `renderer` — turn `FrameData` into output (spec §4.4).
//!
//! * [`typst`] renders frames via the Typst compiler library in-process.
//! * [`gpu`] (feature-gated `gpu`) rasterizes SVG frames on the GPU via
//!   vello + wgpu. Falls back to [`typst`] when no GPU is available.
//! * [`video`] encodes rasterized frames to AV1 (rav1e) / H.264 (openh264) and
//!   muxes them into MP4 / Matroska (WebM/MKV) — all self-contained, no
//!   FFmpeg, no `x264`/`x265` CLI, no system commands.

pub mod audio;
pub mod container;
#[cfg(feature = "gpu")]
pub mod gpu;
pub mod h264;
pub mod rav1e;
pub mod typst;
pub mod video;

/// A single rasterized animation frame (RGBA8, row-major).
#[derive(Clone)]
pub struct RenderedFrame {
    pub width: usize,
    pub height: usize,
    pub rgba: Vec<u8>,
}

pub use typst::Renderer;
#[cfg(test)]
pub(crate) use typst::compile_svg_for_test;
pub use video::{encode_frames, mux, collect_audio, Codec, Container, EncodedVideo};
