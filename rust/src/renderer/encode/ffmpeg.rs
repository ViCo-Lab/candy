//! Optional system-FFmpeg encoding path.
//!
//! When the system has `ffmpeg` on `$PATH`, candy can shell out to it for
//! codecs that have no pure-Rust encoder (x264, x265, VAAPI, VideoToolbox,
//! QSV). This module is the bridge: it pipes raw RGBA frames to ffmpeg's
//! stdin and reads the muxed container (MP4/MKV/WebM) from stdout.
//!
//! # No cargo dependency on ffmpeg
//!
//! ffmpeg is detected at runtime via `which ffmpeg` / `where ffmpeg`. If not
//! found, callers fall back to the self-contained codecs (rav1e/openh264).
//! This keeps candy's build self-contained by default while allowing users
//! with ffmpeg installed to access higher-quality / hardware codecs.
//!
//! # Pipeline
//!
//! ```text
//! candy ──RGBA stdin──▶ ffmpeg ──stdout──▶ muxed container bytes
//!         (rawvideo,      (-c:v libx264 /
//!          rgba,            libx265 /
//!          <w>x<h>)         h264_vaapi /
//!                          h264_videotoolbox /
//!                          h264_qsv / …)
//!                          (-f mp4/mkv/webm)
//! ```
//!
//! Audio is muxed in a second ffmpeg pass (candy decodes Opus/AAC itself,
//! pipes raw PCM to ffmpeg as a second input). This is simpler than teaching
//! candy's hand-written muxer to handle HEVC, and lets ffmpeg's mature muxer
//! handle all container/codec combinations.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::core::error::CandyError;
use crate::renderer::RenderedFrame;
use crate::renderer::encode::{Codec, Container};

/// Monotonic counter for unique ffmpeg temp-file names (avoids collisions
/// when multiple candy processes run concurrently).
static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Check whether `ffmpeg` is on `$PATH`. Returns the path if found.
pub fn find_ffmpeg() -> Option<PathBuf> {
    let exe = if cfg!(windows) { "ffmpeg.exe" } else { "ffmpeg" };
    let paths = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&paths) {
        let candidate = dir.join(exe);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// The ffmpeg encoder name and container format for a given candy [`Codec`].
///
/// Returns `(encoder_name, output_format, file_extension)`. Returns `None`
/// for self-contained codecs (Av1, H264) — those don't use ffmpeg.
fn ffmpeg_args(codec: Codec) -> Option<(&'static str, &'static str)> {
    match codec {
        Codec::X264 => Some(("libx264", "mp4")),
        Codec::X265 => Some(("libx265", "mp4")),
        Codec::H264Vaapi => Some(("h264_vaapi", "mp4")),
        Codec::H265Vaapi => Some(("hevc_vaapi", "mp4")),
        Codec::H264VideoToolbox => Some(("h264_videotoolbox", "mp4")),
        Codec::H265VideoToolbox => Some(("hevc_videotoolbox", "mp4")),
        Codec::H264Qsv => Some(("h264_qsv", "mp4")),
        Codec::H265Qsv => Some(("hevc_qsv", "mp4")),
        // H265 (the "self-contained or ffmpeg" variant) uses x265 when ffmpeg
        // is available.
        Codec::H265 => Some(("libx265", "mp4")),
        // Self-contained codecs don't go through ffmpeg.
        Codec::Av1 | Codec::H264 => None,
    }
}

/// Map a candy [`Container`] to an ffmpeg `-f` format name.
fn container_format(container: Container) -> &'static str {
    match container {
        Container::Mp4 => "mp4",
        Container::Mkv => "matroska",
        Container::Webm => "webm",
    }
}

/// Encode `frames` to a muxed container byte buffer via system ffmpeg.
///
/// # Arguments
/// * `frames` — RGBA8 frames, all composed to the same `width × height`.
/// * `fps` — frames per second.
/// * `codec` — which ffmpeg encoder to use (X264/X265/Vaapi/...).
/// * `container` — output container (MP4/MKV/WebM).
///
/// # Errors
/// Returns `CandyError::Encode` (E007) if ffmpeg is not found, exits non-zero,
/// or writes no output.
pub fn encode_via_ffmpeg(
    frames: &[RenderedFrame],
    fps: u32,
    codec: Codec,
    container: Container,
) -> Result<Vec<u8>, CandyError> {
    let ffmpeg = find_ffmpeg().ok_or_else(|| {
        CandyError::Encode("ffmpeg not found on $PATH (E007)".into())
    })?;

    let (encoder, _default_ext) = ffmpeg_args(codec).ok_or_else(|| {
        CandyError::Encode(format!("codec {codec:?} does not use ffmpeg"))
    })?;
    let format = container_format(container);

    if frames.is_empty() {
        return Err(CandyError::Encode("no frames to encode".into()));
    }
    let w = frames[0].width;
    let h = frames[0].height;

    // ffmpeg's MP4/MKV/WebM muxers require *seekable* output, and MP4's
    // `faststart` moov rewrite is impossible on a pipe — so piping ffmpeg's
    // output to stdout always fails ("muxer does not support non seekable
    // output"). Instead we write to a unique temp file (seekable) and read the
    // bytes back, which works for every container and keeps faststart.
    let tmp_ext = container_format(container);
    let tmp_name = format!(
        "candy_ff_{}_{}.{tmp_ext}",
        std::process::id(),
        TMP_COUNTER.fetch_add(1, Ordering::Relaxed),
    );
    let tmp_path = std::env::temp_dir().join(tmp_name);

    // Build the ffmpeg command. Order matters for hardware encoders: a render
    // node / device must be declared *before* the input is read, and hardware
    // encoders need the raw RGBA frames uploaded to a hardware surface (not
    // passed straight through). Software lib encoders (x264/x265) instead want
    // `-preset`/`-crf` — options that VAAPI / VideoToolbox / QSV reject.
    //
    //   ffmpeg [-vaapi_device /dev/dri/renderD128] \
    //          -f rawvideo -pix_fmt rgba -s WxH -r FPS -i - \
    //          -c:v <encoder> [-vf ...] [-qp|-b:v|-preset|-crf ...] \
    //          -f <format> -movflags +faststart <tmpfile>
    let bitrate = ((w as u64 * h as u64 * fps as u64) / 20)
        .clamp(120_000, 20_000_000);
    let bitrate_str = bitrate.to_string();

    let mut cmd = Command::new(&ffmpeg);
    // VAAPI needs its render node declared up front (a global option, before -i).
    if matches!(codec, Codec::H264Vaapi | Codec::H265Vaapi) {
        cmd.arg("-vaapi_device").arg("/dev/dri/renderD128");
    }
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        // Input: raw RGBA from stdin.
        .args(["-f", "rawvideo"])
        .args(["-pix_fmt", "rgba"])
        .args(["-s", &format!("{w}x{h}")])
        .args(["-r", &fps.to_string()])
        .args(["-i", "-"])
        // Output codec.
        .args(["-c:v", encoder]);

    // Per-codec options.
    match codec {
        // Software lib encoders: quality preset + CRF + strip alpha (x265 rejects
        // the alpha channel; yuv420p is the universally-supported format).
        Codec::X264 | Codec::X265 | Codec::H265 => {
            cmd.args(["-preset", "medium"]);
            cmd.args(["-crf", "23"]);
            cmd.args(["-vf", "format=yuv420p"]);
        }
        // VAAPI hardware encoders: upload the RGBA frames to a VAAPI hardware
        // surface (nv12) before encoding. `-qp` is VAAPI's constant-quality
        // control — it does NOT accept `-crf`/`-preset`.
        Codec::H264Vaapi | Codec::H265Vaapi => {
            cmd.args(["-vf", "format=nv12,hwupload"]);
            cmd.args(["-qp", "24"]);
        }
        // VideoToolbox (macOS): constant-bitrate control.
        Codec::H264VideoToolbox | Codec::H265VideoToolbox => {
            cmd.args(["-b:v", &bitrate_str]);
        }
        // Intel QSV: init the QSV device, upload to hardware, constant-bitrate.
        Codec::H264Qsv | Codec::H265Qsv => {
            cmd.args(["-init_hw_device", "qsv=qsv:/dev/dri/renderD128"]);
            cmd.args(["-vf", "format=nv12,hwupload=extra_hw_frames=64"]);
            cmd.args(["-b:v", &bitrate_str]);
        }
        // Self-contained codecs (Av1/H264) never reach here — `encode_via_ffmpeg`
        // is only called for ffmpeg-backed codecs — but the match must be total.
        _ => {}
    }

    // Output container (written to the temp file).
    cmd.args(["-f", format])
        .args(["-y", tmp_path.to_str().unwrap_or("/dev/null")]);

    // MP4 with faststart for web streaming (valid because the output is a
    // seekable file, not a pipe).
    if matches!(container, Container::Mp4) {
        cmd.args(["-movflags", "+faststart"]);
    }

    let mut child = cmd.spawn().map_err(|e| {
        CandyError::Encode(format!("failed to spawn ffmpeg: {e}"))
    })?;

    // Feed RGBA frames to stdin.
    let mut stdin = child.stdin.take().ok_or_else(|| {
        CandyError::Encode("ffmpeg stdin not captured".into())
    })?;
    for f in frames {
        stdin.write_all(&f.rgba).map_err(|e| {
            CandyError::Encode(format!("ffmpeg stdin write: {e}"))
        })?;
    }
    drop(stdin); // close stdin → ffmpeg finishes encoding

    let output = child.wait_with_output().map_err(|e| {
        CandyError::Encode(format!("ffmpeg wait: {e}"))
    })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let _ = std::fs::remove_file(&tmp_path);
        return Err(CandyError::Encode(format!(
            "ffmpeg exited with {}: {}",
            output.status,
            stderr.lines().take(20).collect::<Vec<_>>().join("\n")
        )));
    }

    let bytes = std::fs::read(&tmp_path).map_err(|e| {
        CandyError::Encode(format!("ffmpeg temp read: {e}"))
    })?;
    let _ = std::fs::remove_file(&tmp_path);

    if bytes.is_empty() {
        return Err(CandyError::Encode(
            "ffmpeg produced no output (E007)".into(),
        ));
    }

    eprintln!(
        "info: encoded {} frames via ffmpeg -c:v {encoder} -f {format} ({} bytes)",
        frames.len(),
        bytes.len()
    );
    Ok(bytes)
}
