//! AV1 video encoding via `rav1e`, running fully in-process (no FFmpeg, no
//! external CLI).
//!
//! RGBA8 frames produced by the Typst rasterizer are converted to YUV 4:4:4
//! (see the `Cs444` note) and handed to `rav1e`. The resulting AV1 temporal
//! units are returned as [`crate::renderer::EncodedVideo`] for the container
//! muxer to package into MP4 / Matroska (WebM/MKV).

use crate::core::diag::{CandyError, CandyWarn};
use crate::renderer::EncodedVideo;
use crate::renderer::RenderedFrame;
use crate::warn;
use std::fs::File;
use std::io::Write;
use std::path::PathBuf;

#[cfg(feature = "video")]
use rav1e::prelude::*;
use std::panic::{AssertUnwindSafe, catch_unwind};

/// Encode rasterized frames into AV1 and return an [`EncodedVideo`].
///
/// Precondition: `frames` is non-empty, `fps` ≥ 1.
/// Postcondition: on success returns valid AV1 packets + `av1C` codec config.
/// When the `video` feature is disabled (default build has it on), returns
/// `E007`.
pub fn encode(frames: &[RenderedFrame], fps: u32) -> Result<EncodedVideo, CandyError> {
    #[cfg(feature = "video")]
    {
        // `rav1e` 0.8.1 can hit an internal tiling assert that *panics* (and may
        // abort) during inter-prediction (motion estimation) for certain frame
        // geometries. No rav1e release newer than 0.8.1 exists yet, so instead of
        // blanket-forcing all-intra (which throws away all temporal compression),
        // we first try *full-quality* AV1 (inter-prediction on). If that panics we
        // transparently retry in all-intra mode (every frame a keyframe → no ME →
        // no panic). The output is valid AV1 either way; only the
        // temporal-compression efficiency differs. (A panic still results in a
        // clean `E007` rather than a process abort.)
        match catch_unwind(AssertUnwindSafe(|| encode_inner(frames, fps, false))) {
            Ok(r) => r,
            Err(_) => {
                warn!(CandyWarn::EncodeRetry);
                catch_unwind(AssertUnwindSafe(|| encode_inner(frames, fps, true))).unwrap_or_else(
                    |_| {
                        Err(CandyError::Encode(
                            "rav1e aborted during AV1 encoding (E007); falling back to H.264"
                                .into(),
                        ))
                    },
                )
            }
        }
    }
    #[cfg(not(feature = "video"))]
    {
        let _ = (frames, fps);
        Err(CandyError::Encode(
            "video encoding is disabled in this build (E007). Rebuild `candy` with the \
             default `video` feature to enable AV1 encoding."
                .into(),
        ))
    }
}

/// Stateful, frame-by-frame AV1 encoder.
///
/// Unlike the batch [`encode`], a `Rav1eStream` keeps the `rav1e` `Context`
/// alive across [`push`](Self::push) calls and emits one coded sample per
/// frame. This is what lets the renderer stream frames to the muxer without
/// holding every RGBA frame in memory at once: each `push` consumes exactly one
/// frame and produces a small coded sample, so peak memory is bounded by the
/// (small) coded stream rather than `N × width × height × 4` RGBA.
///
/// `all_intra` forces every frame to be a keyframe (disabling inter-prediction
/// / motion estimation), which sidesteps the `rav1e` 0.8.1 tiling assert that
/// panics during ME for some frame geometries.
#[cfg(feature = "video")]
pub(crate) struct Rav1eStream {
    ctx: Context<u8>,
    w: usize,
    h: usize,
    fps: u32,
    seq_header: Option<Vec<u8>>,
    /// Temp file holding each coded temporal unit concatenated.
    samples_file: File,
    /// Path of `samples_file` (so the muxer can stream it back).
    samples_path: PathBuf,
    /// Per-sample byte size, parallel to `keyframes`.
    sample_sizes: Vec<u32>,
    keyframes: Vec<bool>,
}

#[cfg(feature = "video")]
impl Rav1eStream {
    /// Create a streaming AV1 encoder for `width × height` frames at `fps`.
    pub(crate) fn new(
        width: usize,
        height: usize,
        fps: u32,
        all_intra: bool,
    ) -> Result<Self, CandyError> {
        if fps < 1 {
            return Err(CandyError::Encode("fps must be >= 1".into()));
        }
        // rav1e's tiling/superblock layout asserts frame dims are a multiple of
        // the 64px superblock; arbitrary sizes trip an internal assert (and can
        // abort). Round up to 64; extra edge pixels stay black.
        let w = width.max(64).next_multiple_of(64);
        let h = height.max(64).next_multiple_of(64);

        // Insert a keyframe at least once per second (`gop` frames ≈ 1 s) so
        // seeking stays snappy. The exact keyframe positions are read back from
        // each packet (`FrameType::KEY`) and reported via `EncodedVideo::keyframes`.
        let gop = fps.max(1);

        let mut enc_cfg = EncoderConfig::with_speed_preset(8);
        enc_cfg.width = w;
        enc_cfg.height = h;
        enc_cfg.bit_depth = 8;
        // Cs420 trips an internal rav1e tiling assert for some geometries; Cs444
        // (no chroma subsampling) shares the luma geometry and stays stable.
        enc_cfg.chroma_sampling = ChromaSampling::Cs444;
        enc_cfg.time_base = Rational::new(1, fps as u64);
        enc_cfg.speed_settings.scene_detection_mode = SceneDetectionSpeed::None;
        enc_cfg.min_key_frame_interval = 0;
        enc_cfg.max_key_frame_interval = if all_intra { 1 } else { gop as u64 };

        let cfg = Config::default().with_encoder_config(enc_cfg);
        let ctx = cfg
            .new_context()
            .map_err(|e| CandyError::Encode(format!("invalid rav1e config: {:?}", e)))?;

        let (samples_file, samples_path) = crate::renderer::encode::video::new_samples_tempfile()?;

        Ok(Self {
            ctx,
            w,
            h,
            fps,
            seq_header: None,
            samples_file,
            samples_path,
            sample_sizes: Vec::new(),
            keyframes: Vec::new(),
        })
    }

    /// Encode one RGBA frame and append its coded sample(s).
    pub(crate) fn push(&mut self, frame: &RenderedFrame) -> Result<(), CandyError> {
        let mut rav1e_frame = Frame::new_with_padding(self.w, self.h, ChromaSampling::Cs444, 0);
        fill_frame_from_rgba(&mut rav1e_frame, &frame.rgba, frame.width, frame.height);

        self.ctx
            .send_frame(rav1e_frame)
            .map_err(|e| CandyError::Encode(format!("rav1e send_frame failed: {:?}", e)))?;

        while let Ok(packet) = self.ctx.receive_packet() {
            self.capture(&packet.data, packet.frame_type == FrameType::KEY)?;
        }
        Ok(())
    }

    fn capture(&mut self, data: &[u8], is_key: bool) -> Result<(), CandyError> {
        if self.seq_header.is_none() {
            if let Some(sh) = first_obu_of_type(data, 1) {
                self.seq_header = Some(sh);
            }
        }
        self.samples_file
            .write_all(data)
            .map_err(|e| CandyError::Encode(format!("sample write: {e}")))?;
        self.sample_sizes.push(data.len() as u32);
        self.keyframes.push(is_key);
        Ok(())
    }

    /// Finish encoding, flush the encoder, and return the file-backed
    /// [`EncodedVideoFile`] (the coded samples stay in their temp file; only
    /// metadata is returned). The streaming muxer streams the file into the
    /// container, so nothing is ever buffered in RAM.
    pub(crate) fn finish_file(
        self,
    ) -> Result<crate::renderer::encode::video::EncodedVideoFile, CandyError> {
        let mut this = self;
        this.ctx.flush();
        while let Ok(packet) = this.ctx.receive_packet() {
            this.capture(&packet.data, packet.frame_type == FrameType::KEY)?;
        }

        let codec_private = match this.seq_header {
            Some(sh) => {
                let mut p = vec![0x81u8, 0x01];
                p.extend_from_slice(&sh);
                p
            }
            None => vec![0x81u8, 0x01],
        };

        // The first sample must always be seekable (a key frame). If the encoder
        // left it unmarked, force it so the stream has a valid decode entry point.
        let mut keyframes = this.keyframes;
        if keyframes.first() == Some(&false) {
            keyframes[0] = true;
        }

        Ok(crate::renderer::encode::video::EncodedVideoFile {
            width: this.w as u32,
            height: this.h as u32,
            fps: this.fps,
            is_av1: true,
            codec_private,
            sample_sizes: this.sample_sizes,
            keyframes,
            samples_path: this.samples_path,
        })
    }

    /// Finish encoding and assemble the in-memory [`EncodedVideo`] (reads the
    /// temp sample file back). Used by the batch `encode_frames` path and tests,
    /// which already hold every frame in RAM anyway.
    pub(crate) fn finish(self) -> Result<EncodedVideo, CandyError> {
        let file = self.finish_file()?;
        let bytes = std::fs::read(&file.samples_path)
            .map_err(|e| CandyError::Encode(format!("sample read: {e}")))?;
        let _ = std::fs::remove_file(&file.samples_path);
        let mut frames = Vec::with_capacity(file.sample_sizes.len());
        let mut off = 0usize;
        for &sz in &file.sample_sizes {
            let sz = sz as usize;
            frames.push(bytes[off..off + sz].to_vec());
            off += sz;
        }
        Ok(EncodedVideo {
            width: file.width,
            height: file.height,
            fps: file.fps,
            is_av1: file.is_av1,
            frames,
            codec_private: file.codec_private,
            keyframes: file.keyframes,
        })
    }
}

/// Core AV1 encoder (compiled only with the `video` feature).
///
/// `all_intra` forces every frame to be a keyframe (disabling inter-prediction
/// / motion estimation), which sidesteps the `rav1e` 0.8.1 tiling assert that
/// panics during ME for some frame geometries. It is only set by [`encode`] on
/// the fallback retry. This is the batch entry point; the streaming path uses
/// [`Rav1eStream`] directly.
#[cfg(feature = "video")]
fn encode_inner(
    frames: &[RenderedFrame],
    fps: u32,
    all_intra: bool,
) -> Result<EncodedVideo, CandyError> {
    if frames.is_empty() {
        return Err(CandyError::Encode(
            "cannot encode an empty animation".into(),
        ));
    }
    let mut stream = Rav1eStream::new(frames[0].width, frames[0].height, fps, all_intra)?;
    for f in frames {
        stream.push(f)?;
    }
    stream.finish()
}

/// Extract the first OBU of the given `obu_type` (1 = Sequence Header) from a
/// temporal unit. Returns the full OBU (including its header) bytes.
#[cfg(feature = "video")]
fn first_obu_of_type(tu: &[u8], obu_type: u8) -> Option<Vec<u8>> {
    let mut i = 0;
    while i < tu.len() {
        let header = *tu.get(i)?;
        let has_size = (header & 0b0000_0100) != 0;
        let ext = (header & 0b0000_1000) != 0;
        let type_field = (header >> 3) & 0x0F;
        let mut j = i + 1;
        if ext {
            j += 1; // obu_extension_header
        }
        let size = if has_size {
            let (sz, n) = read_leb128(tu, j)?;
            j += n;
            sz as usize
        } else {
            tu.len() - j
        };
        let end = (j + size).min(tu.len());
        if type_field == obu_type {
            return Some(tu[i..end].to_vec());
        }
        i = end;
    }
    None
}

/// Read an unsigned LEB128 integer; returns (value, bytes_consumed).
#[cfg(feature = "video")]
fn read_leb128(buf: &[u8], mut i: usize) -> Option<(u64, usize)> {
    let mut result: u64 = 0;
    let mut shift = 0;
    let mut count = 0;
    while i < buf.len() {
        let byte = buf[i];
        result |= ((byte & 0x7F) as u64) << shift;
        count += 1;
        i += 1;
        if byte & 0x80 == 0 {
            break;
        }
        shift += 7;
    }
    Some((result, count))
}

/// Convert an RGBA8 buffer into the `rav1e` frame's Y/U/V planes (4:4:4).
#[cfg(feature = "video")]
fn fill_frame_from_rgba(frame: &mut Frame<u8>, rgba: &[u8], src_w: usize, src_h: usize) {
    let y_stride = frame.planes[0].cfg.stride;
    let c_stride = frame.planes[1].cfg.stride;

    for y in 0..src_h {
        for x in 0..src_w {
            let o = (y * src_w + x) * 4;
            let (r, g, b) = (rgba[o] as f32, rgba[o + 1] as f32, rgba[o + 2] as f32);
            let yv = (0.299 * r + 0.587 * g + 0.114 * b).clamp(0.0, 255.0) as u8;
            let u = (-0.169 * r - 0.331 * g + 0.5 * b + 128.0).clamp(0.0, 255.0) as u8;
            let v = (0.5 * r - 0.419 * g - 0.081 * b + 128.0).clamp(0.0, 255.0) as u8;
            frame.planes[0].data[y * y_stride + x] = yv;
            frame.planes[1].data[y * c_stride + x] = u;
            frame.planes[2].data[y * c_stride + x] = v;
        }
    }
}
