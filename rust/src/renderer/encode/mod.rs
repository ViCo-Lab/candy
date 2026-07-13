//! Video encoding & container muxing.
//!
//! * [`video`] — codec dispatch (`encode_frames` / `mux` / `collect_audio`) and
//!   the [`Codec`] / [`Container`] / [`EncodedVideo`] types.
//! * [`rav1e`] — AV1 encoder (pure Rust, self-contained).
//! * [`h264`] — H.264 encoder (openh264, self-contained).
//! * [`ffmpeg`] — optional system-FFmpeg encoding path (runtime-detected).
//! * [`container`] — self-contained MP4 / Matroska (WebM/MKV) muxers.

pub mod video;
pub mod rav1e;
pub mod h264;
pub mod ffmpeg;
pub mod container;

pub use video::{
    Codec, Container, EncodedVideo, collect_audio, encode_frames, mux, write_gif, write_png,
    write_rgba_draft,
};
