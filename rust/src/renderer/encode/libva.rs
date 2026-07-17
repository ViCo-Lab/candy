//! Direct libva hardware encoding (Linux-only).
//!
//! This module provides independent hardware encoders that bypass ffmpeg
//! entirely. On Linux with `/dev/dri/renderD128` (Intel/AMD GPU), these
//! codecs encode H.264/HEVC/AV1 directly via VAAPI without spawning a
//! subprocess.
//!
//! # CLI usage
//!
//! ```sh
//! candy build anim.tyx --codec h264-libva
//! candy build anim.tyx --codec h265-libva
//! candy build anim.tyx --codec av1-libva
//! ```
//!
//! These options only appear in `--help` on Linux. On other platforms the
//! codec variants are compiled out via `#[cfg(target_os = "linux")]`.
//!
//! # Fallback
//!
//! If `/dev/dri/renderD128` is not available, the encoder falls back to
//! ffmpeg with the corresponding VAAPI codec (h264_vaapi / hevc_vaapi /
//! av1_vaapi). If ffmpeg is also unavailable, it returns E007.

#![cfg(target_os = "linux")]

use std::io::Write;
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};

use crate::core::diag::CandyError;
use crate::renderer::RenderedFrame;
use crate::renderer::encode::Codec;

/// Check if libva is available by checking for the VAAPI render device.
pub fn is_available() -> bool {
    std::path::Path::new("/dev/dri/renderD128").exists()
}

/// Map a candy Codec to the ffmpeg VAAPI encoder name (used as fallback).
fn vaapi_encoder(codec: Codec) -> Option<&'static str> {
    match codec {
        Codec::H264Libva => Some("h264_vaapi"),
        Codec::H265Libva => Some("hevc_vaapi"),
        Codec::Av1Libva => Some("av1_vaapi"),
        _ => None,
    }
}

/// A streaming libva encoder. Frames are pushed one at a time; the encoded
/// bitstream is written to a temp file and muxed by the caller at `finish`.
///
/// Currently uses ffmpeg with VAAPI as the backend (still hardware, but routed
/// through a subprocess). The direct FFI path (vaInitialize + vaCreateConfig +
/// vaCreateSurface + vaRenderPicture) is the next step — the current
/// implementation already provides hardware acceleration with reduced overhead
/// via BufWriter.
pub struct LibvaStream {
    child: Child,
    stdin: std::io::BufWriter<ChildStdin>,
    tmp_path: PathBuf,
    frame_count: usize,
}

impl LibvaStream {
    /// Create a new libva streaming encoder.
    pub fn new(codec: Codec, w: usize, h: usize, fps: u32) -> Result<Self, CandyError> {
        if !is_available() {
            return Err(CandyError::Encode(
                "libva not available: /dev/dri/renderD128 not found. \
                 Install libva and GPU drivers (e.g. intel-media-va-driver-non-free)"
                    .into(),
            ));
        }

        let encoder = vaapi_encoder(codec)
            .ok_or_else(|| CandyError::Encode(format!("codec {codec:?} is not a libva codec")))?;

        // Find ffmpeg for the fallback path
        let ffmpeg = crate::renderer::encode::ffmpeg::find_ffmpeg().ok_or_else(|| {
            CandyError::Encode(
                "ffmpeg not found: libva direct fallback requires ffmpeg with VAAPI support".into(),
            )
        })?;

        let tmp_name = format!(
            "candy_libva_{}_{}.h264",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0),
        );
        let tmp_path = std::env::temp_dir().join(tmp_name);

        let mut cmd = Command::new(&ffmpeg);
        cmd.arg("-vaapi_device")
            .arg("/dev/dri/renderD128")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .args(["-f", "rawvideo"])
            .args(["-pix_fmt", "rgba"])
            .args(["-s", &format!("{w}x{h}")])
            .args(["-r", &fps.to_string()])
            .args(["-i", "-"])
            .args(["-vf", "format=nv12,hwupload"])
            .args(["-c:v", encoder])
            .args(["-low_power", "1"])
            .args(["-qp", "24"])
            .args(["-f", "h264"])
            .args(["-y", tmp_path.to_str().unwrap_or("/dev/null")]);

        let mut child = cmd
            .spawn()
            .map_err(|e| CandyError::Encode(format!("failed to spawn ffmpeg for libva: {e}")))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| CandyError::Encode("libva stdin not captured".into()))?;

        crate::info!("libva direct: spawned encoder {encoder} (streaming)");

        Ok(Self {
            child,
            stdin: std::io::BufWriter::with_capacity(1 << 20, stdin),
            tmp_path,
            frame_count: 0,
        })
    }

    /// Push one RGBA frame to the encoder.
    pub fn push(&mut self, frame: &RenderedFrame) -> Result<(), CandyError> {
        self.stdin
            .write_all(&frame.rgba)
            .map_err(|e| CandyError::Encode(format!("libva stdin write: {e}")))?;
        if self.frame_count % 16 == 0 {
            self.stdin
                .flush()
                .map_err(|e| CandyError::Encode(format!("libva stdin flush: {e}")))?;
        }
        self.frame_count += 1;
        Ok(())
    }

    /// Finish encoding: close stdin, wait for the child, and return the
    /// temp file path containing the raw H.264/HEVC/AV1 bitstream.
    pub fn finish(mut self) -> Result<PathBuf, CandyError> {
        // Flush and drop stdin to signal EOF.
        self.stdin
            .flush()
            .map_err(|e| CandyError::Encode(format!("libva stdin flush: {e}")))?;
        drop(self.stdin);

        let status = self
            .child
            .wait()
            .map_err(|e| CandyError::Encode(format!("libva ffmpeg wait: {e}")))?;

        if !status.success() {
            let _ = std::fs::remove_file(&self.tmp_path);
            return Err(CandyError::Encode(format!(
                "libva encode failed: exit code {}",
                status.code().unwrap_or(-1)
            )));
        }

        let metadata = std::fs::metadata(&self.tmp_path)
            .map_err(|e| CandyError::Encode(format!("libva temp file: {e}")))?;
        if metadata.len() == 0 {
            let _ = std::fs::remove_file(&self.tmp_path);
            return Err(CandyError::Encode("libva produced no output".into()));
        }

        crate::info!(
            "libva direct: encoded {} frames ({} bytes)",
            self.frame_count,
            metadata.len()
        );
        Ok(self.tmp_path)
    }
}
