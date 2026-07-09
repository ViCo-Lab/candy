//! AV1 video encoding via `rav1e`, running fully in-process (no FFmpeg, no
//! external CLI).
//!
//! RGBA8 frames produced by the Typst rasterizer are converted to YUV 4:4:4
//! (see the `Cs444` note) and handed to `rav1e`. The resulting AV1 temporal
//! units are returned as [`crate::renderer::EncodedVideo`] for the container
//! muxer to package into MP4 / Matroska (WebM/MKV).

use crate::core::error::CandyError;
use crate::renderer::EncodedVideo;
use crate::renderer::RenderedFrame;

#[cfg(feature = "video")]
use rav1e::prelude::*;
use std::panic::{catch_unwind, AssertUnwindSafe};

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
                eprintln!(
                    "warn: [E007] rav1e inter-prediction panicked; retrying AV1 in \
                     all-intra mode (valid but no temporal compression)"
                );
                catch_unwind(AssertUnwindSafe(|| encode_inner(frames, fps, true)))
                    .unwrap_or_else(|_| {
                        Err(CandyError::Encode(
                            "rav1e aborted during AV1 encoding (E007); falling back to H.264"
                                .into(),
                        ))
                    })
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

/// Core AV1 encoder (compiled only with the `video` feature).
///
/// `all_intra` forces every frame to be a keyframe (disabling inter-prediction
/// / motion estimation), which sidesteps the `rav1e` 0.8.1 tiling assert that
/// panics during ME for some frame geometries. It is only set by [`encode`] on
/// the fallback retry.
#[cfg(feature = "video")]
fn encode_inner(frames: &[RenderedFrame], fps: u32, all_intra: bool) -> Result<EncodedVideo, CandyError> {
    if frames.is_empty() {
        return Err(CandyError::Encode("cannot encode an empty animation".into()));
    }
    if fps < 1 {
        return Err(CandyError::Encode("fps must be >= 1".into()));
    }

    let width = frames[0].width;
    let height = frames[0].height;
    // rav1e's tiling/superblock layout asserts frame dims are a multiple of the
    // 64px superblock; arbitrary sizes trip an internal assert (and can abort).
    // Round up to 64; extra edge pixels stay black.
    let w = width.max(64).next_multiple_of(64);
    let h = height.max(64).next_multiple_of(64);

    // Insert a keyframe at least once per second (`gop` frames ≈ 1 s) so
    // seeking stays snappy. The exact keyframe positions are read back from each
    // packet (`FrameType::KEY`) and reported via `EncodedVideo::keyframes` for an
    // honest sync-sample table.
    let gop = (fps as u32).max(1);

    let mut enc_cfg = EncoderConfig::with_speed_preset(8);
    enc_cfg.width = w;
    enc_cfg.height = h;
    enc_cfg.bit_depth = 8;
    // Cs420 trips an internal rav1e tiling assert for some geometries; Cs444
    // (no chroma subsampling) shares the luma geometry and stays stable.
    enc_cfg.chroma_sampling = ChromaSampling::Cs444;
    enc_cfg.time_base = Rational::new(1, fps as u64);
    enc_cfg.speed_settings.scene_detection_mode = SceneDetectionSpeed::None;
    // Default: allow inter-prediction (temporal compression). When `all_intra`
    // is set (the panic-retry path) we force a keyframe on every frame, which
    // disables ME and avoids the 0.8.1 tiling panic (`rect.y >= -(cfg.yorigin)`
    // in tiling/plane_region.rs). Otherwise we cap the GOP at `gop` so seeking
    // stays snappy while still getting temporal compression.
    enc_cfg.min_key_frame_interval = 0;
    enc_cfg.max_key_frame_interval = if all_intra { 1 } else { gop as u64 };

    let cfg = Config::default().with_encoder_config(enc_cfg);
    let mut ctx: Context<u8> = cfg
        .new_context()
        .map_err(|e| CandyError::Encode(format!("invalid rav1e config: {:?}", e)))?;

    let mut frames_out: Vec<Vec<u8>> = Vec::with_capacity(frames.len());
    let mut keyframes: Vec<bool> = Vec::with_capacity(frames.len());
    let mut seq_header: Option<Vec<u8>> = None;

    for frame in frames {
        let mut rav1e_frame = Frame::new_with_padding(w, h, ChromaSampling::Cs444, 0);
        fill_frame_from_rgba(&mut rav1e_frame, &frame.rgba, width, height);

        ctx.send_frame(rav1e_frame)
            .map_err(|e| CandyError::Encode(format!("rav1e send_frame failed: {:?}", e)))?;

        while let Ok(packet) = ctx.receive_packet() {
            if seq_header.is_none() {
                if let Some(sh) = first_obu_of_type(&packet.data, 1) {
                    seq_header = Some(sh);
                }
            }
            frames_out.push(packet.data.to_vec());
            keyframes.push(packet.frame_type == FrameType::KEY);
        }
    }

    ctx.flush();
    while let Ok(packet) = ctx.receive_packet() {
        if seq_header.is_none() {
            if let Some(sh) = first_obu_of_type(&packet.data, 1) {
                seq_header = Some(sh);
            }
        }
        frames_out.push(packet.data.to_vec());
        keyframes.push(packet.frame_type == FrameType::KEY);
    }

    let codec_private = match seq_header {
        Some(sh) => {
            // AV1CodecConfigurationRecord: marker(0x81) + version(0x01) + the
            // Sequence Header OBU. Decoders read the in-band sequence header.
            let mut p = vec![0x81u8, 0x01];
            p.extend_from_slice(&sh);
            p
        }
        None => vec![0x81u8, 0x01],
    };

    // The first sample must always be seekable (a key frame). If the encoder
    // left it unmarked, force it so the stream has a valid decode entry point.
    if keyframes.first() == Some(&false) {
        keyframes[0] = true;
    }

    Ok(EncodedVideo {
        width: w as u32,
        height: h as u32,
        fps,
        is_av1: true,
        frames: frames_out,
        codec_private,
        keyframes,
    })
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
