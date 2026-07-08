//! Self-contained media container muxers (no FFmpeg / external tools).
//!
//! * [`mux_mp4`]      — MP4 (ISO BMFF) for AV1 (`av01`+`av1C`) or H.264
//!   (`avc1`+`avcC`), optionally with an AAC audio track.
//! * [`mux_matroska`] — Matroska for WebM (`webm`) or MKV (`matroska`), AV1 or
//!   H.264 video, optionally Opus or AAC audio.
//!
//! All byte layout is written by hand so `candy` is fully self-contained.

use crate::core::error::CandyError;
use crate::renderer::audio::AudioData;
use crate::renderer::EncodedVideo;

/// Mux an encoded video (and optional audio) into an MP4 file.
pub fn mux_mp4(v: &EncodedVideo, audio: Option<&AudioData>) -> Result<Vec<u8>, CandyError> {
    let nframes = v.frames.len() as u32;
    if nframes == 0 {
        return Err(CandyError::Encode("cannot mux an empty video (E007)".into()));
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
                .map(|f| ((f.duration_ms as u64 * a.sample_rate as u64) / 1000) as u32)
                .collect();
            let total: u64 = durs.iter().map(|d| *d as u64).sum();
            (sizes, durs, total)
        }
        Some(_) => {
            eprintln!("warn: [E007] MP4 only muxes AAC audio; ignoring non-AAC track");
            (vec![], vec![], 0)
        }
        None => (vec![], vec![], 0),
    };
    let a_bytes: usize = a_sizes.iter().map(|s| *s as usize).sum();

    let ftyp = b(
        b"isom",
        {
            let mut p = vec![];
            p.extend_from_slice(b"isom");
            p.extend_from_slice(&[0, 0, 0, 0]);
            p.extend_from_slice(b"isom");
            p.extend_from_slice(if v.is_av1 { b"av01" } else { b"avc1" });
            p
        },
    );

    let moov0 = build_moov(v, audio, &v_sizes, &a_sizes, &a_dur_samples, total_ms, 0, 0);
    let moov_len = moov0.len();
    let mdat_offset = ftyp.len() + moov_len + 8;
    let video_offset = mdat_offset;
    let audio_offset = mdat_offset + v_bytes;
    let moov = build_moov(
        v,
        audio,
        &v_sizes,
        &a_sizes,
        &a_dur_samples,
        total_ms,
        video_offset as u32,
        audio_offset as u32,
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

/// Build the `moov` box. `v_off`/`a_off` are the absolute file offsets of the
/// first sample of each track (0 means "measure pass").
fn build_moov(
    v: &EncodedVideo,
    audio: Option<&AudioData>,
    v_sizes: &[u32],
    a_sizes: &[u32],
    a_dur: &[u32],
    total_ms: u64,
    v_off: u32,
    a_off: u32,
) -> Vec<u8> {
    let nframes = v.frames.len() as u32;
    let has_audio = !a_sizes.is_empty();
    let next_track_id: u32 = if has_audio { 3 } else { 2 };

    let mvhd = full_box(
        b"mvhd",
        0,
        0,
        {
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
        },
    );

    let video_trak = build_video_trak(v, nframes, v_sizes, total_ms, v_off);
    let mut traks = vec![video_trak];
    if has_audio {
        if let Some(a) = audio {
            traks.push(build_audio_trak_mp4(a, a_sizes, a_dur, a_off));
        }
    }

    let mut moov = full_box(b"moov", 0, 0, vec![]);
    moov.extend_from_slice(&mvhd);
    for t in traks {
        moov.extend_from_slice(&t);
    }
    moov
}

fn build_video_trak(
    v: &EncodedVideo,
    nframes: u32,
    v_sizes: &[u32],
    total_ms: u64,
    v_off: u32,
) -> Vec<u8> {
    let tkhd = full_box(
        b"tkhd",
        0,
        0x0000_0007,
        {
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
            p.extend_from_slice(&((v.width as u32) << 16).to_be_bytes());
            p.extend_from_slice(&((v.height as u32) << 16).to_be_bytes());
            p
        },
    );

    let mdhd = full_box(
        b"mdhd",
        0,
        0,
        {
            let mut p = vec![];
            p.extend_from_slice(&0u32.to_be_bytes());
            p.extend_from_slice(&0u32.to_be_bytes());
            p.extend_from_slice(&(v.fps as u32).to_be_bytes()); // timescale
            p.extend_from_slice(&nframes.to_be_bytes()); // duration
            p.extend_from_slice(&0x55C4u16.to_be_bytes()); // language "und"
            p.extend_from_slice(&0u16.to_be_bytes());
            p
        },
    );

    let hdlr = full_box(
        b"hdlr",
        0,
        0,
        {
            let mut p = vec![];
            p.extend_from_slice(&0u32.to_be_bytes());
            p.extend_from_slice(b"vide");
            p.extend_from_slice(&[0u8; 12]);
            p.extend_from_slice(b"candy video\0");
            p
        },
    );

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
    let tkhd = full_box(
        b"tkhd",
        0,
        0x0000_0007,
        {
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
        },
    );
    let mdhd = full_box(
        b"mdhd",
        0,
        0,
        {
            let mut p = vec![];
            p.extend_from_slice(&0u32.to_be_bytes());
            p.extend_from_slice(&0u32.to_be_bytes());
            p.extend_from_slice(&a.sample_rate.to_be_bytes());
            p.extend_from_slice(&durs.iter().map(|d| *d as u64).sum::<u64>().to_be_bytes());
            p.extend_from_slice(&0x55C4u16.to_be_bytes());
            p.extend_from_slice(&0u16.to_be_bytes());
            p
        },
    );
    let hdlr = full_box(
        b"hdlr",
        0,
        0,
        {
            let mut p = vec![];
            p.extend_from_slice(&0u32.to_be_bytes());
            p.extend_from_slice(b"soun");
            p.extend_from_slice(&[0u8; 12]);
            p.extend_from_slice(b"candy audio\0");
            p
        },
    );
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
    p.extend_from_slice(&((a.sample_rate as u32) << 16).to_be_bytes());
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

const MATRIX: [u8; 44] = [
    0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0x40, 0, 0, 0,
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
/// `matroska` (AV1/H.264 + Opus/AAC).
pub fn mux_matroska(
    v: &EncodedVideo,
    audio: Option<&AudioData>,
    webm: bool,
) -> Result<Vec<u8>, CandyError> {
    let nframes = v.frames.len() as u32;
    if nframes == 0 {
        return Err(CandyError::Encode("cannot mux an empty video (E007)".into()));
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
            let block = simple_block(1, rel, true, &v.frames[f]);
            c.extend_from_slice(&ebml_elem(&[0xA3], &block));
        }
        // Audio blocks whose timestamp falls in this cluster range.
        if let Some(a) = audio {
            for (idx, af) in a.frames.iter().enumerate() {
                let ms = af.timestamp_ms;
                if ms >= *c_ms && ms <= last_ms && ms >= *c_ms && *f0 < nframes as usize {
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
    let mut segment = Vec::new();
    segment.extend_from_slice(&info_el);
    segment.extend_from_slice(&tracks_el);
    segment.extend_from_slice(&cluster_bytes);
    let segment_el = ebml_elem(&[0x18, 0x53, 0x80, 0x67], &segment);

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

/// Encode an integer as an EBML vint (with leading marker bits).
///
/// An `L`-byte vint marks its length by setting the top `L` bits of the first
/// byte to `1` (e.g. `L=1` → `0x80`, `L=2` → `0xC0`, `L=3` → `0xE0`). The
/// remaining bits hold the most-significant base-128 digit of the value.
fn ebml_vint(v: u64) -> Vec<u8> {
    let mut groups: Vec<u8> = Vec::new();
    let mut val = v;
    if val == 0 {
        groups.push(0);
    } else {
        while val > 0 {
            groups.push((val & 0x7F) as u8);
            val >>= 7;
        }
        groups.reverse();
    }
    let len = groups.len().max(1);
    // Top `len` bits set to mark an `len`-byte vint.
    let marker = (((1u16 << len) - 1) << (8 - len)) as u8;
    groups[0] |= marker;
    groups
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
    v.to_be_bytes().to_vec()
}

fn f64_to_bytes(v: f64) -> Vec<u8> {
    v.to_be_bytes().to_vec()
}
