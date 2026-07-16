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
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::core::diag::CandyError;
use crate::core::meta::PrivateMeta;
use crate::info;
use crate::renderer::RenderedFrame;
use crate::renderer::encode::{Codec, Container};

/// Monotonic counter for unique ffmpeg temp-file names (avoids collisions
/// when multiple candy processes run concurrently).
static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Check whether `ffmpeg` is on `$PATH`. Returns the path if found.
pub fn find_ffmpeg() -> Option<PathBuf> {
    let exe = if cfg!(windows) {
        "ffmpeg.exe"
    } else {
        "ffmpeg"
    };
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

/// Spawn an ffmpeg child that reads raw RGBA frames of size `w×h` from stdin
/// and writes a muxed `container` to a temp file. Returns the child process, its
/// stdin handle (the caller writes frames to it, then drops it), and the temp
/// file path (passed back to [`finish_ffmpeg`]).
///
/// This is the streaming primitive behind [`encode_via_ffmpeg`]: the caller can
/// feed frames one at a time instead of buffering every RGBA frame up front.
pub(crate) fn spawn_ffmpeg(
    codec: Codec,
    container: Container,
    w: u32,
    h: u32,
    fps: u32,
    private_metadata: &PrivateMeta,
) -> Result<(Child, ChildStdin, PathBuf), CandyError> {
    let ffmpeg = find_ffmpeg()
        .ok_or_else(|| CandyError::Encode("ffmpeg not found on $PATH (E007)".into()))?;

    let (encoder, _default_ext) = ffmpeg_args(codec)
        .ok_or_else(|| CandyError::Encode(format!("codec {codec:?} does not use ffmpeg")))?;
    let format = container_format(container);

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
    let bitrate = ((w as u64 * h as u64 * fps as u64) / 20).clamp(120_000, 20_000_000);
    let bitrate_str = bitrate.to_string();

    let mut cmd = Command::new(&ffmpeg);
    if matches!(codec, Codec::H264Vaapi | Codec::H265Vaapi) {
        cmd.arg("-vaapi_device").arg("/dev/dri/renderD128");
    }
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .args(["-f", "rawvideo"])
        .args(["-pix_fmt", "rgba"])
        .args(["-s", &format!("{w}x{h}")])
        .args(["-r", &fps.to_string()])
        .args(["-i", "-"])
        .args(["-c:v", encoder]);

    match codec {
        Codec::X264 | Codec::X265 | Codec::H265 => {
            cmd.args(["-preset", "medium"]);
            cmd.args(["-crf", "23"]);
            cmd.args(["-vf", "format=yuv420p"]);
        }
        Codec::H264Vaapi | Codec::H265Vaapi => {
            cmd.args(["-vf", "format=nv12,hwupload"]);
            cmd.args(["-qp", "24"]);
        }
        Codec::H264VideoToolbox | Codec::H265VideoToolbox => {
            cmd.args(["-b:v", &bitrate_str]);
        }
        Codec::H264Qsv | Codec::H265Qsv => {
            cmd.args(["-init_hw_device", "qsv=qsv:/dev/dri/renderD128"]);
            cmd.args(["-vf", "format=nv12,hwupload=extra_hw_frames=64"]);
            cmd.args(["-b:v", &bitrate_str]);
        }
        _ => {}
    }

    cmd.arg("-metadata")
        .arg(format!("candy-meta={}", private_metadata.to_json()));

    cmd.args(["-f", format])
        .args(["-y", tmp_path.to_str().unwrap_or("/dev/null")]);

    if matches!(container, Container::Mp4) {
        cmd.args(["-movflags", "+faststart"]);
    }

    let mut child = cmd
        .spawn()
        .map_err(|e| CandyError::Encode(format!("failed to spawn ffmpeg: {e}")))?;

    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| CandyError::Encode("ffmpeg stdin not captured".into()))?;

    info!("spawned ffmpeg -c:v {encoder} -f {format} (streaming)");
    Ok((child, stdin, tmp_path))
}

/// Finish an ffmpeg encode started by [`spawn_ffmpeg`]: the child's stdin must
/// already be dropped/closed so ffmpeg flushes, then we wait and read back the
/// muxed container bytes. Used by the batch [`encode_via_ffmpeg`] path (which
/// already holds every frame in RAM); the streaming pipeline uses
/// [`finish_ffmpeg_to_file`] instead to avoid buffering the container.
pub(crate) fn finish_ffmpeg(child: Child, tmp_path: &Path) -> Result<Vec<u8>, CandyError> {
    let output = child
        .wait_with_output()
        .map_err(|e| CandyError::Encode(format!("ffmpeg wait: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let _ = std::fs::remove_file(tmp_path);
        return Err(CandyError::Encode(format!(
            "ffmpeg exited with {}: {}",
            output.status,
            stderr.lines().take(20).collect::<Vec<_>>().join("\n")
        )));
    }

    let bytes = std::fs::read(tmp_path)
        .map_err(|e| CandyError::Encode(format!("ffmpeg temp read: {e}")))?;
    let _ = std::fs::remove_file(tmp_path);

    if bytes.is_empty() {
        return Err(CandyError::Encode(
            "ffmpeg produced no output (E007)".into(),
        ));
    }
    Ok(bytes)
}

/// Finish an ffmpeg encode started by [`spawn_ffmpeg`]: the child's stdin must
/// already be dropped/closed so ffmpeg flushes, then we wait and copy the muxed
/// container (already a seekable temp file) directly to `output`. Copying the
/// file avoids buffering the entire container in RAM, so a long HD/high-FPS
/// render cannot OOM on the coded stream.
pub(crate) fn finish_ffmpeg_to_file(
    mut child: Child,
    tmp_path: &Path,
    output: &Path,
) -> Result<(), CandyError> {
    let status = child
        .wait()
        .map_err(|e| CandyError::Encode(format!("ffmpeg wait: {e}")))?;

    if !status.success() {
        let _ = std::fs::remove_file(tmp_path);
        return Err(CandyError::Encode(format!(
            "ffmpeg exited with {status} (E007); run with verbose logging for details"
        )));
    }

    std::fs::copy(tmp_path, output)
        .map_err(|e| CandyError::Encode(format!("ffmpeg output copy: {e}")))?;
    let _ = std::fs::remove_file(tmp_path);
    Ok(())
}

/// Encode `frames` to a muxed container byte buffer via system ffmpeg.
///
/// Batch wrapper over [`spawn_ffmpeg`]/[`finish_ffmpeg`]; the streaming path
/// feeds frames one at a time instead of buffering them all.
///
/// # Errors
/// Returns `CandyError::Encode` (E007) if ffmpeg is not found, exits non-zero,
/// or writes no output.
pub fn encode_via_ffmpeg(
    frames: &[RenderedFrame],
    fps: u32,
    codec: Codec,
    container: Container,
    private_metadata: &PrivateMeta,
) -> Result<Vec<u8>, CandyError> {
    if frames.is_empty() {
        return Err(CandyError::Encode("no frames to encode".into()));
    }
    let w = frames[0].width;
    let h = frames[0].height;
    let (child, mut stdin, tmp_path) =
        spawn_ffmpeg(codec, container, w as u32, h as u32, fps, private_metadata)?;
    for f in frames {
        stdin
            .write_all(&f.rgba)
            .map_err(|e| CandyError::Encode(format!("ffmpeg stdin write: {e}")))?;
    }
    drop(stdin); // close stdin → ffmpeg finishes encoding
    finish_ffmpeg(child, &tmp_path)
}
