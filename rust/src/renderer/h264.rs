//! H.264 video encoding via `openh264`, fully in-process (no `x264` CLI, no
//! FFmpeg). `openh264` links the system `libopenh264` at build time but does
//! all encoding inside the `candy` process.
//!
//! RGBA frames are converted to planar I420 and fed to the encoder. The Annex-B
//! output is repackaged into length-prefixed NAL samples (as required by MP4 /
//! Matroska) and the SPS/PPS are extracted into an `avcC` codec config.

use crate::core::error::CandyError;
use crate::renderer::EncodedVideo;
use crate::renderer::RenderedFrame;

/// Encode rasterized frames into H.264 and return an [`EncodedVideo`].
pub fn encode(frames: &[RenderedFrame], fps: u32) -> Result<EncodedVideo, CandyError> {
    if frames.is_empty() {
        return Err(CandyError::Encode("cannot encode an empty animation".into()));
    }
    if fps < 1 {
        return Err(CandyError::Encode("fps must be >= 1".into()));
    }

    let width = frames[0].width;
    let height = frames[0].height;
    // H.264 needs even dimensions for 4:2:0 chroma.
    let w = width.next_multiple_of(2);
    let h = height.next_multiple_of(2);

    // The default `EncoderConfig` leaves `max_frame_rate` at 0 Hz, which makes
    // OpenH264's `initialize_ext` return `Native:5`. We must set a valid frame
    // rate (and a bitrate scaled to the resolution) before encoding.
    let target_bps = ((w as u64 * h as u64 * fps as u64) / 20)
        .clamp(120_000, 20_000_000) as u32;
    let config = openh264::encoder::EncoderConfig::new()
        .max_frame_rate(openh264::encoder::FrameRate::from_hz(fps as f32))
        .bitrate(openh264::encoder::BitRate::from_bps(target_bps));

    let mut encoder = openh264::encoder::Encoder::with_api_config(
        openh264::OpenH264API::from_source(),
        config,
    )
    .map_err(|e| CandyError::Encode(format!("openh264 init failed: {e}")))?;

    let mut samples: Vec<Vec<u8>> = Vec::with_capacity(frames.len());
    let mut sps: Option<Vec<u8>> = None;
    let mut pps: Option<Vec<u8>> = None;

    for frame in frames {
        let (y, u, v) = rgba_to_i420_packed(&frame.rgba, width, height, w, h);
        let yuv = openh264::formats::YUVBuffer::from_vec([y, u, v].concat(), w, h);
        let encoded = encoder
            .encode(&yuv)
            .map_err(|e| CandyError::Encode(format!("openh264 encode failed: {e}")))?;

        // Walk every NAL unit. OpenH264 emits Annex-B (`00 00 00 01` start
        // codes) before each NAL, so `nal_unit()` returns the start code *and*
        // the NAL payload. We strip the start code, derive the real NAL header
        // byte, and repackage the payload as a length-prefixed sample (as
        // required by MP4 / Matroska). The first SPS/PPS seen are captured for
        // the `avcC` box.
        let mut sample: Vec<u8> = Vec::new();
        for l in 0..encoded.num_layers() {
            let layer = encoded.layer(l).expect("layer index within range");
            for n in 0..layer.nal_count() {
                let nal = layer.nal_unit(n).expect("nal index within range");
                let payload = nal_payload(nal);
                let nal_type = payload.first().copied().unwrap_or(0) & 0x1F;
                if sps.is_none() && nal_type == 7 {
                    sps = Some(payload.to_vec());
                } else if pps.is_none() && nal_type == 8 {
                    pps = Some(payload.to_vec());
                }
                sample.extend_from_slice(&(payload.len() as u32).to_be_bytes());
                sample.extend_from_slice(payload);
            }
        }
        samples.push(sample);
    }

    let (sps, pps) = match (sps, pps) {
        (Some(s), Some(p)) => (s, p),
        _ => {
            return Err(CandyError::Encode(
                "openh264 did not emit SPS/PPS (E007)".into(),
            ))
        }
    };

    Ok(EncodedVideo {
        width: w as u32,
        height: h as u32,
        fps,
        is_av1: false,
        frames: samples,
        codec_private: build_avcc(&sps, &pps),
    })
}

/// Convert an RGBA frame to planar I420, padded to `(w, h)` (even). Returns
/// the three owned planes `(Y, U, V)`.
fn rgba_to_i420_packed(
    rgba: &[u8],
    src_w: usize,
    src_h: usize,
    w: usize,
    h: usize,
) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    let mut yp = vec![0u8; w * h];
    let mut up = vec![0u8; (w / 2) * (h / 2)];
    let mut vp = vec![0u8; (w / 2) * (h / 2)];

    for y in 0..src_h.min(h) {
        for x in 0..src_w.min(w) {
            let o = (y * src_w + x) * 4;
            let (r, g, b) = (rgba[o] as f32, rgba[o + 1] as f32, rgba[o + 2] as f32);
            yp[y * w + x] = (0.299 * r + 0.587 * g + 0.114 * b).clamp(0.0, 255.0) as u8;
        }
    }
    for y in 0..(src_h.min(h) / 2) {
        for x in 0..(src_w.min(w) / 2) {
            let sx = x * 2;
            let sy = y * 2;
            let o = (sy * src_w + sx) * 4;
            let (r, g, b) = (rgba[o] as f32, rgba[o + 1] as f32, rgba[o + 2] as f32);
            up[y * (w / 2) + x] = (-0.169 * r - 0.331 * g + 0.5 * b + 128.0).clamp(0.0, 255.0) as u8;
            let o2 = (sy * src_w + (sx + 1).min(src_w - 1)) * 4;
            let (r2, g2, b2) = (rgba[o2] as f32, rgba[o2 + 1] as f32, rgba[o2 + 2] as f32);
            vp[y * (w / 2) + x] = (0.5 * r2 - 0.419 * g2 - 0.081 * b2 + 128.0).clamp(0.0, 255.0) as u8;
        }
    }
    (yp, up, vp)
}

/// OpenH264 prepends an Annex-B start code (`00 00 00 01` or `00 00 01`) before
/// every NAL unit in its output buffer. Return the slice *after* that start
/// code — i.e. the actual NAL payload (whose first byte is the NAL header).
fn nal_payload(nal: &[u8]) -> &[u8] {
    if nal.len() >= 4 && nal[0] == 0 && nal[1] == 0 && nal[2] == 0 && nal[3] == 1 {
        &nal[4..]
    } else if nal.len() >= 3 && nal[0] == 0 && nal[1] == 0 && nal[2] == 1 {
        &nal[3..]
    } else {
        nal
    }
}

/// Build an `avcC` codec configuration record from SPS/PPS.
fn build_avcc(sps: &[u8], pps: &[u8]) -> Vec<u8> {
    let mut v = Vec::new();
    v.push(1); // configurationVersion
    v.push(sps[1]); // AVCProfileIndication
    v.push(sps[2]); // profile_compatibility
    v.push(sps[3]); // AVCLevelIndication
    v.push(0xFF); // 6 bits reserved (111111) + 2 bits lengthSizeMinusOne (11 => 3)
    v.push(0xE1); // 3 bits reserved (111) + 5 bits numOfSPS (00001)
    v.extend_from_slice(&(sps.len() as u16).to_be_bytes());
    v.extend_from_slice(sps);
    v.push(1); // numOfPPS
    v.extend_from_slice(&(pps.len() as u16).to_be_bytes());
    v.extend_from_slice(pps);
    v
}
