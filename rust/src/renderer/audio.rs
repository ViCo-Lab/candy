//! Audio ingestion for `candy.audio` tracks.
//!
//! Candy is self-contained: it does not shell out to `ffmpeg`. Instead it
//! demuxes the two most common *voice* formats in pure Rust and hands the raw
//! coded packets to the container muxer:
//!
//! * **Opus** in an Ogg container (`.opus` / `.ogg`) → Matroska (WebM/MKV).
//! * **AAC** in ADTS framing (`.aac`, or `.m4a` raw ADTS) → MP4.
//!
//! Unsupported combinations are reported so the pipeline can fall back to
//! attaching the file as a binary attachment (no data is lost).

use std::path::Path;

use crate::core::ast::AudioTrack;
use crate::core::error::CandyError;

/// Audio codec carried by an [`AudioData`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioCodec {
    Opus,
    Aac,
}

/// A single coded audio packet with its placement on the timeline.
#[derive(Debug, Clone)]
pub struct AudioFrame {
    /// Presentation timestamp, milliseconds from clip start.
    pub timestamp_ms: u64,
    /// Duration of the packet, milliseconds.
    pub duration_ms: u64,
    /// Coded packet bytes (Opus page / AAC raw frame).
    pub data: Vec<u8>,
}

/// Demuxed audio ready for muxing.
#[derive(Debug, Clone)]
pub struct AudioData {
    pub codec: AudioCodec,
    pub sample_rate: u32,
    pub channels: u16,
    /// `CodecPrivate`: OpusHead (Matroska) or AudioSpecificConfig (MP4).
    pub codec_private: Vec<u8>,
    pub frames: Vec<AudioFrame>,
}

impl AudioData {
    /// End timestamp (ms) of the last packet.
    pub fn end_ms(&self) -> u64 {
        self.frames
            .last()
            .map(|f| f.timestamp_ms + f.duration_ms)
            .unwrap_or(0)
    }
}

/// Parse `track.path` into [`AudioData`], honoring `slice`/`loop` hints.
pub fn parse_audio(track: &AudioTrack) -> Result<AudioData, CandyError> {
    let path = Path::new(&track.path);
    let bytes = std::fs::read(path)
        .map_err(|e| CandyError::Io(std::io::Error::new(e.kind(), format!("audio '{}': {e}", track.path))))?;

    let mut data = if is_opus(&bytes) {
        parse_opus_ogg(&bytes)?
    } else if is_adts(&bytes) {
        parse_adts_aac(&bytes)?
    } else {
        return Err(CandyError::Encode(format!(
            "unsupported audio format for '{}' (E007): candy supports Opus (.opus/.ogg) and \
             AAC/ADTS (.aac). Other formats are attached as files, not muxed.",
            track.path
        )));
    };

    // Apply `slice` (seconds) by dropping packets outside [start, end].
    if let Some((s, e)) = track.slice {
        let s_ms = (s * 1000.0) as u64;
        let e_ms = (e * 1000.0) as u64;
        data.frames.retain(|f| f.timestamp_ms + f.duration_ms > s_ms && f.timestamp_ms < e_ms);
        // Re-zero timestamps after the slice cut.
        let mut shift = 0i64;
        if let Some(first) = data.frames.first() {
            shift = first.timestamp_ms as i64;
        }
        for f in &mut data.frames {
            f.timestamp_ms = (f.timestamp_ms as i64 - shift).max(0) as u64;
        }
    }

    // Apply `loop`: append the clip once more (best-effort).
    if track.loop_track && !data.frames.is_empty() {
        let base = data.end_ms();
        let cloned: Vec<AudioFrame> = data
            .frames
            .iter()
            .map(|f| AudioFrame {
                timestamp_ms: f.timestamp_ms + base,
                duration_ms: f.duration_ms,
                data: f.data.clone(),
            })
            .collect();
        data.frames.extend(cloned);
    }

    Ok(data)
}

/// Detect an Ogg/Opus file (capture pattern `OggS` + `OpusHead`).
fn is_opus(bytes: &[u8]) -> bool {
    bytes.len() > 40 && &bytes[0..4] == b"OggS" && bytes.windows(8).any(|w| w == b"OpusHead")
}

/// Detect ADTS AAC (syncword `0xFFF` at a frame boundary).
fn is_adts(bytes: &[u8]) -> bool {
    bytes.len() >= 4 && bytes[0] == 0xFF && (bytes[1] & 0xF0) == 0xF0
}

/// Demux Opus packets from an Ogg container. Each Opus packet is 20 ms.
fn parse_opus_ogg(bytes: &[u8]) -> Result<AudioData, CandyError> {
    let packets = ogg_packets(bytes)?;
    // packet[0] = OpusHead: "OpusHead" + version(1) + channels(1) + preskip(2) + ...
    let head = packets
        .first()
        .ok_or_else(|| CandyError::Encode("Ogg/Opus: missing OpusHead (E007)".into()))?;
    if head.len() < 9 || &head[0..8] != b"OpusHead" {
        return Err(CandyError::Encode("Ogg/Opus: bad OpusHead (E007)".into()));
    }
    let channels = head[9] as u16;
    let sample_rate = 48_000; // Opus always decodes to 48 kHz
    let frame_ms: u64 = 20;

    let audio: Vec<&[u8]> = packets.iter().skip(2).map(|p| &p[..]).collect(); // skip head + tags
    let mut frames = Vec::with_capacity(audio.len());
    for (i, p) in audio.iter().enumerate() {
        frames.push(AudioFrame {
            timestamp_ms: (i as u64) * frame_ms,
            duration_ms: frame_ms,
            data: p.to_vec(),
        });
    }

    Ok(AudioData {
        codec: AudioCodec::Opus,
        sample_rate,
        channels,
        codec_private: head.to_vec(),
        frames,
    })
}

/// Demux raw AAC frames from ADTS framing. Each frame = 1024 samples.
fn parse_adts_aac(bytes: &[u8]) -> Result<AudioData, CandyError> {
    let freq_table = [
        96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350,
    ];
    let mut frames = Vec::new();
    let mut sample_rate = 44100u32;
    let mut channels: u16 = 2;
    let mut asc: u16 = 0x1190; // default LC, 44100, stereo
    let mut i = 0usize;
    while i + 7 <= bytes.len() {
        if bytes[i] != 0xFF || (bytes[i + 1] & 0xF0) != 0xF0 {
            i += 1;
            continue;
        }
        let frame_len = (((bytes[i + 1] & 0x03) as usize) << 11)
            | ((bytes[i + 2] as usize) << 3)
            | ((bytes[i + 3] as usize) >> 5);
        if frame_len < 7 || i + frame_len > bytes.len() {
            break;
        }
        let profile = (bytes[i + 2] >> 6) & 0x03;
        let freq_index = (bytes[i + 2] >> 2) & 0x0F;
        let ch = (((bytes[i + 2] & 0x01) as u16) << 2) | ((bytes[i + 3] >> 6) as u16);
        let sr = *freq_table.get(freq_index as usize).unwrap_or(&44100);
        let header_len = if (bytes[i + 1] & 0x01) == 1 { 7 } else { 9 };
        let aac = bytes[i + header_len..i + frame_len].to_vec();
        let dur_ms = (1024 * 1000 + sr as usize / 2) / sr as usize;

        if frames.is_empty() {
            sample_rate = sr;
            channels = ch;
            asc = (((profile + 1) as u16) << 11) | ((freq_index as u16) << 7) | (ch << 3);
        }
        frames.push(AudioFrame {
            timestamp_ms: (frames.len() as u64) * dur_ms as u64,
            duration_ms: dur_ms as u64,
            data: aac,
        });
        i += frame_len;
    }

    if frames.is_empty() {
        return Err(CandyError::Encode("ADTS AAC: no frames found (E007)".into()));
    }
    Ok(AudioData {
        codec: AudioCodec::Aac,
        sample_rate,
        channels,
        codec_private: asc.to_be_bytes().to_vec(),
        frames,
    })
}

/// Reassemble Ogg logical-stream packets from raw bytes.
fn ogg_packets(bytes: &[u8]) -> Result<Vec<Vec<u8>>, CandyError> {
    let mut packets = Vec::new();
    let mut i = 0usize;
    // (partial packet bytes, is_continuation)
    let mut carry: Option<Vec<u8>> = None;
    while i + 26 <= bytes.len() {
        if &bytes[i..i + 4] != b"OggS" {
            i += 1;
            continue;
        }
        let nsegs = bytes[i + 26] as usize;
        let seg_start = i + 27;
        if seg_start + nsegs > bytes.len() {
            break;
        }
        let continued = (bytes[i + 5] & 0x01) != 0;
        let mut packet = if continued {
            carry.take().unwrap_or_default()
        } else {
            Vec::new()
        };
        let mut seg_end = seg_start;
        let mut complete = false;
        for s in 0..nsegs {
            let len = bytes[seg_start + s] as usize;
            let start = seg_end;
            let end = start + len;
            if end > bytes.len() {
                return Err(CandyError::Encode("Ogg: truncated segment (E007)".into()));
            }
            packet.extend_from_slice(&bytes[start..end]);
            seg_end = end;
            if len < 255 {
                complete = true;
                break;
            }
        }
        if complete {
            packets.push(packet);
        } else {
            carry = Some(packet);
        }
        i = seg_end;
    }
    if let Some(p) = carry {
        packets.push(p);
    }
    Ok(packets)
}
