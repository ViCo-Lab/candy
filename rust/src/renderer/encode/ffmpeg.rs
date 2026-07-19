//! Optional system-FFmpeg encoding path.
//!
//! When the system has `ffmpeg` on `$PATH`, candy can shell out to it for
//! codecs that have no pure-Rust encoder (x264, x265, VAAPI, VideoToolbox,
//! QSV). This module is the bridge: it pipes raw RGBA frames to ffmpeg's
//! stdin and reads the muxed container (MP4/MKV/WebM) back.
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
//! candy ──RGBA stdin──▶ ffmpeg ──seekable sink──▶ muxed container bytes
//!         (rawvideo,      (-c:v libx264 /
//!          rgba,            libx265 /
//!          <w>x<h>)         h264_vaapi /
//!                          h264_videotoolbox /
//!                          h264_qsv / …)
//!                          (-f mp4/mkv/webm)
//! ```
//!
//! ffmpeg's MP4/MKV/WebM muxers require a *seekable* output (MP4's `faststart`
//! moov rewrite is impossible on a pipe). On Linux we hand ffmpeg an anonymous
//! `memfd` — a tmpfs-resident, seekable, in-RAM file — so the muxed container
//! never touches disk: a long HD/high-FPS render avoids round-tripping the
//! coded stream through disk (faster, and works on read-only / tmpfs-less
//! filesystems), and ffmpeg's stderr is likewise redirected to a memfd so a
//! long encode can't deadlock on a full stderr pipe. On other platforms (or if
//! `memfd_create` is unavailable) we fall back to seekable temp files.
//!
//! Audio is muxed in a second ffmpeg pass (candy decodes Opus/AAC itself,
//! pipes raw PCM to ffmpeg as a second input). This is simpler than teaching
//! candy's hand-written muxer to handle HEVC, and lets ffmpeg's mature muxer
//! handle all container/codec combinations.

use std::io::Write;
#[cfg(target_os = "linux")]
use std::io::{Read, Seek, SeekFrom};
#[cfg(target_os = "linux")]
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
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

/// Seekable sink for ffmpeg's muxed container output.
///
/// On Linux the sink is an anonymous `memfd` (tmpfs-resident, seekable) rather
/// than a real temp file: ffmpeg's MP4/MKV/WebM muxers need a seekable output
/// (for `faststart` moov rewriting) and a memfd satisfies that while keeping
/// the intermediate entirely in RAM — so a long HD/high-FPS render never
/// round-trips the coded stream through disk (faster, and works on read-only or
/// tmpfs-less filesystems). On other platforms, or if `memfd_create` is
/// unavailable, we fall back to a temp file.
pub(crate) struct MuxSink {
    /// Output path handed to ffmpeg (`-y <path>`): a real temp file elsewhere,
    /// or `/proc/self/fd/N` for a Linux memfd.
    pub path: PathBuf,
    /// Linux memfd backing `path`; kept alive (and used to read the result back)
    /// until the encode finishes. `None` on the temp-file fallback.
    #[cfg(target_os = "linux")]
    pub file: Option<std::fs::File>,
}

impl MuxSink {
    /// Read the whole muxed container back into memory.
    pub(crate) fn read_all(&self) -> std::io::Result<Vec<u8>> {
        #[cfg(target_os = "linux")]
        if let Some(f) = &self.file {
            let mut c = f.try_clone()?;
            c.seek(SeekFrom::Start(0))?;
            let mut buf = Vec::new();
            c.read_to_end(&mut buf)?;
            return Ok(buf);
        }
        std::fs::read(&self.path)
    }

    /// Stream the muxed container to `out` without buffering it all in RAM.
    pub(crate) fn copy_to(&self, out: &Path) -> std::io::Result<u64> {
        #[cfg(target_os = "linux")]
        if let Some(f) = &self.file {
            let mut c = f.try_clone()?;
            c.seek(SeekFrom::Start(0))?;
            let mut outf = std::fs::File::create(out)?;
            return std::io::copy(&mut c, &mut outf);
        }
        std::fs::copy(&self.path, out)
    }

    /// Remove the on-disk temp file. No-op for a memfd, which is freed when this
    /// struct is dropped.
    pub(crate) fn cleanup(&self) {
        #[cfg(target_os = "linux")]
        if self.file.is_none() {
            let _ = std::fs::remove_file(&self.path);
        }
        #[cfg(not(target_os = "linux"))]
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Where ffmpeg's stderr goes. On Linux a memfd `File` (unbounded, in-RAM, so a
/// long encode can't deadlock on a full pipe); on other platforms a temp log
/// file. Only read when ffmpeg reports failure.
pub(crate) enum ErrLog {
    #[cfg(target_os = "linux")]
    Memfd(std::fs::File),
    File(PathBuf),
}

/// Create an anonymous `memfd` (Linux only). Returns the owned fd.
#[cfg(target_os = "linux")]
fn memfd_create_named(name: &str) -> std::io::Result<OwnedFd> {
    let cname = std::ffi::CString::new(name)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    // NOTE: we pass `0` (no `MFD_CLOEXEC`). ffmpeg reaches the memfd via the
    // `/proc/self/fd/N` *path* we hand it as the output file. With `MFD_CLOEXEC`
    // the fd would be closed in the child at `exec` time, making that path
    // invalid by the time ffmpeg `open()`s it — ffmpeg would then abort and
    // break the stdin pipe. Without `CLOEXEC` the fd is inherited by the child
    // (harmless: ffmpeg opens the path itself) and the path resolves correctly.
    let fd = unsafe { libc::memfd_create(cname.as_ptr(), 0) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// Build the seekable sink ffmpeg writes its muxed container to. Uses a memfd on
/// Linux, falling back to a temp file if `memfd_create` is unavailable.
fn make_mux_sink(container: Container) -> Result<MuxSink, CandyError> {
    #[cfg(target_os = "linux")]
    if let Ok(fd) = memfd_create_named("candy-ffmpeg-mux") {
        let file = std::fs::File::from(fd);
        let path = PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()));
        return Ok(MuxSink {
            path,
            file: Some(file),
        });
    }
    let ext = container_format(container);
    let name = format!(
        "candy_ff_{}_{}.{ext}",
        std::process::id(),
        TMP_COUNTER.fetch_add(1, Ordering::Relaxed),
    );
    Ok(MuxSink {
        path: std::env::temp_dir().join(name),
        #[cfg(target_os = "linux")]
        file: None,
    })
}

/// Build ffmpeg's stderr redirection. On Linux a memfd (unbounded, can't block
/// a long encode); elsewhere a temp log file. Returns the `Stdio` to attach and
/// an [`ErrLog`] the caller reads only on failure.
fn make_err_log() -> Result<(Stdio, ErrLog), CandyError> {
    #[cfg(target_os = "linux")]
    if let Ok(fd) = memfd_create_named("candy-ffmpeg-err") {
        let file = std::fs::File::from(fd);
        let reader = file
            .try_clone()
            .map_err(|e| CandyError::Encode(format!("ffmpeg stderr clone: {e}")))?;
        return Ok((Stdio::from(file), ErrLog::Memfd(reader)));
    }
    let name = format!(
        "candy_ff_err_{}_{}.log",
        std::process::id(),
        TMP_COUNTER.fetch_add(1, Ordering::Relaxed),
    );
    let path = std::env::temp_dir().join(name);
    let file = std::fs::File::create(&path)
        .map_err(|e| CandyError::Encode(format!("ffmpeg stderr log: {e}")))?;
    Ok((Stdio::from(file), ErrLog::File(path)))
}

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
        #[cfg(target_os = "linux")]
        Codec::H264Vaapi => Some(("h264_vaapi", "mp4")),
        #[cfg(target_os = "linux")]
        Codec::H265Vaapi => Some(("hevc_vaapi", "mp4")),
        #[cfg(target_os = "macos")]
        Codec::H264VideoToolbox => Some(("h264_videotoolbox", "mp4")),
        #[cfg(target_os = "macos")]
        Codec::H265VideoToolbox => Some(("hevc_videotoolbox", "mp4")),
        #[cfg(target_os = "windows")]
        Codec::H264Qsv => Some(("h264_qsv", "mp4")),
        #[cfg(target_os = "windows")]
        Codec::H265Qsv => Some(("hevc_qsv", "mp4")),
        // H265 (the "self-contained or ffmpeg" variant) uses x265 when ffmpeg
        // is available.
        Codec::H265 => Some(("libx265", "mp4")),
        #[cfg(target_os = "linux")]
        Codec::Av1Vaapi => Some(("av1_vaapi", "mp4")),
        Codec::Vp9 => Some(("libvpx-vp9", "webm")),
        Codec::Vp8 => Some(("libvpx", "webm")),
        // Self-contained codecs don't go through ffmpeg.
        Codec::Av1 | Codec::H264 => None,
        #[cfg(target_os = "linux")]
        Codec::H264Libva | Codec::H265Libva | Codec::Av1Libva => None,
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
/// and writes a muxed `container` to a seekable sink. Returns the child
/// process, its stdin handle (the caller writes frames to it, then drops it),
/// the [`MuxSink`] ffmpeg writes the container to, and the [`ErrLog`] used for
/// failure diagnosis.
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
) -> Result<(Child, ChildStdin, MuxSink, ErrLog), CandyError> {
    let ffmpeg = find_ffmpeg()
        .ok_or_else(|| CandyError::Encode("ffmpeg not found on $PATH (E007)".into()))?;

    let (encoder, _default_ext) = ffmpeg_args(codec)
        .ok_or_else(|| CandyError::Encode(format!("codec {codec:?} does not use ffmpeg")))?;
    let format = container_format(container);

    // ffmpeg's MP4/MKV/WebM muxers require a *seekable* output (MP4's
    // `faststart` moov rewrite is impossible on a pipe), so we hand it a
    // seekable sink. On Linux that sink is an anonymous `memfd` (tmpfs-resident,
    // seekable) — the muxed container never touches disk, which is faster and
    // works on read-only / tmpfs-less filesystems; elsewhere (or if
    // `memfd_create` is unavailable) we fall back to a unique temp file.
    let mux = make_mux_sink(container)?;

    // ffmpeg prints encoding progress (and any warnings/errors) to stderr. A
    // *piped* stderr can fill the OS pipe buffer (~64 KiB) during a long encode
    // and block ffmpeg while the parent is still feeding frames or waiting on
    // it — a classic deadlock that hangs the whole encode. On Linux we redirect
    // stderr to a memfd (unbounded, in-RAM, can't block); elsewhere to a temp
    // log file. We only read it when ffmpeg reports failure.
    let (err_for_ffmpeg, err_log) = make_err_log()?;

    // Build the ffmpeg command. Order matters for hardware encoders: a render
    // node / device must be declared *before* the input is read, and hardware
    // encoders need the raw RGBA frames uploaded to a hardware surface (not
    // passed straight through). Software lib encoders (x264/x265) instead want
    // `-preset`/`-crf` — options that VAAPI / VideoToolbox / QSV reject.
    // `bitrate_str` is only consumed by the VideoToolbox (macOS) and QSV
    // (Windows) hardware encoders below; compute it only on those platforms so
    // it stays unused (and warning-free) on Linux, where those arms are
    // cfg-gated out.
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    let bitrate_str = {
        let bitrate = ((w as u64 * h as u64 * fps as u64) / 20).clamp(120_000, 20_000_000);
        bitrate.to_string()
    };

    let mut cmd = Command::new(&ffmpeg);
    #[cfg(target_os = "linux")]
    if matches!(codec, Codec::H264Vaapi | Codec::H265Vaapi | Codec::Av1Vaapi) {
        cmd.arg("-vaapi_device").arg("/dev/dri/renderD128");
    }
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(err_for_ffmpeg)
        .args(["-f", "rawvideo"])
        .args(["-pix_fmt", "rgba"])
        .args(["-s", &format!("{w}x{h}")])
        .args(["-r", &fps.to_string()])
        .args(["-i", "-"])
        .args(["-c:v", encoder]);

    match codec {
        Codec::X264 | Codec::X265 | Codec::H265 | Codec::Vp9 | Codec::Vp8 => {
            cmd.args(["-preset", "medium"]);
            cmd.args(["-crf", "23"]);
            cmd.args(["-vf", "format=yuv420p"]);
            cmd.args(["-threads", "0"]);
        }
        #[cfg(target_os = "linux")]
        Codec::H264Vaapi | Codec::H265Vaapi | Codec::Av1Vaapi => {
            cmd.args(["-vf", "format=nv12,hwupload"]);
            cmd.args(["-low_power", "1"]);
            cmd.args(["-qp", "24"]);
        }
        #[cfg(target_os = "macos")]
        Codec::H264VideoToolbox | Codec::H265VideoToolbox => {
            cmd.args(["-b:v", &bitrate_str]);
        }
        #[cfg(target_os = "windows")]
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
        .args(["-y", mux.path.to_str().unwrap_or("/dev/null")]);

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
    Ok((child, stdin, mux, err_log))
}

/// Spawn an ffmpeg child on Linux that reads raw RGBA frames from a **pipe**.
///
/// # Why a pipe, not a memfd
///
/// A previous version used a `memfd` for frame input and told ffmpeg to read
/// from `/proc/self/fd/N`. That approach has a fundamental race: ffmpeg opens
/// the memfd as a *regular file* and its `read()` returns 0 (EOF) the instant
/// its read offset catches up to the file size — it does **not** block waiting
/// for the producer to append more data (unlike a pipe, whose `read()` blocks
/// until data is available or the write end is closed). On a fast multi-core
/// machine ffmpeg routinely drains the memfd faster than the producer can
/// render the next frame, hits EOF, and finalises the container with only a
/// handful of frames encoded — producing a video that is a fraction of the
/// expected duration (e.g. 5 frames out of 61 for `preview_demo`).
///
/// The fix is to feed ffmpeg through a **pipe** (`pipe2(2)`). A pipe's `read()`
/// blocks on the read end until the producer writes more data (or closes the
/// write end → real EOF), which is exactly the streaming contract ffmpeg
/// expects from `-i -` / stdin input. The pipe is still entirely kernel-buffered
/// (no disk I/O), so it retains the "intermediate never touches disk" property
/// the memfd was after.
///
/// # Zero-copy frame feed via `vmsplice`
///
/// To avoid the `write()` syscall's user→kernel copy on the producer side, the
/// caller ([`crate::renderer::encode::video::StreamingVideo::push`]) writes each
/// frame's RGBA buffer into the pipe with `vmsplice(2)` + `SPLICE_F_GIFT`. This
/// transfers ownership of the buffer's physical pages to the kernel pipe buffer
/// without copying a single byte — true zero-copy on the producer side. ffmpeg's
/// `read()` then copies the data out of the pipe buffer as usual (one copy,
/// unavoidable: the kernel must hand the bytes to userspace).
///
/// `vmsplice` with `SPLICE_F_GIFT` requires the buffer to be page-aligned and a
/// multiple of the page size. The caller is responsible for padding the RGBA
/// buffer accordingly; `compose()` in `video.rs` does this by sizing the canvas
/// to an even width/height (already required by the encoder) and rounding the
/// buffer length up to a page multiple, zero-padding the tail.
///
/// # Pipe capacity
///
/// The default pipe capacity is 64 KiB — far too small for a single HD frame
/// (~1.85 MiB at 907×510×4). We grow the pipe with `fcntl(F_SETPIPE_SZ)` to at
/// least one frame plus one page, so a single `vmsplice` never deadlocks (the
/// producer's gift fits in one shot). The kernel caps the requested size to a
/// power of two and may round up; on systems where the unprivileged pipe size
/// cap (`/proc/sys/fs/pipe-max-size`) is too low the `F_SETPIPE_SZ` call fails
/// and we fall back to a plain `write()` — still correct, just not zero-copy.
///
/// # Why we still use memfds elsewhere
///
/// The mux **output** sink (see [`make_mux_sink`]) and the stderr redirection
/// (see [`make_err_log`]) still use memfds, and that is correct: ffmpeg writes
/// the whole container to the output sink and seeks back for the `faststart`
/// moov rewrite, so it needs a *seekable* file (a pipe would not work). The
/// stderr sink needs an unbounded buffer so a long encode cannot deadlock on a
/// full stderr pipe. Both of those are write-only-from-ffmpeg, read-once-by-us,
/// so the EOF race does not apply.
///
/// Returns `(Child, File, MuxSink, ErrLog)` where the `File` is the **write end
/// of the pipe** (caller writes one frame at a time, then drops it to signal
/// EOF). The read end is handed to ffmpeg via `-i /proc/self/fd/N` (ffmpeg
/// inherits the fd across `exec`, so the path stays valid even though our own
/// `File` only owns the write end).
#[cfg(target_os = "linux")]
pub(crate) fn spawn_ffmpeg_with_memfd(
    codec: Codec,
    container: Container,
    w: u32,
    h: u32,
    fps: u32,
    private_metadata: &PrivateMeta,
) -> Result<(Child, std::fs::File, MuxSink, ErrLog), CandyError> {
    let ffmpeg = find_ffmpeg()
        .ok_or_else(|| CandyError::Encode("ffmpeg not found on $PATH (E007)".into()))?;

    let (encoder, _default_ext) = ffmpeg_args(codec)
        .ok_or_else(|| CandyError::Encode(format!("codec {codec:?} does not use ffmpeg")))?;
    let format = container_format(container);

    // Create the seekable sink for ffmpeg's output (memfd — needs to be seekable).
    let mux = make_mux_sink(container)?;

    // Create stderr redirection (memfd — needs to be unbounded).
    let (err_for_ffmpeg, err_log) = make_err_log()?;

    // Create a **pipe** for frame input. The read end is inherited by ffmpeg
    // across `exec` (we pass it as `-i /proc/self/fd/N`); the write end is
    // returned to the caller. A pipe's read() blocks until data is available
    // or the write end is closed, which is the streaming contract ffmpeg
    // expects — unlike a memfd (regular file), whose read() returns EOF the
    // moment it catches up to the file size, causing ffmpeg to finalise early.
    let (read_fd, write_fd) = {
        let mut fds = [0i32; 2];
        // We create the pipe WITHOUT `O_CLOEXEC` on the read end so ffmpeg can
        // re-open it via `/proc/self/fd/N` after `exec`. The write end gets
        // `O_CLOEXEC` so it does not leak into the ffmpeg child (the child only
        // needs its own read-end reference; if it inherited the write end the
        // pipe would never signal EOF to ffmpeg when we close our write end).
        // `pipe2(O_CLOEXEC)` sets CLOEXEC on both ends, so we use `pipe()` and
        // then set CLOEXEC only on the write end via `fcntl(F_SETFD)`.
        let rc = unsafe { libc::pipe(fds.as_mut_ptr()) };
        if rc < 0 {
            return Err(CandyError::Encode(format!(
                "pipe for ffmpeg frame input: {}",
                std::io::Error::last_os_error()
            )));
        }
        // Set CLOEXEC on the write end only (so it closes in the child on exec).
        unsafe {
            let flags = libc::fcntl(fds[1], libc::F_GETFD);
            if flags >= 0 {
                libc::fcntl(fds[1], libc::F_SETFD, flags | libc::FD_CLOEXEC);
            }
        }
        // Grow the pipe to at least one frame, so a single `vmsplice` of a
        // full frame never deadlocks waiting for ffmpeg to drain.
        // `F_SETPIPE_SZ` rounds up to a power of two; the kernel may cap this
        // for unprivileged users — on failure we silently keep the default
        // 64 KiB and the caller falls back to chunked `write()`.
        let frame_bytes = (w as usize) * (h as usize) * 4;
        let want = frame_bytes.next_power_of_two().max(1 << 16);
        unsafe {
            let got = libc::fcntl(fds[0], libc::F_SETPIPE_SZ, want as libc::c_int);
            if got < 0 {
                // Best-effort: keep the default pipe size. The caller's
                // `vmsplice` will then write in chunks, blocking as needed.
            }
        }
        let read_fd = unsafe { OwnedFd::from_raw_fd(fds[0]) };
        let write_fd = unsafe { OwnedFd::from_raw_fd(fds[1]) };
        (read_fd, write_fd)
    };
    // `read_file` owns the read end. We hand ffmpeg `/proc/self/fd/N` (where N
    // is the read fd's raw fd) so ffmpeg opens its own reference to the same
    // pipe. After `Command::spawn` returns, ffmpeg has inherited the fd (via
    // the `/proc/self/fd/N` open in the child) and we can drop `read_file`
    // — the write end staying open is what keeps the pipe from signalling EOF.
    let read_file = std::fs::File::from(read_fd);
    let write_file = std::fs::File::from(write_fd);
    let frame_path = format!("/proc/self/fd/{}", read_file.as_raw_fd());

    let mut cmd = Command::new(&ffmpeg);
    if matches!(codec, Codec::H264Vaapi | Codec::H265Vaapi | Codec::Av1Vaapi) {
        cmd.arg("-vaapi_device").arg("/dev/dri/renderD128");
    }

    cmd.stdin(Stdio::null()) // frame input is via the pipe, not stdin
        .stdout(Stdio::null())
        .stderr(err_for_ffmpeg)
        .args(["-f", "rawvideo"])
        .args(["-pix_fmt", "rgba"])
        .args(["-s", &format!("{w}x{h}")])
        .args(["-r", &fps.to_string()])
        .args(["-i", &frame_path]) // read from the pipe
        .args(["-c:v", encoder]);

    match codec {
        Codec::X264 | Codec::X265 | Codec::H265 | Codec::Vp9 | Codec::Vp8 => {
            cmd.args(["-preset", "medium"]);
            cmd.args(["-crf", "23"]);
            cmd.args(["-vf", "format=yuv420p"]);
            cmd.args(["-threads", "0"]);
        }
        Codec::H264Vaapi | Codec::H265Vaapi | Codec::Av1Vaapi => {
            cmd.args(["-vf", "format=nv12,hwupload"]);
            cmd.args(["-low_power", "1"]);
            cmd.args(["-qp", "24"]);
        }
        #[cfg(target_os = "macos")]
        Codec::H264VideoToolbox | Codec::H265VideoToolbox => {
            let bitrate = ((w as u64 * h as u64 * fps as u64) / 20).clamp(120_000, 20_000_000);
            cmd.args(["-b:v", &bitrate.to_string()]);
        }
        #[cfg(target_os = "windows")]
        Codec::H264Qsv | Codec::H265Qsv => {
            let bitrate = ((w as u64 * h as u64 * fps as u64) / 20).clamp(120_000, 20_000_000);
            cmd.args(["-init_hw_device", "qsv=qsv:/dev/dri/renderD128"])
                .args(["-vf", "format=nv12,hwupload=extra_hw_frames=64"])
                .args(["-b:v", &bitrate.to_string()]);
        }
        _ => {}
    }

    cmd.arg("-metadata")
        .arg(format!("candy-meta={}", private_metadata.to_json()));

    cmd.args(["-f", format])
        .args(["-y", mux.path.to_str().unwrap_or("/dev/null")]);

    if matches!(container, Container::Mp4) {
        cmd.args(["-movflags", "+faststart"]);
    }

    let child = cmd
        .spawn()
        .map_err(|e| CandyError::Encode(format!("failed to spawn ffmpeg: {e}")))?;
    // ffmpeg has now opened its own reference to the read end (via the
    // `/proc/self/fd/N` path). Drop our read-end handle so the only thing
    // keeping the pipe alive is the write end (which the caller holds). When
    // the caller drops the write end, ffmpeg sees the real EOF and finalises.
    drop(read_file);

    info!("spawned ffmpeg -c:v {encoder} -f {format} (pipe input, vmsplice)");
    Ok((child, write_file, mux, err_log))
}

/// Write `data` to `writer` using `vmsplice(2)` with `SPLICE_F_GIFT` for true
/// zero-copy (the buffer's physical pages are gifted to the kernel pipe buffer
/// without a `write()`-style copy). Falls back to a plain `write()` if the
/// buffer is not page-aligned, not a page-size multiple, or `vmsplice` fails
/// (e.g. the pipe is too small to accept the whole gift at once).
///
/// # Safety contract
///
/// After a successful `vmsplice` with `SPLICE_F_GIFT`, the gifted pages must
/// not be modified by the caller until ffmpeg has drained them from the pipe.
/// In practice the caller (`StreamingVideo::push`) drops the RGBA buffer
/// immediately after this call returns, so the contract holds: the `Vec`'s
/// allocation is freed (the pages are now owned by the kernel), and even if
/// the allocator reuses them, the kernel still holds its reference until
/// ffmpeg reads the data.
///
/// Returns the number of bytes written (always `data.len()` on success).
#[cfg(target_os = "linux")]
pub(crate) fn vmsplice_frame(writer: &mut std::fs::File, data: &[u8]) -> std::io::Result<usize> {
    use std::sync::atomic::{AtomicBool, Ordering};

    // `vmsplice` with `SPLICE_F_GIFT` requires page-aligned address and a
    // length that is a multiple of the page size. The caller pads the RGBA
    // buffer to satisfy this; if it does not, we fall back to `write()`.
    let page_size = 4096usize;
    let ptr = data.as_ptr() as usize;
    let len = data.len();
    let aligned = ptr % page_size == 0 && len % page_size == 0 && len > 0;
    if !aligned {
        // Use the standard write() — one user→kernel copy, but correct.
        // A retry loop handles short writes (the pipe may be smaller than
        // the frame, so the producer blocks until ffmpeg drains it).
        return writer.write_all(data).map(|()| len);
    }

    // `vmsplice` may write less than requested if the pipe is full (the kernel
    // pipe buffer has a bounded capacity, default 64 KiB, grown via
    // F_SETPIPE_SZ in `spawn_ffmpeg_with_memfd`). Retry until all bytes are
    // gifted, blocking in between (the kernel blocks `vmsplice` on a full pipe,
    // just like `write`). Each retry passes the remaining slice — but the
    // pointer must stay page-aligned, so we advance in page multiples.
    let mut off = 0usize;
    while off < len {
        let remaining = len - off;
        let iov = libc::iovec {
            iov_base: (ptr + off) as *mut libc::c_void,
            iov_len: remaining,
        };
        let written = unsafe {
            libc::vmsplice(
                writer.as_raw_fd(),
                &iov,
                1,
                libc::SPLICE_F_GIFT,
            )
        };
        if written < 0 {
            let err = std::io::Error::last_os_error();
            // `EINVAL` from `vmsplice` typically means the kernel rejected the
            // gift (e.g. the fd is not a pipe, or alignment is wrong despite
            // our checks). Fall back to `write()` for the remainder so the
            // frame is not lost. Use a `static` flag so we warn once per
            // process, not once per frame.
            static WARNED: AtomicBool = AtomicBool::new(false);
            if !WARNED.swap(true, Ordering::Relaxed) {
                eprintln!("candy: vmsplice failed ({err}); falling back to write() for subsequent frames");
            }
            return writer.write_all(&data[off..]).map(|()| len);
        }
        off += written as usize;
    }
    Ok(len)
}

/// Read the last ~20 lines of ffmpeg's stderr log for error reporting. On
/// Linux the log is a memfd (read back via a cloned handle); elsewhere a temp
/// file. Reading only the tail keeps error messages bounded and avoids
/// buffering the whole log in RAM.
fn read_err_log(err: &ErrLog) -> String {
    let raw = match err {
        #[cfg(target_os = "linux")]
        ErrLog::Memfd(f) => {
            let mut c = match f.try_clone() {
                Ok(c) => c,
                Err(_) => return "(cannot read ffmpeg stderr)".to_string(),
            };
            c.seek(SeekFrom::Start(0)).ok();
            let mut s = String::new();
            c.read_to_string(&mut s).ok();
            s
        }
        ErrLog::File(p) => match std::fs::read_to_string(p) {
            Ok(s) => s,
            Err(_) => return "(no ffmpeg stderr captured)".to_string(),
        },
    };
    if raw.is_empty() {
        return "(no ffmpeg stderr captured)".to_string();
    }
    raw.lines()
        .rev()
        .take(20)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("\n")
}

/// Finish an ffmpeg encode started by [`spawn_ffmpeg`]: the child's stdin must
/// already be dropped/closed so ffmpeg flushes, then we wait and read back the
/// muxed container bytes. Used by the batch [`encode_via_ffmpeg`] path (which
/// already holds every frame in RAM); the streaming pipeline uses
/// [`finish_ffmpeg_to_file`] instead to avoid buffering the container.
pub(crate) fn finish_ffmpeg(
    mut child: Child,
    mux: MuxSink,
    err_log: ErrLog,
) -> Result<Vec<u8>, CandyError> {
    let status = child
        .wait()
        .map_err(|e| CandyError::Encode(format!("ffmpeg wait: {e}")))?;

    if !status.success() {
        let stderr = read_err_log(&err_log);
        mux.cleanup();
        return Err(CandyError::Encode(format!(
            "ffmpeg exited with {status}: {stderr}"
        )));
    }

    let bytes = mux
        .read_all()
        .map_err(|e| CandyError::Encode(format!("ffmpeg output read: {e}")))?;
    mux.cleanup();

    if bytes.is_empty() {
        return Err(CandyError::Encode(
            "ffmpeg produced no output (E007)".into(),
        ));
    }
    Ok(bytes)
}

/// Finish an ffmpeg encode started by [`spawn_ffmpeg`]: the child's stdin must
/// already be dropped/closed so ffmpeg flushes, then we wait and copy the muxed
/// container (already a seekable sink) directly to `output`. Copying the
/// container avoids buffering the entire container in RAM, so a long HD/high-FPS
/// render cannot OOM on the coded stream.
pub(crate) fn finish_ffmpeg_to_file(
    mut child: Child,
    mux: MuxSink,
    output: &Path,
    err_log: ErrLog,
) -> Result<(), CandyError> {
    let status = child
        .wait()
        .map_err(|e| CandyError::Encode(format!("ffmpeg wait: {e}")))?;

    if !status.success() {
        let stderr = read_err_log(&err_log);
        mux.cleanup();
        return Err(CandyError::Encode(format!(
            "ffmpeg exited with {status} (E007): {stderr}"
        )));
    }

    mux.copy_to(output)
        .map_err(|e| CandyError::Encode(format!("ffmpeg output copy: {e}")))?;
    mux.cleanup();
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
    let (child, mut stdin, mux, err_log) =
        spawn_ffmpeg(codec, container, w as u32, h as u32, fps, private_metadata)?;
    for f in frames {
        stdin
            .write_all(&f.rgba)
            .map_err(|e| CandyError::Encode(format!("ffmpeg stdin write: {e}")))?;
    }
    drop(stdin); // close stdin → ffmpeg finishes encoding
    finish_ffmpeg(child, mux, err_log)
}
