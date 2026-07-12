//! `renderer` — turn `FrameData` into output (spec §4.4).
//!
//! * [`typst`] renders frames via the Typst compiler library in-process.
//! * [`gpu`] (feature-gated `gpu`) rasterizes SVG frames on the GPU via
//!   vello + wgpu. Falls back to [`typst`] when no GPU is available.
//! * [`video`] encodes rasterized frames to AV1 (rav1e) / H.264 (openh264) and
//!   muxes them into MP4 / Matroska (WebM/MKV) — all self-contained, no
//!   FFmpeg, no `x264`/`x265` CLI, no system commands.

pub mod audio;
pub mod encode;
#[cfg(feature = "gpu")]
pub mod raster;
pub mod typst;

/// A single rasterized animation frame (RGBA8, row-major).
#[derive(Clone)]
pub struct RenderedFrame {
    pub width: usize,
    pub height: usize,
    pub rgba: Vec<u8>,
}

pub use typst::Renderer;
#[cfg(test)]
pub(crate) use typst::compile_file_for_test;
pub use encode::{encode_frames, mux, collect_audio, Codec, Container, EncodedVideo};
