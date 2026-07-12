//! Audio ingestion for `candy.audio` tracks.
//!
//! Candy is self-contained: it does not shell out to `ffmpeg`. Instead it
//! demuxes the two most common *voice* formats in pure Rust and hands the raw
//! coded packets to the container muxer:
//!
//! * **Opus** in an Ogg container (`.opus` / `.ogg`) → Matroska (WebM/MKV).
//! * **AAC** in ADTS framing (`.aac`, or `.m4a` raw ADTS) → MP4.

pub mod audio;
pub use audio::*;
