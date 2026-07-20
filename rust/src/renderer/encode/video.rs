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
//! The FFmpeg path pipes raw RGBA frames to ffmpeg's stdin and has ffmpeg
//! write the muxed container to a seekable sink (stdout pipes can't be seeked
//! by the MP4/MKV/WebM muxers, and a piped stderr would deadlock on a long
//! encode). On Linux that sink is an in-RAM `memfd` (no disk temp file, faster,
//! works on read-only filesystems); elsewhere it falls back to a temp file.
//! The container is then copied to the final output. No cargo dependency on
//! ffmpeg. If ffmpeg is not found, candy falls back to the self-contained
//! codecs.
//!
//! Typst auto-sizes each page to its content, so per-frame sizes can vary. We
//! *compose* every frame onto a uniform opaque-white canvas of the largest size
//! seen (the `move` offset is already baked into the pixels), then encode.

use std::fs;
#[cfg(not(target_os = "linux"))]
use std::io::Write;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::process::Child;

use crate::core::diag::{CandyError, CandyWarn};
use crate::core::meta::PrivateMeta;
use crate::renderer::RenderedFrame;
use crate::renderer::audio::{self, AudioData};
use crate::renderer::encode::container;
#[cfg(target_os = "linux")]
use crate::renderer::encode::ffmpeg::spawn_ffmpeg_with_memfd;
use crate::renderer::encode::ffmpeg::{ErrLog, MuxSink};
use crate::warn;

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
    #[cfg(target_os = "linux")]
    H264Vaapi,
    /// H.265 via VAAPI.
    #[cfg(target_os = "linux")]
    H265Vaapi,
    /// H.264 via VideoToolbox (macOS hardware encoder).
    #[cfg(target_os = "macos")]
    H264VideoToolbox,
    /// H.265 via VideoToolbox.
    #[cfg(target_os = "macos")]
    H265VideoToolbox,
    /// H.264 via Intel Quick Sync Video (QSV).
    #[cfg(target_os = "windows")]
    H264Qsv,
    /// H.265 via Intel QSV.
    #[cfg(target_os = "windows")]
    H265Qsv,
    /// AV1 via VAAPI (Linux Intel/AMD GPU hardware encoder).
    #[cfg(target_os = "linux")]
    Av1Vaapi,
    /// VP9 via libvpx (system ffmpeg).
    Vp9,
    /// VP8 via libvpx (system ffmpeg).
    Vp8,
}

/// An encoded video ready for container muxing.
pub struct EncodedVideo {
    /// Encoded width in pixels (may include padding applied by the encoder).
    pub width: u32,
    /// Encoded height in pixels.
    pub height: u32,
    /// Frames per second (time base).
    pub fps: u32,
    /// `true` for AV1, `false` for H.264/H.265.
    pub is_av1: bool,
    /// `true` for H.265/HEVC, `false` for H.264/AV1.
    pub is_hevc: bool,
    /// One coded sample per frame (AV1 temporal unit, or length-prefixed NALs
    /// for H.264).
    pub frames: Vec<Vec<u8>>,
    /// Codec-private config: `av1C` payload (AV1), `avcC` (H.264), or `hvcC` (H.265).
    pub codec_private: Vec<u8>,
    /// Per-sample keyframe flags, parallel to `frames`. A keyframe (IDR for
    /// H.264 / AV1 key frame) decodes without referencing earlier samples, so
    /// the container's sync-sample table / block keyframe flag must list
    /// *exactly* these. Lying about this (e.g. marking every frame as a
    /// keyframe) makes players trust a non-keyframe as seekable → scrubbing
    /// and thumbnail generation fail on the resulting frame.
    pub keyframes: Vec<bool>,
}

/// A file-backed encoded video.
///
/// During a streaming encode every coded sample is written to `samples_path`
/// (a temp file) as it is produced, and only the *small* per-sample metadata
/// (size + keyframe flag) is kept in RAM. This keeps peak memory bounded to
/// that metadata regardless of video length / resolution — a long HD/high-FPS
/// render can no longer OOM on the coded stream (the old design accumulated
/// every sample in `EncodedVideo::frames: Vec<Vec<u8>>` and then built the
/// whole container in RAM).
pub(crate) struct EncodedVideoFile {
    /// Encoded width in pixels.
    pub width: u32,
    /// Encoded height in pixels.
    pub height: u32,
    /// Frames per second (time base).
    pub fps: u32,
    /// `true` for AV1, `false` for H.264/H.265.
    pub is_av1: bool,
    /// `true` for H.265/HEVC, `false` for H.264/AV1.
    pub is_hevc: bool,
    /// Codec-private config: `av1C` payload (AV1), `avcC` (H.264), or `hvcC` (H.265).
    pub codec_private: Vec<u8>,
    /// Coded-sample byte sizes, parallel to `keyframes`.
    pub sample_sizes: Vec<u32>,
    /// Per-sample keyframe flags, parallel to `sample_sizes`.
    pub keyframes: Vec<bool>,
    /// Temp file holding every coded sample concatenated (no container headers).
    pub samples_path: PathBuf,
}

/// Monotonic counter for unique temp-file names (avoids collisions when
/// multiple candy processes run concurrently).
static TMP_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Create a unique temp file for streaming coded samples, returning the open
/// handle and its path. The caller appends samples and later either reads them
/// back (`EncodedVideoFile`) or streams them into a container.
pub(crate) fn new_samples_tempfile() -> Result<(std::fs::File, PathBuf), CandyError> {
    let name = format!(
        "candy_samples_{}_{}.bin",
        std::process::id(),
        TMP_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
    );
    let path = std::env::temp_dir().join(name);
    let f = std::fs::File::create(&path)
        .map_err(|e| CandyError::Encode(format!("temp sample file: {e}")))?;
    Ok((f, path))
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
///
/// On Linux the returned `rgba` buffer is **padded to a page multiple** so the
/// caller can feed it to ffmpeg via `vmsplice(SPLICE_F_GIFT)` for true
/// zero-copy (the buffer's physical pages are gifted to the kernel pipe buffer
/// without a `write()`-style copy). The page-multiple length exists only to
/// satisfy `vmsplice`'s alignment requirement; the caller (`StreamingVideo::
/// push`) MUST write **exactly `tw*th*4` bytes** to ffmpeg and never the
/// padding tail. ffmpeg's rawvideo demuxer reads a fixed `tw*th*4` bytes per
/// frame and treats the pipe as one continuous stream, so any extra padding
/// bytes would be prepended to the next frame and shift every frame — a
/// "marquee" scroll artifact that is only invisible when `tw*th*4` is already a
/// multiple of the page size (true for standard HD sizes, not for low res).
fn compose(frame: &RenderedFrame, tw: usize, th: usize) -> RenderedFrame {
    let frame_bytes = tw * th * 4;
    // On Linux, pad the buffer to a page multiple so `vmsplice_frame` can
    // attempt a true zero-copy `vmsplice(SPLICE_F_GIFT)` gift of the buffer's
    // pages to the kernel pipe. Whether the gift is actually taken depends on
    // the base pointer being page-aligned: the global `Vec` allocator only
    // guarantees 16-byte alignment, but for large allocations (≥ ~128 KiB on
    // glibc) it delegates to `mmap`, which *does* return page-aligned
    // pointers — so HD/4K frames (≥ ~1.85 MiB) typically get the zero-copy
    // path for free. `vmsplice_frame` checks alignment at runtime and silently
    // falls back to `write()` if it is not satisfied. ffmpeg's rawvideo
    // demuxer reads exactly `tw*th*4` bytes per frame and ignores the tail
    // padding.
    #[cfg(target_os = "linux")]
    let mut rgba: Vec<u8> = {
        const PAGE: usize = 4096;
        let padded = (frame_bytes + PAGE - 1) & !(PAGE - 1);
        let mut v: Vec<u8> = Vec::with_capacity(padded);
        v.resize(padded, 255);
        v
    };
    #[cfg(not(target_os = "linux"))]
    let mut rgba = vec![255u8; frame_bytes];
    // Fast path: the frame already matches the target canvas exactly (the
    // common case — the uniform canvas is sized to the scene's page). A single
    // bulk copy is faster than the per-row loop below and lets the compiler
    // emit a single optimized memcpy.
    let cw = frame.width.min(tw);
    let ch = frame.height.min(th);
    if frame.width == tw && frame.height == th {
        rgba[..frame_bytes].copy_from_slice(&frame.rgba[..frame_bytes]);
    } else {
        // Clamp to the target canvas: a frame wider/taller than `tw`/`th` (e.g.
        // an object moved past the page edge, or a mismatched page size) must
        // not overrun `rgba`. The uniform canvas is the max page size, so
        // clipping is safe and only trims overflow that would otherwise panic
        // on copy.
        for y in 0..ch {
            let src = y * frame.width * 4;
            let dst = y * tw * 4;
            rgba[dst..dst + cw * 4].copy_from_slice(&frame.rgba[src..src + cw * 4]);
        }
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
        match self {
            Codec::X264 | Codec::X265 => true,
            #[cfg(target_os = "linux")]
            Codec::H264Vaapi | Codec::H265Vaapi | Codec::Av1Vaapi => true,
            #[cfg(target_os = "macos")]
            Codec::H264VideoToolbox | Codec::H265VideoToolbox => true,
            #[cfg(target_os = "windows")]
            Codec::H264Qsv | Codec::H265Qsv => true,
            _ => false,
        }
    }

    /// Returns `true` if candy has a self-contained encoder for this codec
    /// (rav1e for AV1, openh264 for H.264). H265 is self-contained-only when
    /// ffmpeg is not available (it returns E007 in that case).
    pub fn is_self_contained(self) -> bool {
        matches!(self, Codec::Av1 | Codec::H264 | Codec::H265)
    }
}

/// On Linux, ffmpeg frame input uses an anonymous `memfd` per frame (zero-copy,
/// tmpfs-resident) instead of a bounded OS pipe — see [`ffmpeg::spawn_ffmpeg_with_memfd`].
pub(crate) struct StreamingVideo {
    container: Container,
    meta: PrivateMeta,
    tw: usize,
    th: usize,
    audio: Option<AudioData>,
    #[cfg(target_os = "linux")]
    ffmpeg: Option<(Child, std::fs::File, MuxSink, ErrLog)>,
    #[cfg(not(target_os = "linux"))]
    ffmpeg: Option<(
        Child,
        std::io::BufWriter<std::process::ChildStdin>,
        MuxSink,
        ErrLog,
    )>,
    frame_count: usize,
    rav1e: Option<crate::renderer::encode::rav1e::Rav1eStream>,
    h264: Option<crate::renderer::encode::h264::H264Stream>,
}

impl StreamingVideo {
    /// Begin a streaming encode. `tw`/`th` are the uniform canvas size every
    /// frame is composited onto (must match what the caller pushes). `audio` is
    /// muxed in at `finish` (pass `None` for the no-audio batch path).
    pub(crate) fn new(
        fps: u32,
        codec: Codec,
        container: Container,
        meta: &PrivateMeta,
        tw: usize,
        th: usize,
        audio: Option<AudioData>,
    ) -> Result<Self, CandyError> {
        if fps < 1 {
            return Err(CandyError::Encode("fps must be >= 1".into()));
        }
        // Every video encoder (libx264/x265, openh264, rav1e, VAAPI, …) requires
        // even picture dimensions — some need multiples of 16. The batch
        // `encode_frames` path pads the canvas with `max(16).next_multiple_of(2)`,
        // but the streaming path used to forward `tw`/`th` verbatim, so an odd
        // natural page width (e.g. 907px) made ffmpeg abort with "width not
        // divisible by 2", killing its stdin pipe and surfacing as a misleading
        // "Broken pipe" error. Pad here so the composed frames, the encoder
        // setup, and the ffmpeg `-s` argument all agree on an even canvas.
        let tw = tw.max(16).next_multiple_of(2);
        let th = th.max(16).next_multiple_of(2);
        let uses_ffmpeg = codec.uses_ffmpeg()
            || (codec == Codec::H265 && crate::renderer::encode::ffmpeg::find_ffmpeg().is_some());
        if uses_ffmpeg {
            #[cfg(target_os = "linux")]
            {
                // On Linux, use memfd-based frame input for zero-copy data sharing
                let (child, frame_fd, mux, err_log) =
                    spawn_ffmpeg_with_memfd(codec, container, tw as u32, th as u32, fps, meta)?;
                Ok(Self {
                    container,
                    meta: meta.clone(),
                    tw,
                    th,
                    audio,
                    ffmpeg: Some((child, frame_fd, mux, err_log)),
                    frame_count: 0,
                    rav1e: None,
                    h264: None,
                })
            }
            #[cfg(not(target_os = "linux"))]
            {
                // On non-Linux, use standard stdin pipe
                let (child, stdin, mux, err_log) = crate::renderer::encode::ffmpeg::spawn_ffmpeg(
                    codec, container, tw as u32, th as u32, fps, meta,
                )?;
                Ok(Self {
                    container,
                    meta: meta.clone(),
                    tw,
                    th,
                    audio,
                    ffmpeg: Some((
                        child,
                        std::io::BufWriter::with_capacity(1 << 20, stdin),
                        mux,
                        err_log,
                    )),
                    frame_count: 0,
                    rav1e: None,
                    h264: None,
                })
            }
        } else if codec == Codec::H264 {
            Ok(Self {
                container,
                meta: meta.clone(),
                tw,
                th,
                audio,
                ffmpeg: None,
                frame_count: 0,
                rav1e: None,
                h264: Some(crate::renderer::encode::h264::H264Stream::new(tw, th, fps)?),
            })
        } else if codec == Codec::Av1 {
            Ok(Self {
                container,
                meta: meta.clone(),
                tw,
                th,
                audio,
                ffmpeg: None,
                frame_count: 0,
                // Streaming AV1 uses all-intra: the streaming pipeline drops each
                // frame's RGBA after encoding, so it cannot retry a panicked
                // inter-prediction pass the way the batch `encode` does. all-intra
                // disables motion estimation entirely, sidestepping the rav1e
                // 0.8.1 tiling assert that panics during ME for some geometries.
                rav1e: Some(crate::renderer::encode::rav1e::Rav1eStream::new(
                    tw, th, fps, true,
                )?),
                h264: None,
            })
        } else {
            // H265 without ffmpeg, or any other unsupported codec.
            Err(CandyError::Encode(
                "HEVC/H.265 encoding is not available: no pure-Rust encoder and no system \
                 ffmpeg. Install ffmpeg with x265 support, or use AV1 (default) / H.264."
                    .into(),
            ))
        }
    }

    /// Encode and absorb one composited frame. The frame's RGBA is consumed and
    /// dropped here, so the caller is free to release it immediately.
    pub(crate) fn push(&mut self, frame: &RenderedFrame) -> Result<(), CandyError> {
        let composed = compose(frame, self.tw, self.th);
        // ffmpeg's rawvideo demuxer reads a *fixed* `tw*th*4` bytes per frame and
        // treats the pipe as one continuous byte stream. `compose()` pads the RGBA
        // buffer to a page multiple (for the `vmsplice` zero-copy gift), but those
        // trailing padding bytes must NOT be written to ffmpeg — otherwise they
        // become the leading bytes of the next frame, shifting every frame by
        // `padding` bytes and producing a scrolling "marquee" artifact. This only
        // vanished at high resolution because standard HD canvases make
        // `tw*th*4` already a multiple of the page size (padding == 0); at low
        // resolution padding > 0 and the shift is visible. Slice to exactly
        // `frame_bytes` before handing the data to ffmpeg.
        let frame_bytes = self.tw * self.th * 4;
        if let Some((_, stdin, _, _)) = self.ffmpeg.as_mut() {
            // On Linux, `stdin` is the write end of a pipe whose read end feeds
            // ffmpeg. Use `vmsplice(SPLICE_F_GIFT)` for true zero-copy when the
            // RGBA buffer (pointer *and* `frame_bytes` length) is page-aligned;
            // otherwise fall back to a plain `write()`. Either way the pipe's
            // read() blocks ffmpeg until data is available — no premature EOF.
            #[cfg(target_os = "linux")]
            {
                crate::renderer::encode::ffmpeg::vmsplice_frame(
                    stdin,
                    &composed.rgba[..frame_bytes],
                )
                .map_err(|e| CandyError::Encode(format!("ffmpeg vmsplice/write: {e}")))?;
            }
            #[cfg(not(target_os = "linux"))]
            {
                stdin
                    .write_all(&composed.rgba[..frame_bytes])
                    .map_err(|e| CandyError::Encode(format!("ffmpeg stdin write: {e}")))?;
                // Periodic flush to prevent unbounded buffer growth while keeping
                // write() syscall count low (1MB buffer batches ~120 frames at 1080p).
                if self.frame_count % 16 == 0 {
                    stdin
                        .flush()
                        .map_err(|e| CandyError::Encode(format!("ffmpeg stdin flush: {e}")))?;
                }
            }
            self.frame_count += 1;
            return Ok(());
        }
        if let Some(r) = self.rav1e.as_mut() {
            // Safety net: rav1e 0.8.1 can still panic in rare geometries even in
            // all-intra mode. Convert that to a clean error (no process abort).
            // The streaming path cannot transparently fall back to H.264 here
            // because each frame's RGBA has already been dropped, so we surface
            // E007 and let the caller retry with `--codec h264` if desired.
            return match catch_unwind(AssertUnwindSafe(|| r.push(&composed))) {
                Ok(res) => res,
                Err(_) => Err(CandyError::Encode(
                    "rav1e aborted during AV1 streaming encode (E007); try `--codec h264`".into(),
                )),
            };
        }
        if let Some(h) = self.h264.as_mut() {
            return h.push(&composed);
        }
        Err(CandyError::Encode(
            "streaming encoder not initialized".into(),
        ))
    }

    /// Finish encoding and write the muxed container directly to `output`
    /// (audio included). The coded samples are streamed from their temp file
    /// into the container, so peak memory stays bounded to the small per-sample
    /// metadata — the whole container is never buffered in RAM (which would OOM
    /// on a long HD/high-FPS render).
    pub(crate) fn finish(self, output: &Path) -> Result<(), CandyError> {
        if let Some((child, stdin, mux, err_log)) = self.ffmpeg {
            drop(stdin);
            return crate::renderer::encode::ffmpeg::finish_ffmpeg_to_file(
                child, mux, output, err_log,
            );
        }
        let video = if let Some(r) = self.rav1e {
            r.finish_file()?
        } else if let Some(h) = self.h264 {
            h.finish_file()?
        } else {
            return Err(CandyError::Encode(
                "streaming encoder not initialized".into(),
            ));
        };
        match self.container {
            Container::Mp4 => {
                container::mux_mp4_to_file(&video, self.audio.as_ref(), output, &self.meta)
            }
            Container::Mkv => container::mux_matroska_to_file(
                &video,
                self.audio.as_ref(),
                false,
                output,
                &self.meta,
            ),
            Container::Webm => container::mux_matroska_to_file(
                &video,
                self.audio.as_ref(),
                true,
                output,
                &self.meta,
            ),
        }
    }

    /// Finish encoding and return the raw [`EncodedVideo`] (no mux, no audio).
    /// Used by the batch [`encode_frames`] entry point and tests.
    pub(crate) fn finish_video(self) -> Result<EncodedVideo, CandyError> {
        if let Some(r) = self.rav1e {
            r.finish()
        } else if let Some(h) = self.h264 {
            h.finish()
        } else {
            Err(CandyError::Encode(
                "streaming encoder not initialized".into(),
            ))
        }
    }
}

/// Encode composed RGBA frames into an [`EncodedVideo`] with the chosen codec.
///
/// Batch wrapper over [`StreamingVideo`] (no audio, no mux) kept for callers
/// that already hold every frame in memory (tests, small drafts). The streaming
/// pipeline in `lib.rs` uses [`StreamingVideo`] directly to avoid that buffering.
///
/// `private_metadata` is accepted for pipeline continuity. Metadata embedding
/// happens at mux time (see [`mux`]), not in the codec encoder.
pub fn encode_frames(
    frames: &[RenderedFrame],
    fps: u32,
    codec: Codec,
    _private_metadata: &PrivateMeta,
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

    match codec {
        // H.264 is the default self-contained codec. If openh264 fails for any
        // reason, transparently fall back to AV1 (rav1e) so a valid,
        // self-contained video is still produced.
        Codec::H264 => {
            let mut s =
                StreamingVideo::new(fps, codec, Container::Mp4, _private_metadata, tw, th, None)?;
            for f in frames {
                s.push(f)?;
            }
            s.finish_video()
        }
        // AV1 (opt-in via `--codec av1`). `rav1e` 0.8.1 can panic on some frame
        // geometries; `Rav1eStream` surfaces that as an error so we fall back to
        // H.264.
        Codec::Av1 => {
            let mut s =
                StreamingVideo::new(fps, codec, Container::Mp4, _private_metadata, tw, th, None)?;
            for f in frames {
                s.push(f)?;
            }
            match s.finish_video() {
                Ok(v) => Ok(v),
                Err(e) => {
                    warn!(CandyWarn::CodecFallback(format!("AV1 -> H.264: {e}")));
                    let mut s2 = StreamingVideo::new(
                        fps,
                        Codec::H264,
                        Container::Mp4,
                        _private_metadata,
                        tw,
                        th,
                        None,
                    )?;
                    for f in frames {
                        s2.push(f)?;
                    }
                    s2.finish_video()
                }
            }
        }
        Codec::H265 => {
            if crate::renderer::encode::ffmpeg::find_ffmpeg().is_some() {
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
///
/// `private_metadata` is embedded in the container's metadata area: an
/// iTunes-style `meta`/`ilst`/`©cmt` entry for MP4, and a `Tags`/`SimpleTag`
/// element for Matroska (WebM/MKV) — mirroring the metadata embedded in GIF
/// comments and PNG tEXt chunks.
pub fn mux(
    video: &EncodedVideo,
    audio: Option<&AudioData>,
    container: Container,
    private_metadata: &PrivateMeta,
) -> Result<Vec<u8>, CandyError> {
    match container {
        Container::Mp4 => container::mux_mp4(video, audio, private_metadata),
        Container::Mkv => container::mux_matroska(video, audio, false, private_metadata),
        Container::Webm => container::mux_matroska(video, audio, true, private_metadata),
    }
}

/// Parse every `candy.audio` track into a single merged [`AudioData`] (or
/// `None` if there are none). The first track's codec wins; mismatched tracks
/// are dropped with a warning.
pub fn collect_audio(tracks: &[crate::core::ast::AudioTrack], _fps: u32) -> Option<AudioData> {
    let mut merged: Option<AudioData> = None;
    for t in tracks {
        let Ok(mut ad) = audio::parse_audio(t) else {
            warn!(CandyWarn::AudioDropped(format!(
                "'{}' unsupported format",
                t.path
            )));
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
                    warn!(CandyWarn::AudioDropped(format!(
                        "'{}' codec differs from first track",
                        t.path
                    )));
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
    if frames.is_empty() {
        return Err(CandyError::Encode(
            "cannot write an empty RGBA draft (no frames were produced)".into(),
        ));
    }
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

/// A streaming GIF encoder: frames are pushed one at a time and written to the
/// output file immediately, so the caller never holds more than one frame's
/// RGBA at once.
pub(crate) struct GifStream {
    encoder: gif::Encoder<std::fs::File>,
    tw: u16,
    th: u16,
    delay_cs: u16,
}

impl GifStream {
    /// Open `path` for an animated GIF of `tw × th` (uniform) frames at `fps`.
    pub(crate) fn new(
        path: &Path,
        fps: u32,
        meta: &PrivateMeta,
        tw: usize,
        th: usize,
    ) -> Result<Self, CandyError> {
        if fps < 1 {
            return Err(CandyError::Encode("fps must be >= 1".into()));
        }
        // Every video encoder (libx264/x265, openh264, rav1e, VAAPI, …) requires
        // even picture dimensions — some need multiples of 16. The batch
        // `encode_frames` path pads the canvas with `max(16).next_multiple_of(2)`,
        // but the streaming path used to forward `tw`/`th` verbatim, so an odd
        // natural page width (e.g. 907px) made ffmpeg abort with "width not
        // divisible by 2", killing its stdin pipe and surfacing as a misleading
        // "Broken pipe" error. Pad here so the composed frames, the encoder
        // setup, and the ffmpeg `-s` argument all agree on an even canvas.
        let tw = tw.max(16).next_multiple_of(2);
        let th = th.max(16).next_multiple_of(2);
        let tw = tw.max(1) as u16;
        let th = th.max(1) as u16;
        let file = fs::File::create(path)?;
        let mut encoder = gif::Encoder::new(file, tw, th, &[])
            .map_err(|e| CandyError::Encode(format!("gif encoder init failed: {e}")))?;
        encoder
            .set_repeat(gif::Repeat::Infinite)
            .map_err(|e| CandyError::Encode(format!("gif repeat failed: {e}")))?;
        // Embed private metadata as a GIF comment extension before the frames.
        let meta_json = meta.to_json();
        encoder
            .write_raw_extension(
                gif::AnyExtension(gif::Extension::Comment as u8),
                &[meta_json.as_bytes()],
            )
            .map_err(|e| CandyError::Encode(format!("gif comment extension failed: {e}")))?;
        // Frame delay in centiseconds (GIF's time unit); round to at least 1.
        let delay_cs = ((1000 / fps as u64) / 10).clamp(1, u64::MAX) as u16;
        Ok(Self {
            encoder,
            tw,
            th,
            delay_cs,
        })
    }

    /// Encode and write one composited frame.
    pub(crate) fn push(&mut self, frame: &RenderedFrame) -> Result<(), CandyError> {
        let composed = compose(frame, self.tw as usize, self.th as usize);
        // `compose` may page-pad the buffer for `vmsplice` on Linux, but the
        // GIF encoder needs exactly `tw*th*4` bytes — slice down to the frame.
        let frame_bytes = self.tw as usize * self.th as usize * 4;
        let mut rgba = composed.rgba[..frame_bytes].to_vec();
        let mut f = gif::Frame::from_rgba_speed(self.tw, self.th, &mut rgba, 10);
        f.delay = self.delay_cs;
        self.encoder
            .write_frame(&f)
            .map_err(|e| CandyError::Encode(format!("gif frame encode failed: {e}")))?;
        Ok(())
    }

    /// Finish the GIF (flushes the encoder).
    pub(crate) fn finish(self) -> Result<(), CandyError> {
        Ok(())
    }
}

/// Encode `frames` into an animated GIF written to `path`.
///
/// Batch wrapper over [`GifStream`]; the streaming pipeline in `lib.rs` uses
/// [`GifStream`] directly to avoid buffering every frame's RGBA at once.
pub fn write_gif(
    frames: &[RenderedFrame],
    fps: u32,
    path: &Path,
    private_metadata: &PrivateMeta,
) -> Result<(), CandyError> {
    if frames.is_empty() {
        return Err(CandyError::Encode("cannot write an empty GIF".into()));
    }
    if fps < 1 {
        return Err(CandyError::Encode("fps must be >= 1".into()));
    }
    let max_w = frames.iter().map(|f| f.width).max().unwrap();
    let max_h = frames.iter().map(|f| f.height).max().unwrap();
    let tw = max_w.max(1);
    let th = max_h.max(1);
    let mut g = GifStream::new(path, fps, private_metadata, tw, th)?;
    for f in frames {
        g.push(f)?;
    }
    g.finish()
}

/// Encode a single [`RenderedFrame`] as an RGBA PNG bitmap written to `path`.
///
/// Used by the `--format png` target, which exports the animation's final
/// frame as a static bitmap (the "poster" of the animation). The private
/// metadata is embedded as a `candy-meta` tEXt chunk.
pub fn write_png(
    frame: &RenderedFrame,
    path: &Path,
    private_metadata: &PrivateMeta,
) -> Result<(), CandyError> {
    use png::{BitDepth, ColorType, text_metadata::TEXtChunk};
    let file = fs::File::create(path)?;
    let mut enc = png::Encoder::new(file, frame.width as u32, frame.height as u32);
    enc.set_color(ColorType::Rgba);
    enc.set_depth(BitDepth::Eight);
    let mut writer = enc
        .write_header()
        .map_err(|e| CandyError::Encode(format!("png header encode failed: {e}")))?;
    // Embed private metadata as a tEXt chunk right after the header.
    let meta_json = private_metadata.to_json();
    let text_chunk = TEXtChunk::new("candy-meta", meta_json);
    writer
        .write_text_chunk(&text_chunk)
        .map_err(|e| CandyError::Encode(format!("png text chunk failed: {e}")))?;
    writer
        .write_image_data(&frame.rgba)
        .map_err(|e| CandyError::Encode(format!("png data encode failed: {e}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::meta::PrivateMeta;

    fn sample_meta() -> PrivateMeta {
        PrivateMeta::default()
    }

    fn sample_frame() -> RenderedFrame {
        RenderedFrame {
            width: 4,
            height: 4,
            rgba: vec![255u8; 4 * 4 * 4],
        }
    }

    #[test]
    fn write_gif_embeds_private_metadata_comment_extension() {
        let tmp = std::env::temp_dir().join("candy_test_gif_meta.gif");
        let meta = sample_meta();
        let frames = vec![sample_frame()];
        write_gif(&frames, 10, &tmp, &meta).unwrap();

        let bytes = std::fs::read(&tmp).unwrap();
        std::fs::remove_file(&tmp).ok();

        // Comment extension label byte 0xFE follows the extension introducer 0x21.
        // The JSON content itself is written verbatim in the sub-blocks, so a
        // substring search is sufficient to confirm it was embedded.
        assert!(
            bytes.windows(2).any(|w| w == [0x21, 0xFE]),
            "GIF should contain a comment extension (0x21 0xFE)"
        );
        let expected = format!("\"codename\":\"{}\"", meta.codename);
        assert!(
            bytes
                .windows(expected.len())
                .any(|w| w == expected.as_bytes()),
            "GIF comment extension should contain private metadata JSON"
        );
    }

    #[test]
    fn write_png_embeds_private_metadata_text_chunk() {
        let tmp = std::env::temp_dir().join("candy_test_png_meta.png");
        let meta = sample_meta();
        write_png(&sample_frame(), &tmp, &meta).unwrap();

        let bytes = std::fs::read(&tmp).unwrap();
        std::fs::remove_file(&tmp).ok();

        // tEXt chunk keyword "candy-meta" followed by null and then the JSON text.
        assert!(
            bytes
                .windows("candy-meta".len())
                .any(|w| w == b"candy-meta"),
            "PNG should contain a candy-meta tEXt chunk"
        );
        let expected = format!("\"codename\":\"{}\"", meta.codename);
        assert!(
            bytes
                .windows(expected.len())
                .any(|w| w == expected.as_bytes()),
            "PNG tEXt chunk should contain private metadata JSON"
        );
    }
}
