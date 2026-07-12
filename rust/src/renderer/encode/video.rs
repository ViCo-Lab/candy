//! Video encoding dispatch.
//!
//! ## Self-contained codecs (default, no system deps)
//!
//! * **H.264** (`openh264`, linked `libopenh264`) — default.
//! * **AV1** (`rav1e`, pure Rust) — opt-in via `--codec av1`.
//!
//! ## Optional system-FFmpeg codecs (runtime-detected, no cargo deps)
//!
//! When the system has `ffmpeg` on `$PATH`, candy can shell out to it for
//! codecs that have no pure-Rust encoder:
//!
//! * **H.265/HEVC** (`x265` via ffmpeg) — the spec marks HEVC optional; this
//!   makes it available on systems with ffmpeg + x265 installed.
//! * **x264** (`x264` via ffmpeg) — higher-quality / faster than openh264 on
//!   systems with x264, used when `--codec x264` is passed.
//! * **Hardware encoders** (VAAPI on Linux, VideoToolbox on macOS, QSV on
//!   Windows/Intel) — selected via `--codec h264-vaapi` / `h265-vaapi` /
//!   `h264-videotoolbox` / `h265-videotoolbox` / `h264-qsv` / `h265-qsv`.
//!   These are runtime-detected; if the hardware encoder is unavailable,
//!   candy falls back to the software equivalent.
//!
//! The FFmpeg path pipes raw RGBA frames to ffmpeg's stdin and reads the
//! muxed output from stdout — no temp files, no cargo dependency on ffmpeg.
//! If ffmpeg is not found, candy falls back to the self-contained codecs.
//!
//! Typst auto-sizes each page to its content, so per-frame sizes can vary. We
//! *compose* every frame onto a uniform opaque-white canvas of the largest size
//! seen (the `move` offset is already baked into the pixels), then encode.

use std::fs;
use std::path::Path;

use crate::core::error::CandyError;
use crate::renderer::RenderedFrame;
use crate::renderer::audio::{self, AudioData};
use crate::renderer::encode::container;

/// Video codec selector.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    /// AV1 via rav1e (pure Rust, self-contained).
    Av1,
    /// H.264 via openh264 (self-contained). Default.
    H264,
    /// H.265/HEVC. Self-contained build returns E007; with system ffmpeg +
    /// x265, shells out to ffmpeg.
    H265,
    /// H.264 via system ffmpeg + libx264 (higher quality than openh264).
    /// Falls back to openh264 if ffmpeg is unavailable.
    X264,
    /// H.265/HEVC via system ffmpeg + libx265.
    /// Falls back to AV1 (rav1e) if ffmpeg is unavailable.
    X265,
    /// H.264 via VAAPI (Linux Intel/AMD GPU hardware encoder).
    /// Falls back to openh264 if ffmpeg or the VAAPI device is unavailable.
    H264Vaapi,
    /// H.265 via VAAPI.
    H265Vaapi,
    /// H.264 via VideoToolbox (macOS hardware encoder).
    H264VideoToolbox,
    /// H.265 via VideoToolbox.
    H265VideoToolbox,
    /// H.264 via Intel Quick Sync Video (QSV).
    H264Qsv,
    /// H.265 via Intel QSV.
    H265Qsv,
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
    /// Per-sample keyframe flags, parallel to `frames`. A keyframe (IDR for
    /// H.264 / AV1 key frame) decodes without referencing earlier samples, so
    /// the container's sync-sample table / block keyframe flag must list
    /// *exactly* these. Lying about this (e.g. marking every frame as a
    /// keyframe) makes players trust a non-keyframe as seekable → scrubbing
    /// and thumbnail generation fail on the resulting frame.
    pub keyframes: Vec<bool>,
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

impl Codec {
    /// Returns `true` if this codec should shell out to system ffmpeg
    /// (rather than using candy's self-contained rav1e/openh264 encoders).
    pub fn uses_ffmpeg(self) -> bool {
        matches!(
            self,
            Codec::X264
                | Codec::X265
                | Codec::H264Vaapi
                | Codec::H265Vaapi
                | Codec::H264VideoToolbox
                | Codec::H265VideoToolbox
                | Codec::H264Qsv
                | Codec::H265Qsv
        )
    }

    /// Returns `true` if candy has a self-contained encoder for this codec
    /// (rav1e for AV1, openh264 for H.264). H265 is self-contained-only when
    /// ffmpeg is not available (it returns E007 in that case).
    pub fn is_self_contained(self) -> bool {
        matches!(self, Codec::Av1 | Codec::H264 | Codec::H265)
    }
}

/// Encode composed RGBA frames into an [`EncodedVideo`] with the chosen codec.
///
/// For self-contained codecs (Av1, H264, H265-without-ffmpeg), this runs the
/// in-process encoder. For ffmpeg codecs (X264, X265, VAAPI, VideoToolbox,
/// QSV), use [`crate::renderer::encode::ffmpeg::encode_via_ffmpeg`] instead — that
/// function returns already-muxed bytes and bypasses this path entirely.
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
        // H.264 is the default self-contained codec. If openh264 fails for any
        // reason, transparently fall back to AV1 (rav1e) so a valid,
        // self-contained video is still produced.
        Codec::H264 => match crate::renderer::encode::h264::encode(&composed, fps) {
            Ok(v) => Ok(v),
            Err(e) => {
                eprintln!(
                    "warn: [{}] H.264 encode failed, falling back to AV1: {e}",
                    e.code()
                );
                crate::renderer::encode::rav1e::encode(&composed, fps)
            }
        },
        // AV1 (opt-in via `--codec av1`). `rav1e` 0.8.1 can panic on some frame
        // geometries; `encode` already retries in all-intra mode, and only if
        // that also fails do we fall back to H.264.
        Codec::Av1 => match crate::renderer::encode::rav1e::encode(&composed, fps) {
            Ok(v) => Ok(v),
            Err(e) => {
                eprintln!(
                    "warn: [{}] AV1 encode failed, falling back to H.264: {e}",
                    e.code()
                );
                crate::renderer::encode::h264::encode(&composed, fps)
            }
        },
        Codec::H265 => {
            // Try ffmpeg + x265 first; if ffmpeg is not available, return E007.
            if crate::renderer::encode::ffmpeg::find_ffmpeg().is_some() {
                // This path returns muxed bytes, so callers must use the
                // ffmpeg path directly. Here we return an error to signal
                // the caller should use encode_via_ffmpeg instead.
                Err(CandyError::Encode(
                    "H.265 requires the ffmpeg path — use encode_via_ffmpeg (E007 fallback)".into(),
                ))
            } else {
                Err(CandyError::Encode(
                    "HEVC/H.265 encoding is not available: no pure-Rust encoder and no system \
                     ffmpeg. Install ffmpeg with x265 support, or use AV1 (default) / H.264."
                        .into(),
                ))
            }
        }
        // FFmpeg codecs should not reach here — callers route them to
        // encode_via_ffmpeg directly. Return a clear error if they do.
        _ => Err(CandyError::Encode(format!(
            "codec {codec:?} must use the ffmpeg path (encode_via_ffmpeg), not encode_frames"
        ))),
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
pub fn collect_audio(tracks: &[crate::core::ast::AudioTrack], _fps: u32) -> Option<AudioData> {
    let mut merged: Option<AudioData> = None;
    for t in tracks {
        let Ok(mut ad) = audio::parse_audio(t) else {
            eprintln!(
                "warn: [E007] dropping audio '{}' (unsupported format)",
                t.path
            );
            continue;
        };
        let off = t.start_ms as u64; // already in ms
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
