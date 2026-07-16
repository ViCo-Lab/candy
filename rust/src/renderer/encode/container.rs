//! Self-contained media container muxers (no FFmpeg / external tools).
//!
//! * [`mux_mp4`]      — MP4 (ISO BMFF) for AV1 (`av01`+`av1C`) or H.264
//!   (`avc1`+`avcC`), optionally with an AAC audio track.
//! * [`mux_matroska`] — Matroska for WebM (`webm`) or MKV (`matroska`), AV1 or
//!   H.264 video, optionally Opus or AAC audio.
//!
//! All byte layout is written by hand so `candy` is fully self-contained.

use crate::core::diag::{CandyError, CandyWarn};
use crate::core::meta::PrivateMeta;
use crate::renderer::EncodedVideo;
use crate::renderer::EncodedVideoFile;
use crate::renderer::audio::AudioData;
use crate::warn;
use std::fs::File;
use std::io::{Read, Write};
use std::path::Path;

/// Mux an encoded video (and optional audio) into an MP4 file.
///
/// `private_metadata` is embedded in the `moov`/`udta` user-data area as an
/// iTunes-style `meta`/`ilst`/`©cmt` (comment) entry containing the compact
/// JSON, mirroring the metadata embedded in GIF comments and PNG tEXt chunks.
pub fn mux_mp4(
    v: &EncodedVideo,
    audio: Option<&AudioData>,
    private_metadata: &PrivateMeta,
) -> Result<Vec<u8>, CandyError> {
    let nframes = v.frames.len() as u32;
    if nframes == 0 {
        return Err(CandyError::Encode(
            "cannot mux an empty video (E007)".into(),
        ));
    }
    let total_ms = (nframes as u64) * 1000 / v.fps as u64;
    let v_sizes: Vec<u32> = v.frames.iter().map(|f| f.len() as u32).collect();
    let v_bytes: usize = v_sizes.iter().map(|s| *s as usize).sum();

    let (a_sizes, a_dur_samples, _a_total_samples): (Vec<u32>, Vec<u32>, u64) = match audio {
        Some(a) if a.codec == crate::renderer::audio::AudioCodec::Aac => {
            let sizes: Vec<u32> = a.frames.iter().map(|f| f.data.len() as u32).collect();
            let durs: Vec<u32> = a
                .frames
                .iter()
                .map(|f| ((f.duration_ms * a.sample_rate as u64) / 1000) as u32)
                .collect();
            let total: u64 = durs.iter().map(|d| *d as u64).sum();
            (sizes, durs, total)
        }
        Some(_) => {
            warn!(CandyWarn::AudioIgnored);
            (vec![], vec![], 0)
        }
        None => (vec![], vec![], 0),
    };
    let a_bytes: usize = a_sizes.iter().map(|s| *s as usize).sum();

    let ftyp = b(b"ftyp", {
        let mut p = vec![];
        p.extend_from_slice(b"isom"); // major_brand
        p.extend_from_slice(&[0, 0, 0, 0]); // minor_version
        p.extend_from_slice(b"isom"); // compatible_brand
        p.extend_from_slice(if v.is_av1 { b"av01" } else { b"avc1" }); // compatible_brand
        p.extend_from_slice(b"mp42"); // compatible_brand (MP4 v2)
        p.extend_from_slice(b"mmp4"); // compatible_brand (mobile MP4)
        p
    });

    let moov0 = build_moov(
        v,
        nframes,
        audio,
        &v_sizes,
        &a_sizes,
        &a_dur_samples,
        total_ms,
        0,
        0,
        private_metadata,
    );
    let moov_len = moov0.len();
    let mdat_offset = ftyp.len() + moov_len + 8;
    let video_offset = mdat_offset;
    let audio_offset = mdat_offset + v_bytes;
    let moov = build_moov(
        v,
        nframes,
        audio,
        &v_sizes,
        &a_sizes,
        &a_dur_samples,
        total_ms,
        video_offset as u32,
        audio_offset as u32,
        private_metadata,
    );

    let mut out = Vec::with_capacity(ftyp.len() + moov.len() + 8 + v_bytes + a_bytes);
    out.extend_from_slice(&ftyp);
    out.extend_from_slice(&moov);
    // mdat
    out.extend_from_slice(&((v_bytes + a_bytes + 8) as u32).to_be_bytes());
    out.extend_from_slice(b"mdat");
    for f in &v.frames {
        out.extend_from_slice(f);
    }
    for f in audio.map(|a| &a.frames).into_iter().flatten() {
        out.extend_from_slice(&f.data);
    }
    Ok(out)
}

/// Build a minimal [`EncodedVideo`] shim that carries the metadata the muxer
/// needs (dimensions, fps, codec config, keyframes) but no sample bytes. Used
/// by the file-backed [`mux_mp4_to_file`]/[`mux_matroska_to_file`] paths, which
/// stream the actual samples from disk instead of holding them in RAM.
fn video_shim(v: &EncodedVideoFile) -> EncodedVideo {
    EncodedVideo {
        width: v.width,
        height: v.height,
        fps: v.fps,
        is_av1: v.is_av1,
        frames: Vec::new(),
        codec_private: v.codec_private.clone(),
        keyframes: v.keyframes.clone(),
    }
}

/// Mux a file-backed encoded video (and optional audio) directly into `output`.
///
/// `ftyp` + `moov` are built in RAM (small — only per-sample metadata), then the
/// coded samples are streamed from `v.samples_path` into the `mdat` box. Peak
/// memory is bounded to the metadata regardless of video length / resolution,
/// so a long HD/high-FPS render cannot OOM on the coded stream.
pub(crate) fn mux_mp4_to_file(
    v: &EncodedVideoFile,
    audio: Option<&AudioData>,
    output: &Path,
    private_metadata: &PrivateMeta,
) -> Result<(), CandyError> {
    let nframes = v.sample_sizes.len() as u32;
    if nframes == 0 {
        return Err(CandyError::Encode(
            "cannot mux an empty video (E007)".into(),
        ));
    }
    let total_ms = (nframes as u64) * 1000 / v.fps as u64;
    let v_sizes: Vec<u32> = v.sample_sizes.clone();
    let v_bytes: usize = v_sizes.iter().map(|s| *s as usize).sum();

    let (a_sizes, a_dur_samples, _a_total_samples): (Vec<u32>, Vec<u32>, u64) = match audio {
        Some(a) if a.codec == crate::renderer::audio::AudioCodec::Aac => {
            let sizes: Vec<u32> = a.frames.iter().map(|f| f.data.len() as u32).collect();
            let durs: Vec<u32> = a
                .frames
                .iter()
                .map(|f| ((f.duration_ms * a.sample_rate as u64) / 1000) as u32)
                .collect();
            let total: u64 = durs.iter().map(|d| *d as u64).sum();
            (sizes, durs, total)
        }
        Some(_) => {
            warn!(CandyWarn::AudioIgnored);
            (vec![], vec![], 0)
        }
        None => (vec![], vec![], 0),
    };
    let a_bytes: usize = a_sizes.iter().map(|s| *s as usize).sum();

    let ftyp = b(b"ftyp", {
        let mut p = vec![];
        p.extend_from_slice(b"isom");
        p.extend_from_slice(&[0, 0, 0, 0]);
        p.extend_from_slice(b"isom");
        p.extend_from_slice(if v.is_av1 { b"av01" } else { b"avc1" });
        p.extend_from_slice(b"mp42");
        p.extend_from_slice(b"mmp4");
        p
    });

    let moov0 = build_moov(
        &video_shim(v),
        nframes,
        audio,
        &v_sizes,
        &a_sizes,
        &a_dur_samples,
        total_ms,
        0,
        0,
        private_metadata,
    );
    let moov_len = moov0.len();
    let mdat_offset = ftyp.len() + moov_len + 8;
    let video_offset = mdat_offset;
    let audio_offset = mdat_offset + v_bytes;
    let moov = build_moov(
        &video_shim(v),
        nframes,
        audio,
        &v_sizes,
        &a_sizes,
        &a_dur_samples,
        total_ms,
        video_offset as u32,
        audio_offset as u32,
        private_metadata,
    );

    let mut file = std::fs::File::create(output)
        .map_err(|e| CandyError::Encode(format!("container write: {e}")))?;
    file.write_all(&ftyp)
        .map_err(|e| CandyError::Encode(format!("container write: {e}")))?;
    file.write_all(&moov)
        .map_err(|e| CandyError::Encode(format!("container write: {e}")))?;
    // mdat
    file.write_all(&((v_bytes + a_bytes + 8) as u32).to_be_bytes())
        .map_err(|e| CandyError::Encode(format!("container write: {e}")))?;
    file.write_all(b"mdat")
        .map_err(|e| CandyError::Encode(format!("container write: {e}")))?;
    // Stream the coded video samples from the temp file.
    stream_samples_into(&v.samples_path, &v.sample_sizes, &mut file)?;
    // Audio samples (kept in RAM; audio is small relative to video).
    for f in audio.map(|a| &a.frames).into_iter().flatten() {
        file.write_all(&f.data)
            .map_err(|e| CandyError::Encode(format!("container write: {e}")))?;
    }
    let _ = std::fs::remove_file(&v.samples_path);
    Ok(())
}

/// Copy each coded sample (sized by `sizes`) from `path` into `out`, in order.
/// Uses a fixed 1 MiB copy buffer so peak memory stays tiny regardless of how
/// many / how large the samples are.
fn stream_samples_into(
    path: &Path,
    sizes: &[u32],
    out: &mut std::fs::File,
) -> Result<(), CandyError> {
    use std::io::Read;
    let mut src = std::fs::File::open(path)
        .map_err(|e| CandyError::Encode(format!("sample read: {e}")))?;
    let mut buf = vec![0u8; 1 << 20];
    for &sz in sizes {
        let mut remaining = sz as usize;
        while remaining > 0 {
            let n = remaining.min(buf.len());
            src.read_exact(&mut buf[..n])
                .map_err(|e| CandyError::Encode(format!("sample read: {e}")))?;
            out.write_all(&buf[..n])
                .map_err(|e| CandyError::Encode(format!("container write: {e}")))?;
            remaining -= n;
        }
    }
    Ok(())
}

/// Build the `moov` box. `v_off`/`a_off` are the absolute file offsets of the
/// first sample of each track (0 means "measure pass"). `private_metadata` is
/// embedded in a `udta` user-data box so the metadata survives in the
/// container's metadata area.
#[allow(clippy::too_many_arguments)]
fn build_moov(
    v: &EncodedVideo,
    nframes: u32,
    audio: Option<&AudioData>,
    v_sizes: &[u32],
    a_sizes: &[u32],
    a_dur: &[u32],
    total_ms: u64,
    v_off: u32,
    a_off: u32,
    private_metadata: &PrivateMeta,
) -> Vec<u8> {
    let has_audio = !a_sizes.is_empty();
    let next_track_id: u32 = if has_audio { 3 } else { 2 };

    let mvhd = full_box(b"mvhd", 0, 0, {
        let mut p = vec![];
        p.extend_from_slice(&0u32.to_be_bytes());
        p.extend_from_slice(&0u32.to_be_bytes());
        p.extend_from_slice(&1000u32.to_be_bytes()); // timescale 1000 (ms)
        p.extend_from_slice(&(total_ms as u32).to_be_bytes()); // duration in ms
        p.extend_from_slice(&0x0001_0000u32.to_be_bytes()); // rate
        p.extend_from_slice(&0x0100u16.to_be_bytes()); // volume
        p.extend_from_slice(&[0u8; 10]);
        p.extend_from_slice(&MATRIX);
        p.extend_from_slice(&[0u8; 24]);
        p.extend_from_slice(&next_track_id.to_be_bytes());
        p
    });

    let video_trak = build_video_trak(v, nframes, v_sizes, total_ms, v_off);
    let mut traks = vec![video_trak];
    if has_audio {
        if let Some(a) = audio {
            traks.push(build_audio_trak_mp4(a, a_sizes, a_dur, a_off));
        }
    }

    let mut moov_payload = vec![];
    moov_payload.extend_from_slice(&mvhd);
    for t in traks {
        moov_payload.extend_from_slice(&t);
    }
    // Embed private metadata in the moov user-data area.
    moov_payload.extend_from_slice(&build_meta_udta(&private_metadata.to_json()));
    // moov is a plain box (not a full box) containing mvhd + trak children.
    b(b"moov", moov_payload)
}

/// Build a `udta` box holding the private metadata as an iTunes-style
/// `meta`/`ilst`/`©cmt` (comment) entry.
///
/// Layout:
/// ```text
/// udta
///   meta (full box, v0/flags0)
///     hdlr (handler_type = "mdir", name = "candy")
///     ilst
///       ©cmt
///         data (full box, type=UTF-8, locale=0) -> JSON bytes
/// ```
fn build_meta_udta(json: &str) -> Vec<u8> {
    let data = full_box(b"data", 0, 0, {
        let mut p = vec![];
        p.extend_from_slice(&1u32.to_be_bytes()); // data_type = UTF-8
        p.extend_from_slice(&0u32.to_be_bytes()); // locale = 0
        p.extend_from_slice(json.as_bytes());
        p
    });
    // ©cmt = 0xA9 'c' 'm' 't'
    let cmt = b(&[0xA9, 0x63, 0x6D, 0x74], data);
    let ilst = b(b"ilst", cmt);
    let hdlr = full_box(b"hdlr", 0, 0, {
        let mut p = vec![];
        p.extend_from_slice(&0u32.to_be_bytes()); // pre_defined
        p.extend_from_slice(b"mdir"); // handler_type
        p.extend_from_slice(&[0u8; 12]); // reserved
        p.extend_from_slice(b"candy\0"); // name (null-terminated)
        p
    });
    let meta = full_box(b"meta", 0, 0, {
        let mut p = vec![];
        p.extend_from_slice(&hdlr);
        p.extend_from_slice(&ilst);
        p
    });
    b(b"udta", meta)
}

fn build_video_trak(
    v: &EncodedVideo,
    nframes: u32,
    v_sizes: &[u32],
    total_ms: u64,
    v_off: u32,
) -> Vec<u8> {
    let tkhd = full_box(b"tkhd", 0, 0x0000_0007, {
        let mut p = vec![];
        p.extend_from_slice(&0u32.to_be_bytes());
        p.extend_from_slice(&0u32.to_be_bytes());
        p.extend_from_slice(&1u32.to_be_bytes()); // track_id
        p.extend_from_slice(&0u32.to_be_bytes());
        p.extend_from_slice(&(total_ms as u32).to_be_bytes());
        p.extend_from_slice(&[0u8; 8]);
        p.extend_from_slice(&0u16.to_be_bytes()); // layer
        p.extend_from_slice(&0u16.to_be_bytes()); // alternate_group
        p.extend_from_slice(&0u16.to_be_bytes()); // volume (0 for video)
        p.extend_from_slice(&0u16.to_be_bytes());
        p.extend_from_slice(&MATRIX);
        p.extend_from_slice(&(v.width << 16).to_be_bytes());
        p.extend_from_slice(&(v.height << 16).to_be_bytes());
        p
    });

    let mdhd = full_box(b"mdhd", 0, 0, {
        let mut p = vec![];
        p.extend_from_slice(&0u32.to_be_bytes());
        p.extend_from_slice(&0u32.to_be_bytes());
        p.extend_from_slice(&v.fps.to_be_bytes()); // timescale
        p.extend_from_slice(&nframes.to_be_bytes()); // duration
        p.extend_from_slice(&0x55C4u16.to_be_bytes()); // language "und"
        p.extend_from_slice(&0u16.to_be_bytes());
        p
    });

    let hdlr = full_box(b"hdlr", 0, 0, {
        let mut p = vec![];
        p.extend_from_slice(&0u32.to_be_bytes());
        p.extend_from_slice(b"vide");
        p.extend_from_slice(&[0u8; 12]);
        p.extend_from_slice(b"candy video\0");
        p
    });

    let vmhd = full_box(b"vmhd", 1, 0, {
        let mut p = vec![];
        p.extend_from_slice(&0u16.to_be_bytes());
        p.extend_from_slice(&[0u8; 6]);
        p
    });

    let url = full_box(b"url ", 0, 1, vec![]);
    let dref = full_box(b"dref", 0, 0, {
        let mut p = vec![];
        p.extend_from_slice(&1u32.to_be_bytes());
        p.extend_from_slice(&url);
        p
    });
    let dinf = b(b"dinf", dref);

    let stsd = full_box(b"stsd", 0, 0, {
        let mut p = vec![];
        p.extend_from_slice(&1u32.to_be_bytes());
        p.extend_from_slice(&video_sample_entry(v));
        p
    });
    let stts = full_box(b"stts", 0, 0, {
        let mut p = vec![];
        p.extend_from_slice(&1u32.to_be_bytes());
        p.extend_from_slice(&nframes.to_be_bytes());
        p.extend_from_slice(&1u32.to_be_bytes());
        p
    });
    let stsc = full_box(b"stsc", 0, 0, {
        let mut p = vec![];
        p.extend_from_slice(&1u32.to_be_bytes());
        p.extend_from_slice(&1u32.to_be_bytes());
        p.extend_from_slice(&nframes.to_be_bytes());
        p.extend_from_slice(&1u32.to_be_bytes());
        p
    });
    let stsz = full_box(b"stsz", 0, 0, {
        let mut p = vec![];
        p.extend_from_slice(&0u32.to_be_bytes()); // sample_size (variable)
        p.extend_from_slice(&nframes.to_be_bytes());
        for s in v_sizes {
            p.extend_from_slice(&s.to_be_bytes());
        }
        p
    });
    let stco = full_box(b"stco", 0, 0, {
        let mut p = vec![];
        p.extend_from_slice(&1u32.to_be_bytes());
        p.extend_from_slice(&v_off.to_be_bytes());
        p
    });

    // stss (sync sample table): lists the *actual* keyframe sample numbers
    // (1-based) taken from `v.keyframes`. Marking every frame as a sync
    // sample is a lie that makes players trust a P-frame as independently
    // decodable, so scrubbing and thumbnail generation fail. At least one
    // sync sample is required; if the encoder reported none we fall back to
    // sample 1.
    let sync: Vec<u32> = v
        .keyframes
        .iter()
        .enumerate()
        .filter(|(_, k)| **k)
        .map(|(i, _)| (i + 1) as u32)
        .collect();
    let stss = full_box(b"stss", 0, 0, {
        let mut p = vec![];
        let n = sync.len().max(1) as u32;
        p.extend_from_slice(&n.to_be_bytes()); // entry_count
        if sync.is_empty() {
            p.extend_from_slice(&1u32.to_be_bytes());
        } else {
            for s in &sync {
                p.extend_from_slice(&s.to_be_bytes()); // sample_number (1-based)
            }
        }
        p
    });

    let stbl = b(b"stbl", {
        let mut p = vec![];
        p.extend_from_slice(&stsd);
        p.extend_from_slice(&stts);
        p.extend_from_slice(&stsc);
        p.extend_from_slice(&stss);
        p.extend_from_slice(&stsz);
        p.extend_from_slice(&stco);
        p
    });
    let minf = b(b"minf", {
        let mut p = vec![];
        p.extend_from_slice(&vmhd);
        p.extend_from_slice(&dinf);
        p.extend_from_slice(&stbl);
        p
    });
    let mdia = b(b"mdia", {
        let mut p = vec![];
        p.extend_from_slice(&mdhd);
        p.extend_from_slice(&hdlr);
        p.extend_from_slice(&minf);
        p
    });
    b(b"trak", {
        let mut p = vec![];
        p.extend_from_slice(&tkhd);
        p.extend_from_slice(&mdia);
        p
    })
}

fn video_sample_entry(v: &EncodedVideo) -> Vec<u8> {
    let name: [u8; 4] = if v.is_av1 { *b"av01" } else { *b"avc1" };
    let cfg_name: [u8; 4] = if v.is_av1 { *b"av1C" } else { *b"avcC" };
    let mut p = vec![];
    p.extend_from_slice(&[0u8; 6]); // reserved
    p.extend_from_slice(&1u16.to_be_bytes()); // data_reference_index
    p.extend_from_slice(&[0u8; 16]);
    p.extend_from_slice(&(v.width as u16).to_be_bytes());
    p.extend_from_slice(&(v.height as u16).to_be_bytes());
    p.extend_from_slice(&0x0048_0000u32.to_be_bytes());
    p.extend_from_slice(&0x0048_0000u32.to_be_bytes());
    p.extend_from_slice(&0u32.to_be_bytes());
    p.extend_from_slice(&1u16.to_be_bytes()); // frame_count
    p.extend_from_slice(&[0u8; 32]); // compressorname
    p.extend_from_slice(&24u16.to_be_bytes()); // depth
    p.extend_from_slice(&0xFFFFu16.to_be_bytes());
    p.extend_from_slice(&b(&cfg_name, v.codec_private.clone()));
    b(&name, p)
}

fn build_audio_trak_mp4(a: &AudioData, sizes: &[u32], durs: &[u32], a_off: u32) -> Vec<u8> {
    let tkhd = full_box(b"tkhd", 0, 0x0000_0007, {
        let mut p = vec![];
        p.extend_from_slice(&0u32.to_be_bytes());
        p.extend_from_slice(&0u32.to_be_bytes());
        p.extend_from_slice(&2u32.to_be_bytes()); // track_id
        p.extend_from_slice(&0u32.to_be_bytes());
        p.extend_from_slice(&0u32.to_be_bytes()); // duration (unknown exact)
        p.extend_from_slice(&[0u8; 8]);
        p.extend_from_slice(&0u16.to_be_bytes());
        p.extend_from_slice(&0u16.to_be_bytes());
        p.extend_from_slice(&0x0100u16.to_be_bytes()); // volume 1.0
        p.extend_from_slice(&0u16.to_be_bytes());
        p.extend_from_slice(&MATRIX);
        p.extend_from_slice(&0u32.to_be_bytes()); // width
        p.extend_from_slice(&0u32.to_be_bytes());
        p
    });
    let mdhd = full_box(b"mdhd", 0, 0, {
        let mut p = vec![];
        p.extend_from_slice(&0u32.to_be_bytes());
        p.extend_from_slice(&0u32.to_be_bytes());
        p.extend_from_slice(&a.sample_rate.to_be_bytes());
        p.extend_from_slice(&durs.iter().map(|d| *d as u64).sum::<u64>().to_be_bytes());
        p.extend_from_slice(&0x55C4u16.to_be_bytes());
        p.extend_from_slice(&0u16.to_be_bytes());
        p
    });
    let hdlr = full_box(b"hdlr", 0, 0, {
        let mut p = vec![];
        p.extend_from_slice(&0u32.to_be_bytes());
        p.extend_from_slice(b"soun");
        p.extend_from_slice(&[0u8; 12]);
        p.extend_from_slice(b"candy audio\0");
        p
    });
    let smhd = full_box(b"smhd", 0, 0, {
        let mut p = vec![];
        p.extend_from_slice(&0u16.to_be_bytes());
        p.extend_from_slice(&0u16.to_be_bytes());
        p
    });
    let url = full_box(b"url ", 0, 1, vec![]);
    let dref = full_box(b"dref", 0, 0, {
        let mut p = vec![];
        p.extend_from_slice(&1u32.to_be_bytes());
        p.extend_from_slice(&url);
        p
    });
    let dinf = b(b"dinf", dref);

    let stsd = full_box(b"stsd", 0, 0, {
        let mut p = vec![];
        p.extend_from_slice(&1u32.to_be_bytes());
        p.extend_from_slice(&audio_sample_entry_mp4(a));
        p
    });
    let mut stts_payload = vec![];
    stts_payload.extend_from_slice(&(durs.len() as u32).to_be_bytes());
    for d in durs {
        stts_payload.extend_from_slice(&1u32.to_be_bytes());
        stts_payload.extend_from_slice(&d.to_be_bytes());
    }
    let stts = full_box(b"stts", 0, 0, stts_payload);
    let stsc = full_box(b"stsc", 0, 0, {
        let mut p = vec![];
        p.extend_from_slice(&1u32.to_be_bytes());
        p.extend_from_slice(&1u32.to_be_bytes());
        p.extend_from_slice(&(sizes.len() as u32).to_be_bytes());
        p.extend_from_slice(&1u32.to_be_bytes());
        p
    });
    let mut stsz_payload = vec![];
    stsz_payload.extend_from_slice(&0u32.to_be_bytes());
    stsz_payload.extend_from_slice(&(sizes.len() as u32).to_be_bytes());
    for s in sizes {
        stsz_payload.extend_from_slice(&s.to_be_bytes());
    }
    let stsz = full_box(b"stsz", 0, 0, stsz_payload);
    let stco = full_box(b"stco", 0, 0, {
        let mut p = vec![];
        p.extend_from_slice(&1u32.to_be_bytes());
        p.extend_from_slice(&a_off.to_be_bytes());
        p
    });
    let stbl = b(b"stbl", {
        let mut p = vec![];
        p.extend_from_slice(&stsd);
        p.extend_from_slice(&stts);
        p.extend_from_slice(&stsc);
        p.extend_from_slice(&stsz);
        p.extend_from_slice(&stco);
        p
    });
    let minf = b(b"minf", {
        let mut p = vec![];
        p.extend_from_slice(&smhd);
        p.extend_from_slice(&dinf);
        p.extend_from_slice(&stbl);
        p
    });
    let mdia = b(b"mdia", {
        let mut p = vec![];
        p.extend_from_slice(&mdhd);
        p.extend_from_slice(&hdlr);
        p.extend_from_slice(&minf);
        p
    });
    b(b"trak", {
        let mut p = vec![];
        p.extend_from_slice(&tkhd);
        p.extend_from_slice(&mdia);
        p
    })
}

fn audio_sample_entry_mp4(a: &AudioData) -> Vec<u8> {
    let mut p = vec![];
    p.extend_from_slice(&[0u8; 6]);
    p.extend_from_slice(&1u16.to_be_bytes());
    p.extend_from_slice(&[0u8; 8]);
    p.extend_from_slice(&a.channels.to_be_bytes());
    p.extend_from_slice(&16u16.to_be_bytes());
    p.extend_from_slice(&0u16.to_be_bytes());
    p.extend_from_slice(&0u16.to_be_bytes());
    p.extend_from_slice(&(a.sample_rate << 16).to_be_bytes());
    p.extend_from_slice(&b(b"esds", esds(a)));
    b(b"mp4a", p)
}

/// Build an `esds` (Elementary Stream Descriptor) for AAC LC.
fn esds(a: &AudioData) -> Vec<u8> {
    let dsi = desc(0x05, a.codec_private.clone());
    let sl = desc(0x06, vec![0x02]);
    let mut dc = vec![];
    dc.push(0x40); // objectTypeIndication (AAC)
    dc.push(0x15); // streamType = audio, upstream=0, reserved=1
    dc.extend_from_slice(&[0u8; 3]); // bufferSizeDB
    dc.extend_from_slice(&0u32.to_be_bytes()); // maxBitrate
    dc.extend_from_slice(&0u32.to_be_bytes()); // avgBitrate
    dc.extend_from_slice(&dsi);
    dc.extend_from_slice(&sl);
    let dc = desc(0x04, dc);

    let mut es = vec![];
    es.extend_from_slice(&1u16.to_be_bytes()); // ES_ID
    es.push(0x00); // flags
    es.extend_from_slice(&dc);
    desc(0x03, es)
}

/// Wrap `payload` in an EBML-style descriptor: `[tag][len][payload]`.
fn desc(tag: u8, payload: Vec<u8>) -> Vec<u8> {
    let mut v = vec![tag];
    v.push(payload.len() as u8); // assumes < 128
    v.extend_from_slice(&payload);
    v
}

/// Identity matrix for tkhd/mvhd: 9 × u32 = 36 bytes.
/// `[1.0, 0, 0, 0, 1.0, 0, 0, 0, 1.0]` in 16.16 fixed-point
/// (except the last element which is 0x40000000 in 2.30 fixed-point).
const MATRIX: [u8; 36] = [
    0x00, 0x01, 0x00, 0x00, // a = 1.0
    0, 0, 0, 0, // b = 0
    0, 0, 0, 0, // u = 0
    0, 0, 0, 0, // c = 0
    0x00, 0x01, 0x00, 0x00, // d = 1.0
    0, 0, 0, 0, // v = 0
    0, 0, 0, 0, // x = 0
    0, 0, 0, 0, // y = 0
    0x40, 0x00, 0x00, 0x00, // w = 1.0 (2.30 fixed-point)
];

/// Build a box: `[size:4][type:4][payload]`.
fn b(name: &[u8; 4], payload: Vec<u8>) -> Vec<u8> {
    let mut v = vec![];
    v.extend_from_slice(&((payload.len() + 8) as u32).to_be_bytes());
    v.extend_from_slice(name);
    v.extend_from_slice(&payload);
    v
}

/// Build a full box: `[size:4][type:4][version:1][flags:3][payload]`.
fn full_box(name: &[u8; 4], version: u8, flags: u32, payload: Vec<u8>) -> Vec<u8> {
    let mut body = vec![version];
    body.extend_from_slice(&flags.to_be_bytes()[1..]); // 3-byte flags
    body.extend_from_slice(&payload);
    b(name, body)
}

// ======================== Matroska (WebM / MKV) ========================

/// Mux into Matroska. `webm` selects the `webm` doctype (AV1 + Opus), otherwise
/// `matroska` (AV1/H.264 + Opus/AAC). `private_metadata` is embedded in a
/// `Tags`/`SimpleTag` element (`TagName` = `candy-meta`) containing the compact
/// JSON, mirroring the metadata embedded in GIF comments and PNG tEXt chunks.
pub fn mux_matroska(
    v: &EncodedVideo,
    audio: Option<&AudioData>,
    webm: bool,
    private_metadata: &PrivateMeta,
) -> Result<Vec<u8>, CandyError> {
    let nframes = v.frames.len() as u32;
    if nframes == 0 {
        return Err(CandyError::Encode(
            "cannot mux an empty video (E007)".into(),
        ));
    }

    // Cluster plan (split to keep SimpleBlock timecodes < 2^15 ms).
    let mut clusters: Vec<(u64, usize, usize)> = Vec::new(); // (start_ms, first_frame, last_frame_excl)
    let mut c_start = 0usize;
    let mut c_start_ms = 0u64;
    let mut prev_ms = 0u64;
    for i in 0..nframes as usize {
        let ms = (i as u64) * 1000 / v.fps as u64;
        if i == 0 || ms - c_start_ms > 30_000 {
            if i > c_start {
                clusters.push((c_start_ms, c_start, i));
            }
            c_start = i;
            c_start_ms = ms;
        }
        prev_ms = ms;
    }
    if c_start < nframes as usize {
        clusters.push((c_start_ms, c_start, nframes as usize));
    }
    let last_ms = prev_ms;

    // Build Clusters.
    let mut cluster_bytes = Vec::new();
    for (c_ms, f0, f1) in &clusters {
        let mut c = Vec::new();
        c.extend_from_slice(&ebml_elem(&[0xE7], &(u64_to_bytes(*c_ms)))); // Timecode
        for f in *f0..*f1 {
            let ms = (f as u64) * 1000 / v.fps as u64;
            let rel = (ms - c_ms) as i16;
            // Only mark real keyframes (IDR / AV1 key frame) as seekable in the
            // block; lying here breaks seeking on players that trust the flag.
            let block = simple_block(1, rel, v.keyframes[f], &v.frames[f]);
            c.extend_from_slice(&ebml_elem(&[0xA3], &block));
        }
        // Audio blocks whose timestamp falls in this cluster range.
        if let Some(a) = audio {
            for (idx, af) in a.frames.iter().enumerate() {
                let ms = af.timestamp_ms;
                if ms >= *c_ms && ms <= last_ms && *f0 < nframes as usize {
                    let in_range = ms >= *c_ms
                        && (*f1 >= nframes as usize || ms < ((*f1 as u64) * 1000 / v.fps as u64));
                    if in_range {
                        let rel = (ms as i64 - *c_ms as i64) as i16;
                        let block = simple_block(2, rel, false, &af.data);
                        c.extend_from_slice(&ebml_elem(&[0xA3], &block));
                        let _ = idx;
                    }
                }
            }
        }
        cluster_bytes.extend_from_slice(&ebml_elem(&[0x1F, 0x43, 0xB6, 0x75], &c));
    }

    // Tracks.
    let mut tracks = Vec::new();
    tracks.extend_from_slice(&video_track_entry(v));
    if let Some(a) = audio {
        tracks.extend_from_slice(&audio_track_entry(a));
    }
    let tracks_el = ebml_elem(&[0x16, 0x54, 0xAE, 0x6B], &tracks);

    // Info.
    let duration = last_ms as f64;
    let mut info = Vec::new();
    info.extend_from_slice(&ebml_elem(&[0x2A, 0xD7, 0xB1], &u64_to_bytes(1_000_000))); // TimecodeScale (1ms)
    info.extend_from_slice(&ebml_elem(&[0x4D, 0x80], b"candy")); // MuxingApp
    info.extend_from_slice(&ebml_elem(&[0x57, 0x41], b"candy")); // WritingApp
    info.extend_from_slice(&ebml_elem(&[0x44, 0x89], &f64_to_bytes(duration))); // Duration
    let info_el = ebml_elem(&[0x15, 0x49, 0xA9, 0x66], &info);

    // Segment.
    let tags_el = build_tags(&private_metadata.to_json());
    let mut segment = Vec::new();
    segment.extend_from_slice(&info_el);
    segment.extend_from_slice(&tracks_el);
    segment.extend_from_slice(&tags_el);
    segment.extend_from_slice(&cluster_bytes);
    // Segment size: candy builds the whole file in memory, so the exact size is
    // known. A known-size vint is fully valid EBML and is what `mkvmerge`
    // emits. (ffmpeg's "unknown size" form is just an 8-byte vint of the max
    // value; both are accepted by strict parsers as long as the vint is
    // encoded with the single-marker-bit rule that `ebml_vint` now uses.)
    let seg_size = segment.len() as u64;
    let mut segment_el = Vec::new();
    segment_el.extend_from_slice(&[0x18, 0x53, 0x80, 0x67]); // Segment ID
    segment_el.extend_from_slice(&ebml_vint(seg_size)); // known exact size
    segment_el.extend_from_slice(&segment);

    // EBML header.
    let mut ebml = Vec::new();
    ebml.extend_from_slice(&ebml_elem(&[0x42, 0x86], &u64_to_bytes(1))); // EBMLVersion
    ebml.extend_from_slice(&ebml_elem(&[0x42, 0xF7], &u64_to_bytes(1))); // EBMLReadVersion
    ebml.extend_from_slice(&ebml_elem(&[0x42, 0xF2], &u64_to_bytes(4))); // EBMLMaxIDLength
    ebml.extend_from_slice(&ebml_elem(&[0x42, 0xF3], &u64_to_bytes(8))); // EBMLMaxSizeLength
    let doctype: &[u8] = if webm { b"webm" } else { b"matroska" };
    ebml.extend_from_slice(&ebml_elem(&[0x42, 0x82], doctype)); // DocType
    ebml.extend_from_slice(&ebml_elem(&[0x42, 0x87], &u64_to_bytes(2))); // DocTypeVersion
    ebml.extend_from_slice(&ebml_elem(&[0x42, 0x85], &u64_to_bytes(2))); // DocTypeReadVersion
    let ebml_el = ebml_elem(&[0x1A, 0x45, 0xDF, 0xA3], &ebml);

    let mut out = Vec::new();
    out.extend_from_slice(&ebml_el);
    out.extend_from_slice(&segment_el);
    Ok(out)
}

fn video_track_entry(v: &EncodedVideo) -> Vec<u8> {
    let codec_id: &[u8] = if v.is_av1 {
        b"V_AV1"
    } else {
        b"V_MPEG4/ISO/AVC"
    };
    let mut e = Vec::new();
    e.extend_from_slice(&ebml_elem(&[0xD7], &u64_to_bytes(1))); // TrackNumber
    e.extend_from_slice(&ebml_elem(&[0x73, 0xC5], &u64_to_bytes(1))); // TrackUID
    e.extend_from_slice(&ebml_elem(&[0x83], &u64_to_bytes(1))); // TrackType (video)
    e.extend_from_slice(&ebml_elem(&[0x9C], &u64_to_bytes(0))); // FlagLacing
    e.extend_from_slice(&ebml_elem(&[0x86], codec_id)); // CodecID
    e.extend_from_slice(&ebml_elem(&[0x63, 0xA2], &v.codec_private)); // CodecPrivate
    e.extend_from_slice(&ebml_elem(&[0x53, 0x6E], b"candy video")); // Name
    // Video
    let mut vid = Vec::new();
    vid.extend_from_slice(&ebml_elem(&[0xB0], &u64_to_bytes(v.width as u64))); // PixelWidth
    vid.extend_from_slice(&ebml_elem(&[0xBA], &u64_to_bytes(v.height as u64))); // PixelHeight
    e.extend_from_slice(&ebml_elem(&[0xE0], &vid));
    ebml_elem(&[0xAE], &e)
}

fn audio_track_entry(a: &AudioData) -> Vec<u8> {
    let codec_id: &[u8] = match a.codec {
        crate::renderer::audio::AudioCodec::Opus => b"A_Opus",
        crate::renderer::audio::AudioCodec::Aac => b"A_AAC",
    };
    let mut e = Vec::new();
    e.extend_from_slice(&ebml_elem(&[0xD7], &u64_to_bytes(2))); // TrackNumber
    e.extend_from_slice(&ebml_elem(&[0x73, 0xC5], &u64_to_bytes(2))); // TrackUID
    e.extend_from_slice(&ebml_elem(&[0x83], &u64_to_bytes(2))); // TrackType (audio)
    e.extend_from_slice(&ebml_elem(&[0x9C], &u64_to_bytes(0))); // FlagLacing
    e.extend_from_slice(&ebml_elem(&[0x86], codec_id)); // CodecID
    e.extend_from_slice(&ebml_elem(&[0x63, 0xA2], &a.codec_private)); // CodecPrivate
    e.extend_from_slice(&ebml_elem(&[0x53, 0x6E], b"candy audio")); // Name
    let mut aud = Vec::new();
    aud.extend_from_slice(&ebml_elem(&[0xB5], &f64_to_bytes(a.sample_rate as f64))); // SamplingFrequency
    aud.extend_from_slice(&ebml_elem(&[0x9F], &u64_to_bytes(a.channels as u64))); // Channels
    e.extend_from_slice(&ebml_elem(&[0xE1], &aud));
    ebml_elem(&[0xAE], &e)
}

/// Build a Matroska `Tags` element embedding the private metadata as a
/// `SimpleTag` (`TagName` = `candy-meta`, `TagString` = JSON).
///
/// Layout:
/// ```text
/// Tags (0x1254C367)
///   Tag (0x7373)
///     SimpleTag (0x67C8)
///       TagName (0x45A3)  = "candy-meta"
///       TagLanguage (0x447A) = "und"
///       TagString (0x4484) = JSON
/// ```
fn build_tags(json: &str) -> Vec<u8> {
    let mut simple = Vec::new();
    simple.extend_from_slice(&ebml_elem(&[0x45, 0xA3], b"candy-meta")); // TagName
    simple.extend_from_slice(&ebml_elem(&[0x44, 0x7A], b"und")); // TagLanguage
    simple.extend_from_slice(&ebml_elem(&[0x44, 0x84], json.as_bytes())); // TagString
    let simple_el = ebml_elem(&[0x67, 0xC8], &simple); // SimpleTag
    let tag_el = ebml_elem(&[0x73, 0x73], &simple_el); // Tag
    ebml_elem(&[0x12, 0x54, 0xC3, 0x67], &tag_el) // Tags
}

/// Build a Matroska SimpleBlock.
fn simple_block(track: u64, rel_timecode: i16, keyframe: bool, data: &[u8]) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&ebml_vint(track)); // track number (vint)
    b.extend_from_slice(&rel_timecode.to_be_bytes());
    let mut flags = 0u8;
    if keyframe {
        flags |= 0x80;
    }
    b.push(flags);
    b.extend_from_slice(data);
    b
}

/// Encode an unsigned integer as an EBML vint.
///
/// An `L`-byte vint carries `7 * L` data bits; its top `L` bits are a fixed
/// length marker (`1` followed by `L-1` zero bits, i.e. `0x80 >> (L-1)`) and
/// the remaining bits hold the value, 8 bits per subsequent byte (the first
/// byte's high `8-L` data bits are zero because `v < 2^(7L)`). This matches
/// ffmpeg's `ebml_read_num`, which derives `L` from the leading-zero count of
/// the first byte, clears that single marker bit, then shifts each following
/// byte in with `<< 8`. The data bytes must be emitted most-significant first
/// (ascending `i`); emitting them in reverse corrupts every `L >= 3` vint —
/// which was the original playback bug (the Segment / large Cluster sizes then
/// decoded to the wrong length and the whole file mis-framed).
///
/// A single-byte vint of value 127 would encode as `0xFF`, which EBML reserves
/// for "unknown length"; that one value is bumped to two bytes.
fn ebml_vint(v: u64) -> Vec<u8> {
    // Minimal length whose 7*L data-bit capacity holds `v`.
    let mut len = 1usize;
    while len < 8 && v >= (1u64 << (7 * len)) {
        len += 1;
    }
    if len == 1 && v == 127 {
        len = 2; // avoid the reserved single-byte 0xFF
    }
    let marker = 0x80u8 >> (len - 1);
    let mut out = Vec::with_capacity(len);
    out.push(marker | ((v >> (8 * (len - 1))) as u8));
    for i in 1..len {
        out.push(((v >> (8 * (len - 1 - i))) & 0xFF) as u8);
    }
    out
}

/// Build an EBML element: `[id][size][data]`.
fn ebml_elem(id: &[u8], data: &[u8]) -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(id);
    v.extend_from_slice(&ebml_vint(data.len() as u64));
    v.extend_from_slice(data);
    v
}

fn u64_to_bytes(v: u64) -> Vec<u8> {
    // Minimal big-endian EBML unsigned integer (no leading-zero padding, at
    // least 1 byte). Reference muxers (ffmpeg) emit integers this way, and
    // strict EBML parsers reject the inflated 8-byte form for small values.
    if v == 0 {
        return vec![0];
    }
    let mut b = v.to_be_bytes().to_vec();
    while b.len() > 1 && b[0] == 0 {
        b.remove(0);
    }
    b
}

fn f64_to_bytes(v: f64) -> Vec<u8> {
    v.to_be_bytes().to_vec()
}

/// Mux a file-backed encoded video (and optional audio) directly into `output`
/// for Matroska (WebM/MKV).
///
/// The EBML header, `Segment` info/tracks/tags, and the total `Segment` size are
/// computed in RAM (all small — only per-sample metadata), then each `Cluster`
/// is streamed to `output` by reading the coded samples from `v.samples_path`.
/// Peak memory is bounded to at most one sample in RAM at a time, so a long
/// HD/high-FPS render cannot OOM on the coded stream.
pub(crate) fn mux_matroska_to_file(
    v: &EncodedVideoFile,
    audio: Option<&AudioData>,
    webm: bool,
    output: &Path,
    private_metadata: &PrivateMeta,
) -> Result<(), CandyError> {
    let nframes = v.sample_sizes.len() as u32;
    if nframes == 0 {
        return Err(CandyError::Encode(
            "cannot mux an empty video (E007)".into(),
        ));
    }

    // Cluster plan (split to keep SimpleBlock timecodes < 2^15 ms) — identical
    // to `mux_matroska`.
    let mut clusters: Vec<(u64, usize, usize)> = Vec::new();
    let mut c_start = 0usize;
    let mut c_start_ms = 0u64;
    let mut prev_ms = 0u64;
    for i in 0..nframes as usize {
        let ms = (i as u64) * 1000 / v.fps as u64;
        if i == 0 || ms - c_start_ms > 30_000 {
            if i > c_start {
                clusters.push((c_start_ms, c_start, i));
            }
            c_start = i;
            c_start_ms = ms;
        }
        prev_ms = ms;
    }
    if c_start < nframes as usize {
        clusters.push((c_start_ms, c_start, nframes as usize));
    }
    let last_ms = prev_ms;

    // Tracks / Info / Tags (small, built in RAM).
    let mut tracks = Vec::new();
    tracks.extend_from_slice(&video_track_entry_shim(v));
    if let Some(a) = audio {
        tracks.extend_from_slice(&audio_track_entry(a));
    }
    let tracks_el = ebml_elem(&[0x16, 0x54, 0xAE, 0x6B], &tracks);

    let duration = last_ms as f64;
    let mut info = Vec::new();
    info.extend_from_slice(&ebml_elem(&[0x2A, 0xD7, 0xB1], &u64_to_bytes(1_000_000)));
    info.extend_from_slice(&ebml_elem(&[0x4D, 0x80], b"candy"));
    info.extend_from_slice(&ebml_elem(&[0x57, 0x41], b"candy"));
    info.extend_from_slice(&ebml_elem(&[0x44, 0x89], &f64_to_bytes(duration)));
    let info_el = ebml_elem(&[0x15, 0x49, 0xA9, 0x66], &info);

    let tags_el = build_tags(&private_metadata.to_json());

    // Total Cluster size (computed from sample sizes; no sample data buffered).
    let mut cluster_total = 0u64;
    for (c_ms, f0, f1) in &clusters {
        cluster_total += cluster_size(
            *c_ms, *f0, *f1, &v.sample_sizes, &v.keyframes, audio, last_ms, nframes, v.fps,
        );
    }
    let seg_size = info_el.len() as u64 + tracks_el.len() as u64 + tags_el.len() as u64 + cluster_total;

    // EBML header.
    let mut ebml = Vec::new();
    ebml.extend_from_slice(&ebml_elem(&[0x42, 0x86], &u64_to_bytes(1)));
    ebml.extend_from_slice(&ebml_elem(&[0x42, 0xF7], &u64_to_bytes(1)));
    ebml.extend_from_slice(&ebml_elem(&[0x42, 0xF2], &u64_to_bytes(4)));
    ebml.extend_from_slice(&ebml_elem(&[0x42, 0xF3], &u64_to_bytes(8)));
    let doctype: &[u8] = if webm { b"webm" } else { b"matroska" };
    ebml.extend_from_slice(&ebml_elem(&[0x42, 0x82], doctype));
    ebml.extend_from_slice(&ebml_elem(&[0x42, 0x87], &u64_to_bytes(2)));
    ebml.extend_from_slice(&ebml_elem(&[0x42, 0x85], &u64_to_bytes(2)));
    let ebml_el = ebml_elem(&[0x1A, 0x45, 0xDF, 0xA3], &ebml);

    let mut out = File::create(output)
        .map_err(|e| CandyError::Encode(format!("container write: {e}")))?;
    out.write_all(&ebml_el)
        .map_err(|e| CandyError::Encode(format!("container write: {e}")))?;
    out.write_all(&[0x18, 0x53, 0x80, 0x67])
        .map_err(|e| CandyError::Encode(format!("container write: {e}")))?;
    out.write_all(&ebml_vint(seg_size))
        .map_err(|e| CandyError::Encode(format!("container write: {e}")))?;
    out.write_all(&info_el)
        .map_err(|e| CandyError::Encode(format!("container write: {e}")))?;
    out.write_all(&tracks_el)
        .map_err(|e| CandyError::Encode(format!("container write: {e}")))?;
    out.write_all(&tags_el)
        .map_err(|e| CandyError::Encode(format!("container write: {e}")))?;

    // Stream clusters, reading coded samples sequentially from the temp file.
    let mut sf = File::open(&v.samples_path)
        .map_err(|e| CandyError::Encode(format!("sample read: {e}")))?;
    for (c_ms, f0, f1) in &clusters {
        write_cluster_to_file(&mut out, *c_ms, *f0, *f1, v, &mut sf, audio, last_ms, nframes, v.fps)?;
    }
    let _ = std::fs::remove_file(&v.samples_path);
    Ok(())
}

/// Build a Matroska `VideoTrackEntry` from a file-backed video (metadata only).
fn video_track_entry_shim(v: &EncodedVideoFile) -> Vec<u8> {
    let codec_id: &[u8] = if v.is_av1 {
        b"V_AV1"
    } else {
        b"V_MPEG4/ISO/AVC"
    };
    let mut e = Vec::new();
    e.extend_from_slice(&ebml_elem(&[0xD7], &u64_to_bytes(1)));
    e.extend_from_slice(&ebml_elem(&[0x73, 0xC5], &u64_to_bytes(1)));
    e.extend_from_slice(&ebml_elem(&[0x83], &u64_to_bytes(1)));
    e.extend_from_slice(&ebml_elem(&[0x9C], &u64_to_bytes(0)));
    e.extend_from_slice(&ebml_elem(&[0x86], codec_id));
    e.extend_from_slice(&ebml_elem(&[0x63, 0xA2], &v.codec_private));
    e.extend_from_slice(&ebml_elem(&[0x53, 0x6E], b"candy video"));
    let mut vid = Vec::new();
    vid.extend_from_slice(&ebml_elem(&[0xB0], &u64_to_bytes(v.width as u64)));
    vid.extend_from_slice(&ebml_elem(&[0xBA], &u64_to_bytes(v.height as u64)));
    e.extend_from_slice(&ebml_elem(&[0xE0], &vid));
    ebml_elem(&[0xAE], &e)
}

/// Size in bytes of an EBML element with the given `id` and `data_len`.
fn ebml_elem_size(id: &[u8], data_len: u64) -> u64 {
    id.len() as u64 + ebml_vint(data_len).len() as u64 + data_len
}

/// Size in bytes of a Matroska `SimpleBlock` for `track` carrying `data_len`
/// bytes of coded data.
fn simple_block_size(track: u64, data_len: u64) -> u64 {
    let block = ebml_vint(track).len() as u64 + 2 + 1 + data_len;
    ebml_elem_size(&[0xA3], block)
}

/// Total byte size of a single `Cluster` (without buffering its data), computed
/// from the per-sample sizes and the audio frames that fall inside it.
#[allow(clippy::too_many_arguments)]
fn cluster_size(
    c_ms: u64,
    f0: usize,
    f1: usize,
    v_sizes: &[u32],
    _keyframes: &[bool],
    audio: Option<&AudioData>,
    last_ms: u64,
    nframes: u32,
    fps: u32,
) -> u64 {
    let mut size = ebml_elem_size(&[0xE7], u64_to_bytes(c_ms).len() as u64);
    for &vs in &v_sizes[f0..f1] {
        size += simple_block_size(1, vs as u64);
    }
    if let Some(a) = audio {
        for af in &a.frames {
            let ms = af.timestamp_ms;
            if ms >= c_ms && ms <= last_ms && f0 < nframes as usize {
                let in_range =
                    ms >= c_ms && (f1 >= nframes as usize || ms < ((f1 as u64) * 1000 / fps as u64));
                if in_range {
                    size += simple_block_size(2, af.data.len() as u64);
                }
            }
        }
    }
    ebml_elem_size(&[0x1F, 0x43, 0xB6, 0x75], size)
}

/// Read the next `size` bytes from `sf` (sequential sample read).
fn read_next_sample(sf: &mut File, size: u32) -> Result<Vec<u8>, CandyError> {
    let mut buf = vec![0u8; size as usize];
    sf.read_exact(&mut buf)
        .map_err(|e| CandyError::Encode(format!("sample read: {e}")))?;
    Ok(buf)
}

/// Build and write one `Cluster` to `out`, streaming its coded video samples
/// from `sf` (read sequentially) and interleaving any audio blocks in range.
#[allow(clippy::too_many_arguments)]
fn write_cluster_to_file(
    out: &mut File,
    c_ms: u64,
    f0: usize,
    f1: usize,
    v: &EncodedVideoFile,
    sf: &mut File,
    audio: Option<&AudioData>,
    last_ms: u64,
    nframes: u32,
    fps: u32,
) -> Result<(), CandyError> {
    let mut c: Vec<u8> = Vec::new();
    c.extend_from_slice(&ebml_elem(&[0xE7], &u64_to_bytes(c_ms)));
    for f in f0..f1 {
        let ms = (f as u64) * 1000 / fps as u64;
        let rel = (ms - c_ms) as i16;
        let data = read_next_sample(sf, v.sample_sizes[f])?;
        let block = simple_block(1, rel, v.keyframes[f], &data);
        c.extend_from_slice(&ebml_elem(&[0xA3], &block));
    }
    if let Some(a) = audio {
        for af in &a.frames {
            let ms = af.timestamp_ms;
            if ms >= c_ms && ms <= last_ms && f0 < nframes as usize {
                let in_range =
                    ms >= c_ms && (f1 >= nframes as usize || ms < ((f1 as u64) * 1000 / fps as u64));
                if in_range {
                    let rel = (ms as i64 - c_ms as i64) as i16;
                    let block = simple_block(2, rel, false, &af.data);
                    c.extend_from_slice(&ebml_elem(&[0xA3], &block));
                }
            }
        }
    }
    out.write_all(&ebml_elem(&[0x1F, 0x43, 0xB6, 0x75], &c))
        .map_err(|e| CandyError::Encode(format!("container write: {e}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::meta::PrivateMeta;

    fn sample_meta() -> PrivateMeta {
        PrivateMeta::default()
    }

    fn sample_video() -> EncodedVideo {
        EncodedVideo {
            width: 16,
            height: 16,
            fps: 10,
            is_av1: false,
            frames: vec![vec![0u8; 16], vec![0u8; 16]],
            codec_private: vec![0u8; 4],
            keyframes: vec![true, false],
        }
    }

    #[test]
    fn mux_mp4_embeds_private_metadata_in_udta() {
        let v = sample_video();
        let meta = sample_meta();
        let bytes = mux_mp4(&v, None, &meta).unwrap();

        // udta / meta / ilst / ©cmt markers should all be present.
        assert!(
            bytes.windows(4).any(|w| w == b"udta"),
            "MP4 should contain a udta box"
        );
        assert!(
            bytes.windows(4).any(|w| w == b"meta"),
            "MP4 should contain a meta box"
        );
        assert!(
            bytes.windows(4).any(|w| w == [0xA9, 0x63, 0x6D, 0x74]),
            "MP4 should contain a ©cmt entry"
        );
        let expected = format!("\"codename\":\"{}\"", meta.codename);
        assert!(
            bytes
                .windows(expected.len())
                .any(|w| w == expected.as_bytes()),
            "MP4 metadata should contain private metadata JSON"
        );
    }

    #[test]
    fn mux_matroska_embeds_private_metadata_in_tags() {
        for webm in [false, true] {
            let v = sample_video();
            let meta = sample_meta();
            let bytes = mux_matroska(&v, None, webm, &meta).unwrap();

            // Tags / SimpleTag / TagName markers should be present.
            assert!(
                bytes.windows(4).any(|w| w == [0x12, 0x54, 0xC3, 0x67]),
                "Matroska should contain a Tags element"
            );
            assert!(
                bytes.windows(2).any(|w| w == [0x67, 0xC8]),
                "Matroska should contain a SimpleTag element"
            );
            assert!(
                bytes
                    .windows("candy-meta".len())
                    .any(|w| w == b"candy-meta"),
                "Matroska tag should be named candy-meta"
            );
            let expected = format!("\"codename\":\"{}\"", meta.codename);
            assert!(
                bytes
                    .windows(expected.len())
                    .any(|w| w == expected.as_bytes()),
                "Matroska metadata should contain private metadata JSON"
            );
        }
    }
}
