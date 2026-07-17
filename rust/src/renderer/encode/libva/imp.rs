//! Real direct-VAAPI encoder implementation (feature `libva`).
//!
//! See `libva.rs` for the module overview. This file is only compiled when the
//! `libva` feature is enabled; it `include!`s the bindgen-generated FFI and
//! drives the GPU encoder in-process, then hands the coded samples to candy's
//! self-contained container muxer.

#![allow(clippy::missing_safety_doc)]

use std::fs::File;
use std::io::Write;
use std::os::raw::{c_int, c_uint, c_void};
use std::os::unix::io::AsRawFd;
use std::path::PathBuf;

use crate::core::diag::CandyError;
use crate::core::meta::PrivateMeta;
use crate::renderer::RenderedFrame;
use crate::renderer::audio::AudioData;
use crate::renderer::encode::container;
use crate::renderer::encode::video::{EncodedVideoFile, new_samples_tempfile};
use crate::renderer::encode::{Codec, Container};

// Bindgen-generated raw libva FFI (built by `build.rs` under the `libva` feature).
include!(concat!(env!("OUT_DIR"), "/libva_bindings.rs"));

// --- Local aliases for the VA enums/constants we need -----------------------
// (kept here so the code reads clearly and does not depend on bindgen's
// double-prefixed constant names).
const VA_PROFILE_H264_HIGH: VAProfile = VAProfile_VAProfileH264High;
const VA_PROFILE_HEVC_MAIN: VAProfile = VAProfile_VAProfileHEVCMain;
const VA_PROFILE_AV1_0: VAProfile = VAProfile_VAProfileAV1Profile0;
const VA_ENTRYPOINT_ENC_SLICE: VAEntrypoint = VAEntrypoint_VAEntrypointEncSlice;
// `VAEncTileGroupBufferType` is missing from the public `VABufferType` enum in
// some libva versions even though `VAEncTileGroupBufferAV1` exists. Upstream
// libva defines it as 31; we mirror that so the AV1 path can create the tile
// group buffer. (Verified untested on real hardware in this environment.)
const VA_ENC_TILE_GROUP_BUFFER_TYPE: VABufferType = 31;

pub struct LibvaStream {
    dpy: VADisplay,
    config: VAConfigID,
    context: VAContextID,
    surface: VASurfaceID,
    seq_buf: VABufferID,
    rc_buf: VABufferID,
    fr_buf: VABufferID,
    seq_rendered: bool,
    w: u32,
    h: u32,
    fps: u32,
    is_av1: bool,
    is_hevc: bool,
    container: Container,
    meta: PrivateMeta,
    samples: File,
    samples_path: PathBuf,
    sample_sizes: Vec<u32>,
    keyframes: Vec<bool>,
    codec_private: Vec<u8>,
    private_ready: bool,
    frame_count: usize,
}

impl LibvaStream {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        codec: Codec,
        w: usize,
        h: usize,
        fps: u32,
        container: Container,
        meta: &PrivateMeta,
    ) -> Result<Self, CandyError> {
        if !super::is_available() {
            return Err(CandyError::Libva(
                "libva not available: /dev/dri/renderD128 not found. Install libva + a \
                 VAAPI-capable GPU driver (e.g. intel-media-va-driver-non-free)."
                    .into(),
            ));
        }
        let profile: VAProfile = match codec {
            Codec::H264Libva => VA_PROFILE_H264_HIGH,
            Codec::H265Libva => VA_PROFILE_HEVC_MAIN,
            Codec::Av1Libva => VA_PROFILE_AV1_0,
            _ => {
                return Err(CandyError::Libva(format!(
                    "codec {codec:?} is not a direct-libva codec"
                )))
            }
        };

        // Open the DRM render node and the VAAPI display.
        let drm = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/dri/renderD128")
            .map_err(|e| CandyError::Libva(format!("open /dev/dri/renderD128: {e}")))?;
        let fd = drm.as_raw_fd();
        // Keep `drm` alive for the lifetime of the encoder.
        let _drm = drm;
        let dpy = unsafe { vaGetDisplayDRM(fd) };
        if dpy.is_null() {
            return Err(CandyError::Libva("vaGetDisplayDRM returned null".into()));
        }
        let mut major: i32 = 0;
        let mut minor: i32 = 0;
        va_ok(unsafe { vaInitialize(dpy, &mut major, &mut minor) }, "vaInitialize")?;

        // Confirm the encode entrypoint exists for this profile.
        let mut entrypoints: [VAEntrypoint; 32] = [0; 32];
        let mut num_ep: i32 = 32;
        va_ok(
            unsafe {
                vaQueryConfigEntrypoints(
                    dpy,
                    profile,
                    entrypoints.as_mut_ptr(),
                    &mut num_ep,
                )
            },
            "vaQueryConfigEntrypoints",
        )?;
        let supports_encode = (0..num_ep as usize)
            .any(|i| entrypoints[i] == VA_ENTRYPOINT_ENC_SLICE);
        if !supports_encode {
            unsafe { vaTerminate(dpy) };
            return Err(CandyError::Libva(format!(
                "VAAPI profile {profile:?} does not support the encode (EncSlice) entrypoint \
                 on this hardware"
            )));
        }

        // Configure RT format (YUV420) and rate control (CQP — constant QP).
        let attribs = [
            VAConfigAttrib {
                type_: VAConfigAttribType_VAConfigAttribRTFormat,
                value: VA_RT_FORMAT_YUV420,
            },
            VAConfigAttrib {
                type_: VAConfigAttribType_VAConfigAttribRateControl,
                value: VA_RC_CQP,
            },
        ];
        let mut config: VAConfigID = VA_INVALID_ID;
        va_ok(
            unsafe {
                vaCreateConfig(
                    dpy,
                    profile,
                    VA_ENTRYPOINT_ENC_SLICE,
                    attribs.as_ptr() as *mut VAConfigAttrib,
                    attribs.len() as i32,
                    &mut config,
                )
            },
            "vaCreateConfig",
        )?;

        // One surface (reused per frame, sync'd before reuse).
        let mut surface: VASurfaceID = VA_INVALID_SURFACE;
        va_ok(
            unsafe {
                vaCreateSurfaces(
                    dpy,
                    VA_RT_FORMAT_YUV420,
                    w as u32,
                    h as u32,
                    &mut surface,
                    1,
                    std::ptr::null_mut(),
                    0,
                )
            },
            "vaCreateSurfaces",
        )?;

        let mut context: VAContextID = VA_INVALID_ID;
        va_ok(
            unsafe {
                vaCreateContext(
                    dpy,
                    config,
                    w as u32,
                    h as u32,
                    0,
                    &mut surface,
                    1,
                    &mut context,
                )
            },
            "vaCreateContext",
        )?;

        // Sequence + misc (rate-control / frame-rate) parameter buffers, created
        // once and rendered on the first picture.
        let seq_buf = Self::create_seq_buffer(dpy, context, codec, w as u32, h as u32, fps)?;
        let rc_buf = Self::create_rc_buffer(dpy, context)?;
        let fr_buf = Self::create_framerate_buffer(dpy, context, fps)?;

        let (samples, samples_path) = new_samples_tempfile()
            .map_err(|e| CandyError::Libva(format!("sample temp file: {e}")))?;

        crate::info!(
            "libva: direct VAAPI encode ({codec:?}) {w}x{h} @ {fps}fps (no ffmpeg)"
        );

        Ok(Self {
            dpy,
            config,
            context,
            surface,
            seq_buf,
            rc_buf,
            fr_buf,
            seq_rendered: false,
            w: w as u32,
            h: h as u32,
            fps,
            is_av1,
            is_hevc,
            container,
            meta: meta.clone(),
            samples,
            samples_path,
            sample_sizes: Vec::new(),
            keyframes: Vec::new(),
            codec_private: Vec::new(),
            private_ready: false,
            frame_count: 0,
        })
    }

    /// Create the codec-specific sequence-parameter buffer (zeroed + essential
    /// fields). The buffer holds the data; VAAPI copies it on render.
    fn create_seq_buffer(
        dpy: VADisplay,
        context: VAContextID,
        codec: Codec,
        w: u32,
        h: u32,
        fps: u32,
    ) -> Result<VABufferID, CandyError> {
        let (buf, size) = match codec {
            Codec::H264Libva => {
                let mut s: VAEncSequenceParameterBufferH264 = unsafe { std::mem::zeroed() };
                s.seq_parameter_set_id = 0;
                s.level_idc = 40;
                // All-intra: every picture is an IDR.
                s.intra_period = 1;
                s.intra_idr_period = 1;
                s.ip_period = 1;
                s.bits_per_second = 0;
                s.max_num_ref_frames = 1;
                s.picture_width_in_mbs = (w / 16) as u16;
                s.picture_height_in_mbs = (h / 16) as u16;
                s.seq_fields.set_chroma_format_idc(1);
                s.seq_fields.set_frame_mbs_only_flag(1);
                s.bit_depth_luma_minus8 = 0;
                s.bit_depth_chroma_minus8 = 0;
                (
                    &s as *const _ as *mut c_void,
                    std::mem::size_of::<VAEncSequenceParameterBufferH264>() as u32,
                )
            }
            Codec::H265Libva => {
                let mut s: VAEncSequenceParameterBufferHEVC = unsafe { std::mem::zeroed() };
                s.general_profile_idc = 1; // Main
                s.general_level_idc = 93; // 3.1
                s.general_tier_flag = 0;
                s.intra_period = 1;
                s.intra_idr_period = 1;
                s.ip_period = 1;
                s.bits_per_second = 0;
                s.pic_width_in_luma_samples = w as u16;
                s.pic_height_in_luma_samples = h as u16;
                s.seq_fields.set_chroma_format_idc(1);
                s.log2_min_luma_coding_block_size_minus3 = 0;
                s.log2_diff_max_min_luma_coding_block_size = 1;
                (
                    &s as *const _ as *mut c_void,
                    std::mem::size_of::<VAEncSequenceParameterBufferHEVC>() as u32,
                )
            }
            Codec::Av1Libva => {
                let mut s: VAEncSequenceParameterBufferAV1 = unsafe { std::mem::zeroed() };
                s.seq_profile = 0;
                s.seq_level_idx = 4; // 3.1
                s.seq_tier = 0;
                s.intra_period = 1;
                s.ip_period = 1;
                s.bits_per_second = 0;
                s.seq_fields.set_use_128x128_superblock(1);
                (
                    &s as *const _ as *mut c_void,
                    std::mem::size_of::<VAEncSequenceParameterBufferAV1>() as u32,
                )
            }
            _ => return Err(CandyError::Libva("not a libva codec".into())),
        };
        let mut id: VABufferID = VA_INVALID_ID;
        va_ok(
            unsafe {
                vaCreateBuffer(
                    dpy,
                    context,
                    VABufferType_VAEncSequenceParameterBufferType,
                    size,
                    1,
                    buf,
                    &mut id,
                )
            },
            "vaCreateBuffer(seq)",
        )?;
        Ok(id)
    }

    fn create_rc_buffer(dpy: VADisplay, context: VAContextID) -> Result<VABufferID, CandyError> {
        let mut rc: VAEncMiscParameterRateControl = unsafe { std::mem::zeroed() };
        rc.bits_per_second = 0;
        rc.target_percentage = 100;
        rc.window_size = 0;
        rc.initial_qp = 26;
        rc.min_qp = 1;
        rc.max_qp = 51;
        let mut mp: VAEncMiscParameterBuffer = unsafe { std::mem::zeroed() };
        mp.type_ = VAEncMiscParameterType_VAEncMiscParameterTypeRateControl;
        // `data` is a flexible array; we point it at `rc` by writing rc first into
        // a combined allocation. Simpler: create a buffer large enough and map it.
        let total = std::mem::size_of::<VAEncMiscParameterBuffer>()
            + std::mem::size_of::<VAEncMiscParameterRateControl>();
        let mut storage: Vec<u8> = vec![0u8; total];
        // Copy mp header then rc payload.
        unsafe {
            std::ptr::copy_nonoverlapping(
                &mp as *const _ as *const u8,
                storage.as_mut_ptr(),
                std::mem::size_of::<VAEncMiscParameterBuffer>(),
            );
            std::ptr::copy_nonoverlapping(
                &rc as *const _ as *const u8,
                storage
                    .as_mut_ptr()
                    .add(std::mem::size_of::<VAEncMiscParameterBuffer>()),
                std::mem::size_of::<VAEncMiscParameterRateControl>(),
            );
        }
        let mut id: VABufferID = VA_INVALID_ID;
        va_ok(
            unsafe {
                vaCreateBuffer(
                    dpy,
                    context,
                    VABufferType_VAEncMiscParameterBufferType,
                    total as u32,
                    1,
                    storage.as_mut_ptr() as *mut c_void,
                    &mut id,
                )
            },
            "vaCreateBuffer(rc)",
        )?;
        Ok(id)
    }

    fn create_framerate_buffer(
        dpy: VADisplay,
        context: VAContextID,
        fps: u32,
    ) -> Result<VABufferID, CandyError> {
        let mut fr: VAEncMiscParameterFrameRate = unsafe { std::mem::zeroed() };
        fr.framerate = fps; // numerator; denominator assumed 1
        let mut mp: VAEncMiscParameterBuffer = unsafe { std::mem::zeroed() };
        mp.type_ = VAEncMiscParameterType_VAEncMiscParameterTypeFrameRate;
        let total = std::mem::size_of::<VAEncMiscParameterBuffer>()
            + std::mem::size_of::<VAEncMiscParameterFrameRate>();
        let mut storage: Vec<u8> = vec![0u8; total];
        unsafe {
            std::ptr::copy_nonoverlapping(
                &mp as *const _ as *const u8,
                storage.as_mut_ptr(),
                std::mem::size_of::<VAEncMiscParameterBuffer>(),
            );
            std::ptr::copy_nonoverlapping(
                &fr as *const _ as *const u8,
                storage
                    .as_mut_ptr()
                    .add(std::mem::size_of::<VAEncMiscParameterBuffer>()),
                std::mem::size_of::<VAEncMiscParameterFrameRate>(),
            );
        }
        let mut id: VABufferID = VA_INVALID_ID;
        va_ok(
            unsafe {
                vaCreateBuffer(
                    dpy,
                    context,
                    VABufferType_VAEncMiscParameterBufferType,
                    total as u32,
                    1,
                    storage.as_mut_ptr() as *mut c_void,
                    &mut id,
                )
            },
            "vaCreateBuffer(framerate)",
        )?;
        Ok(id)
    }

    /// Upload one NV12 frame to the VAAPI surface.
    fn upload_nv12(&self, nv12: &[u8]) -> Result<(), CandyError> {
        let fmt = VAImageFormat {
            fourcc: VA_FOURCC_NV12,
            ..unsafe { std::mem::zeroed() }
        };
        let mut image: VAImage = unsafe { std::mem::zeroed() };
        va_ok(
            unsafe { vaCreateImage(self.dpy, &fmt, self.w as c_int, self.h as c_int, &mut image) },
            "vaCreateImage",
        )?;
        let mut ptr: *mut c_void = std::ptr::null_mut();
        va_ok(
            unsafe { vaMapBuffer(self.dpy, image.buf, &mut ptr) },
            "vaMapBuffer(image)",
        )?;
        debug_assert_eq!(image.data_size as usize, nv12.len());
        unsafe {
            std::ptr::copy_nonoverlapping(nv12.as_ptr(), ptr as *mut u8, nv12.len());
        }
        va_ok(unsafe { vaUnmapBuffer(self.dpy, image.buf) }, "vaUnmapBuffer(image)")?;
        va_ok(
            unsafe {
                vaPutImage(
                    self.dpy,
                    self.surface,
                    image.image_id,
                    0,
                    0,
                    self.w as c_uint,
                    self.h as c_uint,
                    0,
                    0,
                    self.w as c_uint,
                    self.h as c_uint,
                )
            },
            "vaPutImage",
        )?;
        va_ok(unsafe { vaDestroyImage(self.dpy, image.image_id) }, "vaDestroyImage")?;
        Ok(())
    }

    /// Encode one NV12 frame; returns the raw coded bytes (across all coded
    /// buffer segments).
    fn encode_nv12(&mut self, nv12: &[u8], is_idr: bool) -> Result<Vec<u8>, CandyError> {
        self.upload_nv12(nv12)?;

        va_ok(
            unsafe { vaBeginPicture(self.dpy, self.context, self.surface) },
            "vaBeginPicture",
        )?;

        // Render the sequence + misc buffers once, on the first picture.
        let mut bufs: Vec<VABufferID> = Vec::new();
        if !self.seq_rendered {
            bufs.push(self.seq_buf);
            bufs.push(self.rc_buf);
            bufs.push(self.fr_buf);
            self.seq_rendered = true;
        }

        // Per-frame coded buffer.
        let coded_size = (self.w * self.h * 2 + 65536) as u32;
        let mut coded_buf: VABufferID = VA_INVALID_ID;
        va_ok(
            unsafe {
                vaCreateBuffer(
                    self.dpy,
                    self.context,
                    VABufferType_VAEncCodedBufferType,
                    coded_size,
                    1,
                    std::ptr::null_mut(),
                    &mut coded_buf,
                )
            },
            "vaCreateBuffer(coded)",
        )?;

        // Build the picture + slice/tile-group parameter buffers (codec-specific).
        let pic_buf = self.create_picture_buffer(coded_buf, is_idr)?;
        let slice_buf = self.create_slice_buffer(is_idr)?;
        bufs.push(pic_buf);
        bufs.push(slice_buf);

        va_ok(
            unsafe {
                vaRenderPicture(
                    self.dpy,
                    self.context,
                    bufs.as_mut_ptr(),
                    bufs.len() as c_int,
                )
            },
            "vaRenderPicture",
        )?;
        va_ok(unsafe { vaEndPicture(self.dpy, self.context) }, "vaEndPicture")?;
        va_ok(
            unsafe { vaSyncSurface(self.dpy, self.surface) },
            "vaSyncSurface",
        )?;

        // Map the coded buffer and gather the bytes.
        let mut p: *mut c_void = std::ptr::null_mut();
        va_ok(
            unsafe { vaMapBuffer(self.dpy, coded_buf, &mut p) },
            "vaMapBuffer(coded)",
        )?;
        let mut out = Vec::new();
        let mut seg: *mut VACodedBufferSegment = p as *mut VACodedBufferSegment;
        while !seg.is_null() {
            let s = unsafe { &*seg };
            if s.size > 0 && !s.buf.is_null() {
                let bytes = unsafe {
                    std::slice::from_raw_parts(s.buf as *const u8, s.size as usize)
                };
                out.extend_from_slice(bytes);
            }
            seg = s.next as *mut VACodedBufferSegment;
        }
        va_ok(unsafe { vaUnmapBuffer(self.dpy, coded_buf) }, "vaUnmapBuffer(coded)")?;
        va_ok(unsafe { vaDestroyBuffer(self.dpy, coded_buf) }, "vaDestroyBuffer(coded)")?;
        va_ok(unsafe { vaDestroyBuffer(self.dpy, pic_buf) }, "vaDestroyBuffer(pic)")?;
        va_ok(unsafe { vaDestroyBuffer(self.dpy, slice_buf) }, "vaDestroyBuffer(slice)")?;

        Ok(out)
    }

    fn create_picture_buffer(
        &self,
        coded_buf: VABufferID,
        is_idr: bool,
    ) -> Result<VABufferID, CandyError> {
        let (data, size): (*mut c_void, u32) = if self.is_av1 {
            let mut p: VAEncPictureParameterBufferAV1 = unsafe { std::mem::zeroed() };
            p.coded_buf = coded_buf;
            p.frame_width_minus_1 = (self.w - 1) as u16;
            p.frame_height_minus_1 = (self.h - 1) as u16;
            p.base_qindex = 128;
            // 0 = key frame, 1 = inter frame. We encode all-intra.
            p.picture_flags.set_frame_type(if is_idr { 0 } else { 1 });
            if is_idr {
                p.refresh_frame_flags = 0xFF;
            }
            (
                &p as *const _ as *mut c_void,
                std::mem::size_of::<VAEncPictureParameterBufferAV1>() as u32,
            )
        } else if self.is_hevc {
            let mut p: VAEncPictureParameterBufferHEVC = unsafe { std::mem::zeroed() };
            p.decoded_curr_pic = VAPictureHEVC {
                picture_id: self.surface,
                pic_order_cnt: 0,
                flags: 0,
                ..unsafe { std::mem::zeroed() }
            };
            p.coded_buf = coded_buf;
            p.pic_init_qp = 26;
            p.nal_unit_type = if is_idr { 19 } else { 1 };
            p.num_ref_idx_l0_default_active_minus1 = 0;
            p.num_ref_idx_l1_default_active_minus1 = 0;
            p.slice_pic_parameter_set_id = 0;
            p.pic_fields.set_idr_pic_flag(if is_idr { 1 } else { 0 });
            (
                &p as *const _ as *mut c_void,
                std::mem::size_of::<VAEncPictureParameterBufferHEVC>() as u32,
            )
        } else {
            let mut p: VAEncPictureParameterBufferH264 = unsafe { std::mem::zeroed() };
            p.CurrPic = VAPictureH264 {
                picture_id: self.surface,
                frame_idx: 0,
                flags: 0,
                TopFieldOrderCnt: 0,
                BottomFieldOrderCnt: 0,
                ..unsafe { std::mem::zeroed() }
            };
            p.coded_buf = coded_buf;
            p.pic_parameter_set_id = 0;
            p.seq_parameter_set_id = 0;
            p.frame_num = 0;
            p.pic_init_qp = 26;
            p.num_ref_idx_l0_active_minus1 = 0;
            p.num_ref_idx_l1_active_minus1 = 0;
            p.chroma_qp_index_offset = 0;
            p.second_chroma_qp_index_offset = 0;
            p.pic_fields.set_idr_pic_flag(if is_idr { 1 } else { 0 });
            (
                &p as *const _ as *mut c_void,
                std::mem::size_of::<VAEncPictureParameterBufferH264>() as u32,
            )
        };
        let mut id: VABufferID = VA_INVALID_ID;
        va_ok(
            unsafe {
                vaCreateBuffer(
                    self.dpy,
                    self.context,
                    VABufferType_VAEncPictureParameterBufferType,
                    size,
                    1,
                    data,
                    &mut id,
                )
            },
            "vaCreateBuffer(pic)",
        )?;
        Ok(id)
    }

    fn create_slice_buffer(&self, is_idr: bool) -> Result<VABufferID, CandyError> {
        let slice_type = if is_idr { 2 } else { 0 }; // I for H264/AV1, I for HEVC
        let (data, size): (*mut c_void, u32) = if self.is_av1 {
            // AV1 has no "slice" buffer; it uses a tile-group buffer.
            let mut s: VAEncTileGroupBufferAV1 = unsafe { std::mem::zeroed() };
            s.tg_start = 0;
            s.tg_end = 0;
            (
                &s as *const _ as *mut c_void,
                std::mem::size_of::<VAEncTileGroupBufferAV1>() as u32,
            )
        } else if self.is_hevc {
            let mut s: VAEncSliceParameterBufferHEVC = unsafe { std::mem::zeroed() };
            let ctu = ((self.w + 63) / 64) * ((self.h + 63) / 64);
            s.slice_segment_address = 0;
            s.num_ctu_in_slice = ctu;
            s.slice_type = slice_type;
            s.num_ref_idx_l0_active_minus1 = 0;
            s.num_ref_idx_l1_active_minus1 = 0;
            s.max_num_merge_cand = 3;
            s.slice_qp_delta = 0;
            (
                &s as *const _ as *mut c_void,
                std::mem::size_of::<VAEncSliceParameterBufferHEVC>() as u32,
            )
        } else {
            let mut s: VAEncSliceParameterBufferH264 = unsafe { std::mem::zeroed() };
            s.macroblock_address = 0;
            s.num_macroblocks = ((self.w / 16) * (self.h / 16)) as u32;
            s.slice_type = slice_type;
            s.pic_parameter_set_id = 0;
            s.idr_pic_id = if is_idr { self.frame_count as u16 } else { 0 };
            s.num_ref_idx_l0_active_minus1 = 0;
            s.num_ref_idx_l1_active_minus1 = 0;
            s.cabac_init_idc = 0;
            s.slice_qp_delta = 0;
            s.disable_deblocking_filter_idc = 0;
            (
                &s as *const _ as *mut c_void,
                std::mem::size_of::<VAEncSliceParameterBufferH264>() as u32,
            )
        };
        let buf_type: VABufferType = if self.is_av1 {
            VA_ENC_TILE_GROUP_BUFFER_TYPE
        } else {
            VABufferType_VAEncSliceParameterBufferType
        };
        let mut id: VABufferID = VA_INVALID_ID;
        va_ok(
            unsafe {
                vaCreateBuffer(
                    self.dpy,
                    self.context,
                    buf_type,
                    size,
                    1,
                    data,
                    &mut id,
                )
            },
            "vaCreateBuffer(slice)",
        )?;
        Ok(id)
    }

    /// Push one RGBA frame (already composited to the uniform canvas).
    pub fn push(&mut self, frame: &RenderedFrame) -> Result<(), CandyError> {
        let nv12 = rgba_to_nv12(&frame.rgba, frame.width, frame.height);
        // All-intra: every frame is an IDR / key frame. This keeps the stream
        // independently decodable (no reference-frame management) and matches the
        // all-intra sequence parameters configured above.
        let is_idr = true;
        let raw = self.encode_nv12(&nv12, is_idr)?;

        // Normalise the coded bytes (Annex-B → length-prefixed for H264/HEVC,
        // strip AV1 temporal delimiter) and extract codec_private on frame 0.
        let first = !self.private_ready;
        let sample = if self.is_av1 {
            process_av1(&raw, &mut self.codec_private, first).0
        } else if self.is_hevc {
            process_hevc(&raw, &mut self.codec_private, first).0
        } else {
            process_h264(&raw, &mut self.codec_private, first).0
        };

        // Every frame is a key frame in the all-intra stream.
        let is_key = true;

        self.samples
            .write_all(&sample)
            .map_err(|e| CandyError::Libva(format!("sample write: {e}")))?;
        self.sample_sizes.push(sample.len() as u32);
        self.keyframes.push(is_key);
        self.private_ready = true;
        self.frame_count += 1;
        Ok(())
    }

    /// Mux the accumulated samples into the chosen container (no FFmpeg).
    pub fn finish(
        mut self,
        output: &std::path::Path,
        audio: Option<&AudioData>,
    ) -> Result<(), CandyError> {
        if self.frame_count == 0 {
            return Err(CandyError::Libva("no frames were encoded".into()));
        }
        if self.codec_private.is_empty() {
            return Err(CandyError::Libva(
                "libva encode produced no codec-private data (sequence header missing)"
                    .into(),
            ));
        }
        let evf = EncodedVideoFile {
            width: self.w,
            height: self.h,
            fps: self.fps,
            is_av1: self.is_av1,
            is_hevc: self.is_hevc,
            codec_private: std::mem::take(&mut self.codec_private),
            sample_sizes: std::mem::take(&mut self.sample_sizes),
            keyframes: std::mem::take(&mut self.keyframes),
            samples_path: std::mem::take(&mut self.samples_path),
        };
        // Drop the samples file handle so the muxer can move/rename it.
        drop(self.samples);

        let res = match self.container {
            Container::Mp4 => container::mux_mp4_to_file(&evf, audio, output, &self.meta),
            Container::Mkv => {
                container::mux_matroska_to_file(&evf, audio, false, output, &self.meta)
            }
            Container::Webm => {
                container::mux_matroska_to_file(&evf, audio, true, output, &self.meta)
            }
        };

        // Tear down VAAPI resources regardless of mux outcome.
        unsafe {
            vaDestroySurfaces(self.dpy, &mut self.surface, 1);
            vaDestroyContext(self.dpy, self.context);
            vaDestroyConfig(self.dpy, self.config);
            vaTerminate(self.dpy);
        }
        res
    }
}

impl Drop for LibvaStream {
    fn drop(&mut self) {
        unsafe {
            vaDestroyBuffer(self.dpy, self.seq_buf);
            vaDestroyBuffer(self.dpy, self.rc_buf);
            vaDestroyBuffer(self.dpy, self.fr_buf);
            vaDestroySurfaces(self.dpy, &mut self.surface, 1);
            vaDestroyContext(self.dpy, self.context);
            vaDestroyConfig(self.dpy, self.config);
            vaTerminate(self.dpy);
        }
    }
}

fn va_ok(status: VAStatus, ctx: &str) -> Result<(), CandyError> {
    if status == VA_STATUS_SUCCESS {
        Ok(())
    } else {
        Err(CandyError::Libva(format!(
            "libva {ctx} failed (VAStatus {status})"
        )))
    }
}

// ===================== RGBA → NV12 =====================

/// Convert an RGBA buffer to NV12 (Y plane then interleaved UV), the surface
/// format VAAPI expects for encode.
fn rgba_to_nv12(rgba: &[u8], w: usize, h: usize) -> Vec<u8> {
    let wh = w * h;
    let mut nv12 = vec![0u8; wh + wh / 2];
    let y_plane = &mut nv12[..wh];
    let uv_plane = &mut nv12[wh..];
    for y in 0..h {
        for x in 0..w {
            let i = (y * w + x) * 4;
            let r = rgba[i] as f32;
            let g = rgba[i + 1] as f32;
            let b = rgba[i + 2] as f32;
            let yy = (0.257 * r + 0.504 * g + 0.098 * b + 16.0).clamp(0.0, 255.0) as u8;
            y_plane[y * w + x] = yy;
        }
    }
    for y in (0..h).step_by(2) {
        for x in (0..w).step_by(2) {
            let i = (y * w + x) * 4;
            let r = rgba[i] as f32;
            let g = rgba[i + 1] as f32;
            let b = rgba[i + 2] as f32;
            let u = (-0.148 * r - 0.291 * g + 0.439 * b + 128.0).clamp(0.0, 255.0) as u8;
            let v = (0.439 * r - 0.368 * g - 0.071 * b + 128.0).clamp(0.0, 255.0) as u8;
            let o = (y / 2) * w + x;
            uv_plane[o] = u;
            uv_plane[o + 1] = v;
        }
    }
    nv12
}

// ===================== Annex-B → length-prefixed =====================

/// Split an Annex-B byte stream into NAL units (including the 1-byte NAL
/// header, without the leading start code).
fn split_annexb(data: &[u8]) -> Vec<Vec<u8>> {
    let mut nals = Vec::new();
    let mut i = 0;
    let n = data.len();
    while i < n {
        // Find next start code.
        if (i + 3 <= n && &data[i..i + 3] == [0, 0, 1])
            || (i + 4 <= n && &data[i..i + 4] == [0, 0, 0, 1])
        {
            let sc = if i + 4 <= n && &data[i..i + 4] == [0, 0, 0, 1] {
                4
            } else {
                3
            };
            let start = i + sc;
            // Find next start code.
            let mut j = start;
            while j + 3 <= n {
                if &data[j..j + 3] == [0, 0, 1] || (j + 4 <= n && &data[j..j + 4] == [0, 0, 0, 1]) {
                    break;
                }
                j += 1;
            }
            if j > start {
                nals.push(data[start..j].to_vec());
            }
            i = j;
        } else {
            i += 1;
        }
    }
    nals
}

fn annexb_to_length_prefixed(nals: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    for nal in nals {
        out.extend_from_slice(&(nal.len() as u32).to_be_bytes());
        out.extend_from_slice(nal);
    }
    out
}

/// H.264: extract SPS(7)/PPS(8) for `avcC` on the first frame, and convert the
/// frame to length-prefixed form. Returns (sample, Some(avcC) on first call).
fn process_h264(raw: &[u8], private: &mut Vec<u8>, first: bool) -> (Vec<u8>, Option<Vec<u8>>) {
    let nals = split_annexb(raw);
    let mut sps = None;
    let mut pps = None;
    for nal in &nals {
        if nal.is_empty() {
            continue;
        }
        let t = nal[0] & 0x1F;
        if t == 7 {
            sps = Some(nal.clone());
        } else if t == 8 {
            pps = Some(nal.clone());
        }
    }
    let priv_out = if first {
        if let (Some(s), Some(p)) = (&sps, &pps) {
            let avcc = build_avcc(s, p);
            *private = avcc.clone();
            Some(avcc)
        } else {
            None
        }
    } else {
        None
    };
    (annexb_to_length_prefixed(&nals), priv_out)
}

/// HEVC: extract VPS(32)/SPS(33)/PPS(34) for `hvcC`, convert to length-prefixed.
fn process_hevc(raw: &[u8], private: &mut Vec<u8>, first: bool) -> (Vec<u8>, Option<Vec<u8>>) {
    let nals = split_annexb(raw);
    let mut vps = None;
    let mut sps = None;
    let mut pps = None;
    for nal in &nals {
        if nal.is_empty() {
            continue;
        }
        let t = (nal[0] >> 1) & 0x3F;
        match t {
            32 => vps = Some(nal.clone()),
            33 => sps = Some(nal.clone()),
            34 => pps = Some(nal.clone()),
            _ => {}
        }
    }
    let priv_out = if first {
        if let (Some(v), Some(s), Some(p)) = (&vps, &sps, &pps) {
            let hvcc = build_hvcc(v, s, p);
            *private = hvcc.clone();
            Some(hvcc)
        } else {
            None
        }
    } else {
        None
    };
    (annexb_to_length_prefixed(&nals), priv_out)
}

fn build_avcc(sps: &[u8], pps: &[u8]) -> Vec<u8> {
    let mut c = Vec::with_capacity(11 + sps.len() + pps.len());
    c.push(1); // configurationVersion
    c.push(sps[1]); // AVCProfileIndication
    c.push(sps[2]); // profile_compatibility
    c.push(sps[3]); // AVCLevelIndication
    c.push(0xFF); // 6 bits reserved (111111) + lengthSizeMinusOne = 3
    c.push(0xE1); // 3 bits reserved (111) + numOfSPS = 1
    c.extend_from_slice(&(sps.len() as u16).to_be_bytes());
    c.extend_from_slice(sps);
    c.push(1); // numOfPPS
    c.extend_from_slice(&(pps.len() as u16).to_be_bytes());
    c.extend_from_slice(pps);
    c
}

/// Best-effort minimal `hvcC` (single layer). HEVC parameter-set layout:
/// NAL header is 2 bytes; then `profile_space(2)|tier(1)|profile_idc(5)`,
/// 32 bits compat flags, 1 byte constraint indicators, then `level_idc`.
fn build_hvcc(vps: &[u8], sps: &[u8], pps: &[u8]) -> Vec<u8> {
    let _ = vps;
    let profile_byte = sps.get(2).copied().unwrap_or(0);
    let profile_space = (profile_byte >> 6) & 0x3;
    let tier = (profile_byte >> 5) & 0x1;
    let profile_idc = profile_byte & 0x1F;
    let level_idc = sps.get(8).copied().unwrap_or(0);
    let mut c = Vec::with_capacity(13 + sps.len() + pps.len());
    c.push(1); // configurationVersion
    c.push((profile_space << 6) | (tier << 5) | profile_idc);
    c.extend_from_slice(&[0xFF, 0xFF, 0xFF, 0xFF]); // general_profile_compatibility_flags
    c.push(0x00); // general_constraint_indicator_flags (top byte)
    c.push(0x00);
    c.push(0x00);
    c.push(0x00);
    c.push(0x00); // general_constraint_indicator_flags (low) + reserved
    c.push(level_idc);
    c.extend_from_slice(&[0x00, 0x00]); // min_spatial_segmentation_idc + reserved
    c.push(0x00); // parallelismType
    c.push(0x00); // chromaFormatIdc (1 = 4:2:0)
    c.push(0x00); // bitDepthLumaMinus8
    c.push(0x00); // bitDepthChromaMinus8
    c.extend_from_slice(&[0x00, 0x00]); // avgFrameRate
    c.push(0x0F); // constantFrameRate=0, numTemporalLayers=1, temporalIdNested=1, lengthSizeMinusOne=3
    c.push(3); // numOfArrays = 3 (VPS, SPS, PPS)
    // VPS array
    c.push(0x20); // array_completeness=1, NAL type (32)
    c.extend_from_slice(&(1u16).to_be_bytes());
    c.extend_from_slice(&(vps.len() as u16).to_be_bytes());
    c.extend_from_slice(vps);
    // SPS array
    c.push(0x21); // NAL type 33
    c.extend_from_slice(&(1u16).to_be_bytes());
    c.extend_from_slice(&(sps.len() as u16).to_be_bytes());
    c.extend_from_slice(sps);
    // PPS array
    c.push(0x22); // NAL type 34
    c.extend_from_slice(&(1u16).to_be_bytes());
    c.extend_from_slice(&(pps.len() as u16).to_be_bytes());
    c.extend_from_slice(pps);
    c
}

// ===================== AV1 OBUs =====================

/// Parse AV1 OBUs, returning (obu_type, payload_without_header) for each.
///
/// AV1 OBUs are length-delimited (not Annex-B). The Temporal Delimiter OBU
/// (type 2) has no size field and a zero-length payload, so it is special-cased.
fn parse_av1_obus(data: &[u8]) -> Vec<(u8, Vec<u8>)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < data.len() {
        if i + 1 > data.len() {
            break;
        }
        let header = data[i];
        let obu_type = (header >> 3) & 0x0F;
        let has_size = (header & 0x02) != 0;
        i += 1;
        let mut size = 0usize;
        if has_size {
            // LEB128 size.
            let mut shift = 0;
            while i < data.len() {
                let b = data[i];
                i += 1;
                size |= ((b & 0x7F) as usize) << shift;
                if b & 0x80 == 0 {
                    break;
                }
                shift += 7;
            }
        } else if obu_type == 2 {
            // Temporal Delimiter: no size field, zero-length payload.
            size = 0;
        } else {
            // Last OBU with no size field: extends to end of buffer.
            size = data.len() - i;
        }
        let end = (i + size).min(data.len());
        out.push((obu_type, data[i..end].to_vec()));
        i = end;
    }
    out
}

fn build_av1c(seq_hdr: &[u8]) -> Vec<u8> {
    // seq_hdr[0]: seq_profile(3) | seq_level_idx_0(5)
    // seq_hdr[1] bit0: seq_tier_0
    let profile = (seq_hdr[0] >> 5) & 0x07;
    let level = seq_hdr[0] & 0x1F;
    let tier = seq_hdr.get(1).copied().unwrap_or(0) & 0x01;
    let mut c = Vec::with_capacity(4);
    c.push(0x81); // marker (1) + version (1)
    c.push((profile << 5) | (level & 0x1F));
    // 8-bit 4:2:0: high_bitdepth=0, twelve_bit=0, monochrome=0,
    // chroma_sub_x=1, chroma_sub_y=1; reserved + initial_presentation_delay = 0
    c.push((tier << 7) | (1 << 3) | (1 << 2));
    c.push(0x00);
    c
}

/// AV1: extract the sequence-header OBU for `av1C` on frame 0, strip the
/// leading Temporal Delimiter (type 2) from each sample.
fn process_av1(raw: &[u8], private: &mut Vec<u8>, first: bool) -> (Vec<u8>, Option<Vec<u8>>) {
    let obus = parse_av1_obus(raw);
    let mut out = Vec::new();
    let mut seq_hdr: Option<Vec<u8>> = None;
    for (t, payload) in &obus {
        if *t == 2 {
            continue; // drop Temporal Delimiter from the sample
        }
        if *t == 1 {
            seq_hdr = Some(payload.clone());
        }
        // Re-emit the OBU (header byte + payload) so the sample stays valid.
        // Reconstruct the header: keep original header byte but force has_size=1
        // so the muxed sample uses length-delimited OBUs.
        let header = (payload_first_header(raw, t) & !0x02) | 0x02;
        out.push(header);
        // LEB128 size
        let size = payload.len();
        let mut s = size;
        loop {
            let b = (s & 0x7F) as u8;
            s >>= 7;
            if s == 0 {
                out.push(b);
                break;
            }
            out.push(b | 0x80);
        }
        out.extend_from_slice(payload);
    }
    let priv_out = if first {
        if let Some(sh) = &seq_hdr {
            let av1c = build_av1c(sh);
            *private = av1c.clone();
            Some(av1c)
        } else {
            None
        }
    } else {
        None
    };
    (out, priv_out)
}

/// Best-effort: recover the original OBU header byte for a parsed OBU. We did
/// not retain it, so reconstruct from type + assume no extension, has_size=1.
fn payload_first_header(_raw: &[u8], obu_type: &u8) -> u8 {
    (obu_type << 3) | 0x02 // reserved(1b)=0, extension=0, has_size=1
}
