//! Video encoding dispatch — fully self-contained (no FFmpeg, no `x264`/`x265`
//! CLI). Every codec runs *in-process*:
//!
//! * **AV1** (`rav1e`, pure Rust) — preferred.
//! * **H.264** (`openh264`, linked `libopenh264`) — optional.
//! * **HEVC** is intentionally *not* supported: there is no pure-Rust / library
//!   encoder we can ship without invoking a system command, and the spec marks
//!   it optional (HEVC/H264).
//!
//! Typst auto-sizes each page to its content, so per-frame sizes can vary. We
//! *compose* every frame onto a uniform opaque-white canvas of the largest size
//! seen (the `move` offset is already baked into the pixels), then encode.

use std::fs;
use std::path::Path;

use crate::core::error::CandyError;
use crate::renderer::RenderedFrame;
use crate::renderer::audio::{self, AudioData};
use crate::renderer::container;

/// Video codec selector.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    Av1,
    H264,
    H265,
}

/// An encoded video ready for container muxing.
pub struct EncodedVideo {
    /// Encoded width in pixels (may include padding applied by the encoder).
    pub width: u32,
    /// Encoded height in pixels.
    pub height: u32,
    /// Frames per second (time base).
    pub fps: u32,
    /// `true` for AV1, `false` for H.264.
    pub is_av1: bool,
    /// One coded sample per frame (AV1 temporal unit, or length-prefixed NALs
    /// for H.264).
    pub frames: Vec<Vec<u8>>,
    /// Codec-private config: `av1C` payload (AV1) or `avcC` (H.264).
    pub codec_private: Vec<u8>,
}

/// Output container.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Container {
    Mp4,
    Mkv,
    Webm,
}

/// Compose `frame` onto a `tw × th` opaque-white canvas, copying the source
/// pixels to the top-left. Returns a fresh `RenderedFrame`.
fn compose(frame: &RenderedFrame, tw: usize, th: usize) -> RenderedFrame {
    let mut rgba = vec![255u8; tw * th * 4];
    for y in 0..frame.height.min(th) {
        let src = y * frame.width * 4;
        let dst = y * tw * 4;
        rgba[dst..dst + frame.width * 4].copy_from_slice(&frame.rgba[src..src + frame.width * 4]);
    }
    RenderedFrame {
        width: tw,
        height: th,
        rgba,
    }
}

/// Encode composed RGBA frames into an [`EncodedVideo`] with the chosen codec.
pub fn encode_frames(
    frames: &[RenderedFrame],
    fps: u32,
    codec: Codec,
) -> Result<EncodedVideo, CandyError> {
    if frames.is_empty() {
        return Err(CandyError::Encode(
            "cannot encode an empty animation".into(),
        ));
    }
    if fps < 1 {
        return Err(CandyError::Encode("fps must be >= 1".into()));
    }
    let max_w = frames.iter().map(|f| f.width).max().unwrap();
    let max_h = frames.iter().map(|f| f.height).max().unwrap();
    // openh264 rejects any picture smaller than 16×16 (`cmUnsupportedData`), and
    // all H.264/AV1 encoders want even dimensions. Pad the composed canvas to a
    // minimum of 16×16 (rounded up to the next even size) so tiny pages (e.g. a
    // single dot) still encode. `max(...)` keeps the canvas >= every frame, so no
    // visible cropping occurs.
    let tw = max_w.max(16).next_multiple_of(2);
    let th = max_h.max(16).next_multiple_of(2);
    let composed: Vec<RenderedFrame> = frames.iter().map(|f| compose(f, tw, th)).collect();

    match codec {
        // AV1 is the priority codec. `rav1e` can panic on some frame geometries;
        // if it does (or returns an error), transparently fall back to H.264 so a
        // valid, self-contained video is still produced.
        Codec::Av1 => match crate::renderer::rav1e::encode(&composed, fps) {
            Ok(v) => Ok(v),
            Err(e) => {
                eprintln!(
                    "warn: [{}] AV1 encode failed, falling back to H.264: {e}",
                    e.code()
                );
                crate::renderer::h264::encode(&composed, fps)
            }
        },
        Codec::H264 => crate::renderer::h264::encode(&composed, fps),
        Codec::H265 => Err(CandyError::Encode(
            "HEVC/H.265 encoding is not available in this self-contained build (E007). Use AV1 \
             (default) or H.264."
                .into(),
        )),
    }
}

/// Package an [`EncodedVideo`] (plus optional audio) into a container byte buffer.
pub fn mux(
    video: &EncodedVideo,
    audio: Option<&AudioData>,
    container: Container,
) -> Result<Vec<u8>, CandyError> {
    match container {
        Container::Mp4 => container::mux_mp4(video, audio),
        Container::Mkv => container::mux_matroska(video, audio, false),
        Container::Webm => container::mux_matroska(video, audio, true),
    }
}

/// Parse every `candy.audio` track into a single merged [`AudioData`] (or
/// `None` if there are none). The first track's codec wins; mismatched tracks
/// are dropped with a warning.
pub fn collect_audio(tracks: &[crate::core::ast::AudioTrack], fps: u32) -> Option<AudioData> {
    let mut merged: Option<AudioData> = None;
    for t in tracks {
        let Ok(mut ad) = audio::parse_audio(t) else {
            eprintln!(
                "warn: [E007] dropping audio '{}' (unsupported format)",
                t.path
            );
            continue;
        };
        let off = (t.start_frame as u64) * 1000 / fps as u64;
        for f in &mut ad.frames {
            f.timestamp_ms += off;
        }
        match &mut merged {
            None => merged = Some(ad),
            Some(m) => {
                if m.codec != ad.codec {
                    eprintln!(
                        "warn: [E007] dropping audio '{}' (codec differs from first track)",
                        t.path
                    );
                } else {
                    m.frames.extend(ad.frames);
                }
            }
        }
    }
    merged
}

/// Write `frames` as a raw RGBA bundle into `dir` (intermediate / draft).
pub fn write_rgba_draft(
    frames: &[RenderedFrame],
    dir: &Path,
    name: &str,
) -> Result<(), CandyError> {
    let path = dir.join(format!("{name}.rgba"));
    let mut buf = Vec::new();
    buf.extend_from_slice(&(frames.len() as u32).to_le_bytes());
    buf.extend_from_slice(&(frames[0].width as u32).to_le_bytes());
    buf.extend_from_slice(&(frames[0].height as u32).to_le_bytes());
    for f in frames {
        buf.extend_from_slice(&f.rgba);
    }
    fs::write(path, buf)?;
    Ok(())
}
