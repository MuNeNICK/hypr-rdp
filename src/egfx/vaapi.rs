//! VA-API H.264 hardware encoder for Intel/AMD GPUs.
//!
//! Uses VA-API for GPU-accelerated H.264 encoding. Surfaces are pooled for
//! triple-buffering, and a separate reconstructed surface pool enables true
//! inter-frame prediction via DPB tracking.
//!
//! Thread Safety: NOT Send — VA-API is thread-local. Create and use on the
//! same thread (the capture thread satisfies this).

use std::mem;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use anyhow::{bail, Context, Result};
use libva_sys::va_display_drm as va;

use super::vaapi_sys::{
    self as sys, VAConfigAttrib, VADRMPRIMESurfaceDescriptor, VAImageFormat, VAProfile, VaBuffer,
    VaConfig, VaContext, VaDisplay, VaImageMapping, VaSurface, VA_INVALID_ID, VA_INVALID_SURFACE,
    VA_PICTURE_H264_INVALID, VA_PICTURE_H264_SHORT_TERM_REFERENCE, VA_RT_FORMAT_YUV420,
};
use super::H264RateControl;

const INPUT_SURFACE_POOL_SIZE: usize = 3;
const CODED_BUFFER_COUNT: usize = 3;

const SLICE_TYPE_I: u8 = 2;
const SLICE_TYPE_P: u8 = 0;

const IDR_INTERVAL: u32 = 30;
const H264_DEFAULT_PIC_INIT_QP: u8 = 26;
const VAAPI_DESIRED_PACKED_HEADERS: u32 =
    va::VA_ENC_PACKED_HEADER_SEQUENCE | va::VA_ENC_PACKED_HEADER_SLICE;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PackedHeaderPlan {
    sequence: bool,
    slice: bool,
}

fn packed_header_config_attrib(supported: u32) -> Option<VAConfigAttrib> {
    if supported == va::VA_ATTRIB_NOT_SUPPORTED {
        return None;
    }

    let selected = supported & VAAPI_DESIRED_PACKED_HEADERS;
    (selected != va::VA_ENC_PACKED_HEADER_NONE).then_some(VAConfigAttrib {
        type_: va::VAConfigAttribType_VAConfigAttribEncPackedHeaders,
        value: selected,
    })
}

fn packed_header_plan(selected: u32, is_idr: bool) -> PackedHeaderPlan {
    PackedHeaderPlan {
        sequence: is_idr && (selected & va::VA_ENC_PACKED_HEADER_SEQUENCE) != 0,
        slice: (selected & va::VA_ENC_PACKED_HEADER_SLICE) != 0,
    }
}

/// idr_pic_id for the packed slice header and the VA slice parameters.
/// H.264 requires consecutive IDR pictures to carry different idr_pic_id
/// values; deriving it from the IDR sequence number satisfies that while
/// keeping the ue(v) encoding short. (frame_num cannot serve here anymore:
/// it is 0 for every IDR.)
fn h264_idr_pic_id(idr_sequence: u64) -> u16 {
    (idr_sequence % 16) as u16
}

/// Pick the lowest level whose MaxMBPS *and* MaxFS admit this stream
/// (Rec. ITU-T H.264 Table A-1). Selecting by MaxMBPS alone signals
/// non-conformant levels for large frames: 3840x2160 is 32400 macroblocks,
/// past level 5.0's MaxFS of 22080 even at low frame rates. Macroblock
/// counts round up for non-multiple-of-16 sizes.
fn h264_level(
    profile: VAProfile,
    bitrate: u32,
    width: u32,
    height: u32,
    fps: u32,
    max_dec_frame_buffering: u32,
) -> Option<u8> {
    // level_idc, MaxMBPS, MaxFS, MaxDpbMbs, MaxBR (Table A-1 units).
    const LEVELS: &[(u8, u32, u32, u32, u32)] = &[
        (30, 40_500, 1_620, 8_100, 10_000),
        (31, 108_000, 3_600, 18_000, 14_000),
        (32, 216_000, 5_120, 20_480, 20_000),
        (40, 245_760, 8_192, 32_768, 20_000),
        (41, 245_760, 8_192, 32_768, 50_000),
        (42, 522_240, 8_704, 34_816, 50_000),
        (50, 589_824, 22_080, 110_400, 135_000),
        (51, 983_040, 36_864, 184_320, 240_000),
        (52, 2_073_600, 36_864, 184_320, 240_000),
    ];

    let width_mbs = u64::from(width.div_ceil(16));
    let height_mbs = u64::from(height.div_ceil(16));
    let frame_mbs = width_mbs * height_mbs;
    if frame_mbs == 0 || fps == 0 {
        return None;
    }
    let mbs_per_sec = frame_mbs * u64::from(fps);
    // Annex A Table A-2: High profile permits the larger NAL bitrate factor.
    let bitrate_factor = if profile == sys::VA_PROFILE_H264_HIGH {
        1_500u64
    } else {
        1_200u64
    };

    LEVELS
        .iter()
        .find(|(_, max_mbps, max_fs, max_dpb_mbs, max_br)| {
            let max_fs = u64::from(*max_fs);
            let max_dpb_frames = (u64::from(*max_dpb_mbs) / frame_mbs).min(16);
            mbs_per_sec <= u64::from(*max_mbps)
                && frame_mbs <= max_fs
                && width_mbs * width_mbs <= 8 * max_fs
                && height_mbs * height_mbs <= 8 * max_fs
                && u64::from(max_dec_frame_buffering) <= max_dpb_frames
                && u64::from(bitrate) <= u64::from(*max_br) * bitrate_factor
        })
        .map(|(level, ..)| *level)
}

/// frame_num and pic_order_cnt_lsb for a picture, counted from the last
/// IDR: §7.4.3 requires an IDR picture to carry frame_num 0, so the
/// counter rebases at every IDR.
fn slice_numbering(frames_since_idr: u64) -> (u16, i32) {
    // max_frame_num = 256, max_pic_order_cnt_lsb = 256 (both minus4 = 4).
    (
        (frames_since_idr % 256) as u16,
        ((frames_since_idr * 2) % 256) as i32,
    )
}

/// Build a packed slice header matching the SPS/PPS and VA slice parameters:
/// 8-bit frame number/POC, CABAC, one reference, and deblocking enabled.
/// The returned bit length excludes byte-padding after the final syntax bit.
fn generate_packed_slice_header(
    is_idr: bool,
    frame_num: u16,
    idr_pic_id: u16,
    poc: i32,
) -> (Vec<u8>, u32) {
    let mut data = Vec::with_capacity(16);
    data.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);

    let mut bits = BitWriter::new();
    bits.write_bits(0, 1); // forbidden_zero_bit
    bits.write_bits(3, 2); // nal_ref_idc: reference picture
    bits.write_bits(if is_idr { 5 } else { 1 }, 5); // nal_unit_type

    bits.write_ue(0); // first_mb_in_slice
    bits.write_ue(if is_idr { 7 } else { 5 }); // slice_type: all-I / all-P
    bits.write_ue(0); // pic_parameter_set_id
    bits.write_bits(u32::from(frame_num) & 0xff, 8); // frame_num
    if is_idr {
        bits.write_ue(u32::from(idr_pic_id));
    }
    bits.write_bits((poc as u32) & 0xff, 8); // pic_order_cnt_lsb
    if !is_idr {
        bits.write_bits(0, 1); // num_ref_idx_active_override_flag
        bits.write_bits(0, 1); // ref_pic_list_modification_flag_l0
    }
    if is_idr {
        bits.write_bits(0, 1); // no_output_of_prior_pics_flag
        bits.write_bits(0, 1); // long_term_reference_flag
    } else {
        bits.write_bits(0, 1); // adaptive_ref_pic_marking_mode_flag
        bits.write_ue(0); // cabac_init_idc
    }
    bits.write_se(0); // slice_qp_delta
    bits.write_ue(0); // disable_deblocking_filter_idc
    bits.write_se(0); // slice_alpha_c0_offset_div2
    bits.write_se(0); // slice_beta_offset_div2

    let bit_length = 32 + bits.bit_len();
    data.extend_from_slice(&bits.finish_with_emulation_prevention());
    (data, bit_length)
}

/// Scan /dev/dri/renderD* for a VA-API device that supports H.264 encoding.
fn find_vaapi_device() -> Result<PathBuf> {
    let dri_path = Path::new("/dev/dri");
    if !dri_path.exists() {
        bail!("/dev/dri not found");
    }

    let mut devices: Vec<PathBuf> = std::fs::read_dir(dri_path)
        .context("failed to read /dev/dri")?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().starts_with("renderD"))
        .map(|e| e.path())
        .collect();
    devices.sort();

    if devices.is_empty() {
        bail!("no render devices found in /dev/dri");
    }

    for device in &devices {
        let display = match VaDisplay::open_drm(device) {
            Ok(d) => d,
            Err(e) => {
                tracing::trace!(device = %device.display(), "Skipping VA-API device: {:?}", e);
                continue;
            }
        };

        let profiles = match display.query_config_profiles() {
            Ok(p) => p,
            Err(_) => continue,
        };

        let h264_profile = if profiles.contains(&sys::VA_PROFILE_H264_HIGH) {
            sys::VA_PROFILE_H264_HIGH
        } else if profiles.contains(&sys::VA_PROFILE_H264_MAIN) {
            sys::VA_PROFILE_H264_MAIN
        } else {
            continue;
        };

        let entrypoints = match display.query_config_entrypoints(h264_profile) {
            Ok(e) => e,
            Err(_) => continue,
        };

        if entrypoints.contains(&sys::VA_ENTRYPOINT_ENC_SLICE) {
            let vendor = display
                .query_vendor_string()
                .unwrap_or_else(|_| "unknown".into());
            tracing::trace!(
                device = %device.display(),
                vendor = %vendor,
                profile = ?h264_profile,
                "Found VA-API device with H.264 encode support"
            );
            return Ok(device.clone());
        }
    }

    bail!(
        "no VA-API device with H.264 encode support found (checked {} devices)",
        devices.len()
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct DpbEntry {
    surface_id: u32,
    frame_num: u16,
    poc: i32,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum VaapiAvc444Subframe {
    Luma,
    Chroma,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum VaapiEncodeRole {
    Generic,
    #[cfg(test)]
    Avc444(VaapiAvc444Subframe),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum VaapiReferenceMode {
    Single,
    #[cfg(test)]
    Avc444Subframes,
}

impl VaapiReferenceMode {
    fn max_num_ref_frames(self) -> u32 {
        1
    }

    fn recon_surface_pool_size(self) -> usize {
        self.max_num_ref_frames() as usize + 1
    }

    fn uses_periodic_idr(self) -> bool {
        self == Self::Single
    }
}

#[derive(Clone, Debug, Default)]
struct VaapiReferenceState {
    last: Option<DpbEntry>,
    #[cfg(test)]
    avc444_luma: Option<DpbEntry>,
    #[cfg(test)]
    avc444_chroma: Option<DpbEntry>,
}

impl VaapiReferenceState {
    fn clear(&mut self) {
        self.last = None;
        #[cfg(test)]
        {
            self.avc444_luma = None;
            self.avc444_chroma = None;
        }
    }

    fn primary_reference(&self, role: VaapiEncodeRole) -> Option<DpbEntry> {
        let _ = role;
        self.last
    }

    fn reference_frames(&self, mode: VaapiReferenceMode, role: VaapiEncodeRole) -> Vec<DpbEntry> {
        let mut refs = Vec::new();
        if let Some(reference) = self.primary_reference(role) {
            refs.push(reference);
        }
        let _ = mode;
        refs
    }

    fn record(&mut self, role: VaapiEncodeRole, entry: DpbEntry) {
        self.last = Some(entry);
        match role {
            VaapiEncodeRole::Generic => {}
            #[cfg(test)]
            VaapiEncodeRole::Avc444(VaapiAvc444Subframe::Luma) => {
                self.avc444_luma = Some(entry);
            }
            #[cfg(test)]
            VaapiEncodeRole::Avc444(VaapiAvc444Subframe::Chroma) => {
                self.avc444_chroma = Some(entry);
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct VaapiRateControlPolicy {
    config_mode: u32,
    pic_init_qp: u8,
    bits_per_second: u32,
    target_percentage: u32,
    initial_qp: u32,
    min_qp: u32,
    rc_flags: u32,
    max_qp: u32,
}

fn vaapi_rate_control_policy(
    bitrate: u32,
    quality: u8,
    rate_control: H264RateControl,
) -> VaapiRateControlPolicy {
    let qp = quality.min(51);
    match rate_control {
        H264RateControl::Vbr => VaapiRateControlPolicy {
            config_mode: sys::VA_RC_VBR,
            pic_init_qp: H264_DEFAULT_PIC_INIT_QP,
            bits_per_second: bitrate,
            target_percentage: 100,
            initial_qp: qp.into(),
            min_qp: 0,
            rc_flags: 0,
            max_qp: u32::from(qp.max(1)),
        },
        H264RateControl::Cqp => VaapiRateControlPolicy {
            config_mode: sys::VA_RC_CQP,
            pic_init_qp: qp,
            bits_per_second: 0,
            target_percentage: 100,
            initial_qp: qp.into(),
            min_qp: qp.into(),
            rc_flags: 1 << 1,
            max_qp: qp.into(),
        },
    }
}

struct VaapiRateControlParameters {
    rate_control: va::VAEncMiscParameterRateControl,
    hrd: Option<va::VAEncMiscParameterHRD>,
    frame_rate: va::VAEncMiscParameterFrameRate,
}

fn vaapi_rate_control_parameters(
    policy: VaapiRateControlPolicy,
    fps: u32,
) -> VaapiRateControlParameters {
    let mut rate_control: va::VAEncMiscParameterRateControl = unsafe { mem::zeroed() };
    rate_control.bits_per_second = policy.bits_per_second;
    rate_control.target_percentage = policy.target_percentage;
    rate_control.window_size = 1000;
    rate_control.initial_qp = policy.initial_qp;
    rate_control.min_qp = policy.min_qp;
    rate_control.rc_flags = va::_VAEncMiscParameterRateControl__bindgen_ty_1 {
        value: policy.rc_flags,
    };
    rate_control.max_qp = policy.max_qp;

    // libva defines HRD/VBV parameters for modes with a bitrate target.
    // CQP has no target, so submitting a zero-sized HRD buffer is invalid
    // policy and crashes some radeonsi versions in vaEndPicture.
    let hrd = (policy.bits_per_second > 0).then(|| {
        let mut hrd: va::VAEncMiscParameterHRD = unsafe { mem::zeroed() };
        hrd.initial_buffer_fullness = policy.bits_per_second / 2;
        hrd.buffer_size = policy.bits_per_second;
        hrd
    });

    let mut frame_rate: va::VAEncMiscParameterFrameRate = unsafe { mem::zeroed() };
    frame_rate.framerate = fps;

    VaapiRateControlParameters {
        rate_control,
        hrd,
        frame_rate,
    }
}

pub struct VaapiEncoder {
    input_surfaces: Vec<VaSurface>,
    recon_surfaces: Vec<VaSurface>,
    coded_buffers: Vec<VaBuffer>,
    dmabuf_input_surface: Option<VaSurface>,
    context: VaContext,
    _config: VaConfig,
    display: Rc<VaDisplay>,
    current_input_surface: usize,
    current_recon_surface: usize,
    current_coded_buffer: usize,
    references: VaapiReferenceState,
    reference_mode: VaapiReferenceMode,
    cached_sps_pps: Option<Vec<u8>>,
    packed_headers: u32,
    width: u32,
    height: u32,
    bitrate: u32,
    quality: u8,
    rate_control: H264RateControl,
    fps: u32,
    level_idc: u8,
    frame_count: u64,
    /// frame_count value at the most recent IDR; frame_num counts from it.
    idr_base: u64,
    /// Number of IDR pictures emitted, for idr_pic_id derivation.
    idr_count: u64,
    force_idr: bool,
    nv12_format: VAImageFormat,
    profile: VAProfile,
}

impl VaapiEncoder {
    pub fn new(
        width: u32,
        height: u32,
        bitrate: u32,
        fps: u32,
        quality: u8,
        rate_control: H264RateControl,
    ) -> Result<Self> {
        Self::new_with_reference_mode(
            width,
            height,
            bitrate,
            fps,
            quality,
            rate_control,
            VaapiReferenceMode::Single,
        )
    }

    fn new_with_reference_mode(
        width: u32,
        height: u32,
        bitrate: u32,
        fps: u32,
        quality: u8,
        rate_control: H264RateControl,
        reference_mode: VaapiReferenceMode,
    ) -> Result<Self> {
        if width == 0 || height == 0 || !width.is_multiple_of(2) || !height.is_multiple_of(2) {
            bail!("dimensions must be non-zero and even: {}x{}", width, height);
        }

        let device_path = find_vaapi_device()?;

        tracing::info!(
            "Initializing VA-API encoder: {}x{}, device={}",
            width,
            height,
            device_path.display()
        );

        let display = VaDisplay::open_drm(&device_path)
            .map_err(|e| anyhow::anyhow!("Failed to open VA display: {:?}", e))?;

        let driver = display
            .query_vendor_string()
            .unwrap_or_else(|_| "unknown".into());
        tracing::info!("VA-API vendor: {}", driver);

        let profiles = display
            .query_config_profiles()
            .map_err(|e| anyhow::anyhow!("Failed to query profiles: {}", e))?;

        let h264_profile = if profiles.contains(&sys::VA_PROFILE_H264_HIGH) {
            sys::VA_PROFILE_H264_HIGH
        } else if profiles.contains(&sys::VA_PROFILE_H264_MAIN) {
            sys::VA_PROFILE_H264_MAIN
        } else {
            bail!("H.264 encode not supported by VA-API driver");
        };

        let entrypoints = display
            .query_config_entrypoints(h264_profile)
            .map_err(|e| anyhow::anyhow!("Failed to query entrypoints: {}", e))?;

        if !entrypoints.contains(&sys::VA_ENTRYPOINT_ENC_SLICE) {
            bail!("H.264 encode entrypoint not supported");
        }

        let supported_packed_headers = display
            .get_config_attrib(
                h264_profile,
                sys::VA_ENTRYPOINT_ENC_SLICE,
                va::VAConfigAttribType_VAConfigAttribEncPackedHeaders,
            )
            .context("Failed to query VA-API packed-header support")?;
        let packed_header_attrib = packed_header_config_attrib(supported_packed_headers);
        let packed_headers = packed_header_attrib.map_or(0, |attrib| attrib.value);

        let rc_policy = vaapi_rate_control_policy(bitrate, quality, rate_control);
        let level_idc = h264_level(
            h264_profile,
            rc_policy.bits_per_second,
            width,
            height,
            fps,
            reference_mode.max_num_ref_frames(),
        )
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no supported H.264 level for {}x{}@{}fps, {}bps, {} references",
                width,
                height,
                fps,
                rc_policy.bits_per_second,
                reference_mode.max_num_ref_frames()
            )
        })?;
        let mut config_attribs = vec![
            VAConfigAttrib {
                type_: sys::VA_CONFIG_ATTRIB_RT_FORMAT,
                value: VA_RT_FORMAT_YUV420,
            },
            VAConfigAttrib {
                type_: sys::VA_CONFIG_ATTRIB_RATE_CONTROL,
                value: rc_policy.config_mode,
            },
        ];
        if let Some(attrib) = packed_header_attrib {
            config_attribs.push(attrib);
        }
        let config = display
            .create_config(h264_profile, sys::VA_ENTRYPOINT_ENC_SLICE, &config_attribs)
            .map_err(|e| anyhow::anyhow!("Failed to create config: {}", e))?;

        let input_surfaces = display
            .create_surfaces(
                VA_RT_FORMAT_YUV420,
                Some(sys::fourcc(b"NV12")),
                width,
                height,
                Some(va::VA_SURFACE_ATTRIB_USAGE_HINT_ENCODER),
                INPUT_SURFACE_POOL_SIZE,
            )
            .map_err(|e| anyhow::anyhow!("Failed to create input surfaces: {}", e))?;

        let recon_surface_count = reference_mode.recon_surface_pool_size();
        let recon_surfaces = display
            .create_surfaces(
                VA_RT_FORMAT_YUV420,
                Some(sys::fourcc(b"NV12")),
                width,
                height,
                Some(va::VA_SURFACE_ATTRIB_USAGE_HINT_ENCODER),
                recon_surface_count,
            )
            .map_err(|e| anyhow::anyhow!("Failed to create recon surfaces: {}", e))?;

        let context = display
            .create_context(&config, width, height, &mut [])
            .map_err(|e| anyhow::anyhow!("Failed to create context: {}", e))?;

        let image_formats = display
            .query_image_formats()
            .map_err(|e| anyhow::anyhow!("Failed to query image formats: {}", e))?;

        let nv12_format = image_formats
            .iter()
            .find(|f| f.fourcc == sys::fourcc(b"NV12"))
            .copied()
            .ok_or_else(|| anyhow::anyhow!("NV12 format not supported"))?;

        let coded_buffer_size = ((width * height * 3) / 2) as usize;
        let mut coded_buffers = Vec::with_capacity(CODED_BUFFER_COUNT);
        for i in 0..CODED_BUFFER_COUNT {
            let buf = context
                .create_coded_buffer(coded_buffer_size)
                .map_err(|e| anyhow::anyhow!("Failed to create coded buffer {}: {}", i, e))?;
            coded_buffers.push(buf);
        }

        tracing::info!(
            profile = ?h264_profile,
            rate_control = ?rate_control,
            quality = quality,
            packed_headers,
            "VA-API encoder ready: {}x{}, {}kbps, IDR every {} frames",
            width, height, bitrate / 1000, IDR_INTERVAL,
        );

        Ok(Self {
            input_surfaces,
            recon_surfaces,
            coded_buffers,
            dmabuf_input_surface: None,
            context,
            _config: config,
            display,
            current_input_surface: 0,
            current_recon_surface: 0,
            current_coded_buffer: 0,
            references: VaapiReferenceState::default(),
            reference_mode,
            cached_sps_pps: None,
            packed_headers,
            width,
            height,
            bitrate,
            quality,
            rate_control,
            fps,
            level_idc,
            frame_count: 0,
            idr_base: 0,
            idr_count: 0,
            force_idr: true,
            nv12_format,
            profile: h264_profile,
        })
    }

    /// Force the next encoded frame to be an IDR (used after error recovery).
    pub fn force_idr(&mut self) {
        self.force_idr = true;
    }

    /// Create the packed SPS/PPS pair selected during config creation.
    fn create_packed_sequence_buffers(&mut self) -> Result<[VaBuffer; 2]> {
        let sps_pps = self.generate_sps_pps();
        let param = va::VAEncPackedHeaderParameterBuffer {
            type_: va::VAEncPackedHeaderType_VAEncPackedHeaderSequence,
            bit_length: (sps_pps.len() * 8) as u32,
            has_emulation_bytes: 1,
            va_reserved: [0; 4],
        };
        let param_buffer = self
            .context
            .create_buffer(
                va::VABufferType_VAEncPackedHeaderParameterBufferType,
                param,
                "vaCreateBuffer (packed sequence param)",
            )
            .context("Failed to create packed sequence param buffer")?;
        let data_buffer = self
            .context
            .create_data_buffer(
                va::VABufferType_VAEncPackedHeaderDataBufferType,
                &sps_pps,
                "vaCreateBuffer (packed sequence data)",
            )
            .context("Failed to create packed sequence data buffer")?;
        self.cached_sps_pps = Some(sps_pps);
        Ok([param_buffer, data_buffer])
    }

    /// Packed slice header for drivers that require it. Mesa radeonsi parses
    /// this header for nal_ref_idc/nal_unit_type and the slice fields the
    /// firmware writes into the final slice NAL; without it the emitted
    /// slice carries a zeroed NAL header byte and never decodes (#21).
    fn create_packed_slice_buffers(
        &mut self,
        is_idr: bool,
        frame_num: u16,
        idr_pic_id: u16,
        poc: i32,
    ) -> Result<[VaBuffer; 2]> {
        let (header, bit_length) = generate_packed_slice_header(is_idr, frame_num, idr_pic_id, poc);
        let param = va::VAEncPackedHeaderParameterBuffer {
            type_: va::VAEncPackedHeaderType_VAEncPackedHeaderSlice,
            bit_length,
            has_emulation_bytes: 1,
            va_reserved: [0; 4],
        };
        let param_buffer = self
            .context
            .create_buffer(
                va::VABufferType_VAEncPackedHeaderParameterBufferType,
                param,
                "vaCreateBuffer (packed slice param)",
            )
            .context("Failed to create packed slice param buffer")?;
        let data_buffer = self
            .context
            .create_data_buffer(
                va::VABufferType_VAEncPackedHeaderDataBufferType,
                &header,
                "vaCreateBuffer (packed slice data)",
            )
            .context("Failed to create packed slice data buffer")?;
        Ok([param_buffer, data_buffer])
    }

    fn is_idr_frame(&self) -> bool {
        self.force_idr
            || (self.reference_mode.uses_periodic_idr()
                && self.frame_count.is_multiple_of(IDR_INTERVAL as u64))
    }

    pub fn encode(&mut self, bgra: &[u8], stride: usize) -> Result<Vec<u8>> {
        let role = VaapiEncodeRole::Generic;
        let is_idr = self.is_idr_frame();
        let input_idx = self.current_input_surface;
        self.current_input_surface = (self.current_input_surface + 1) % self.input_surfaces.len();

        let recon_idx = self.current_recon_surface;
        self.current_recon_surface = (self.current_recon_surface + 1) % self.recon_surfaces.len();

        let coded_idx = self.current_coded_buffer;
        self.current_coded_buffer = (self.current_coded_buffer + 1) % self.coded_buffers.len();

        // Convert BGRA to NV12 and upload to surface via VA image,
        // using the image's actual pitches/offsets (not assuming stride == width).
        {
            let mut image = VaImageMapping::create_from(
                &self.input_surfaces[input_idx],
                self.nv12_format,
                self.width,
                self.height,
            )
            .context("Failed to create VA image")?;

            let (y_pitch, uv_pitch, y_offset, uv_offset) = {
                let i = image.image();
                (
                    i.pitches[0] as usize,
                    i.pitches[1] as usize,
                    i.offsets[0] as usize,
                    i.offsets[1] as usize,
                )
            };

            Self::bgra_to_nv12(
                bgra,
                image.data_mut(),
                self.width as usize,
                self.height as usize,
                stride,
                y_offset,
                y_pitch,
                uv_offset,
                uv_pitch,
            );
        }

        // Build encoding parameters
        let mb_width = self.width.div_ceil(16);
        let mb_height = self.height.div_ceil(16);
        let num_macroblocks = mb_width * mb_height;
        if is_idr {
            self.idr_base = self.frame_count;
        }
        let (frame_num, poc) = slice_numbering(self.frame_count - self.idr_base);
        let idr_pic_id = h264_idr_pic_id(self.idr_count);
        if is_idr {
            self.idr_count += 1;
        }
        let packed_header_plan = packed_header_plan(self.packed_headers, is_idr);

        let mut picture_buffers = Vec::new();

        if is_idr {
            self.references.clear();

            let seq_param = self.build_sequence_params(mb_width as u16, mb_height as u16);
            let seq_buffer = self
                .context
                .create_buffer(
                    va::VABufferType_VAEncSequenceParameterBufferType,
                    seq_param,
                    "vaCreateBuffer (H.264 sequence)",
                )
                .context("Failed to create seq buffer")?;
            picture_buffers.push(seq_buffer);

            picture_buffers.extend(self.create_rate_control_buffers()?);
            if packed_header_plan.sequence {
                picture_buffers.extend(self.create_packed_sequence_buffers()?);
            }
        }

        let pic_param = self.build_picture_params(
            self.recon_surfaces[recon_idx].id(),
            self.coded_buffers[coded_idx].id(),
            is_idr,
            frame_num,
            poc,
            role,
        );
        let pic_buffer = self
            .context
            .create_buffer(
                va::VABufferType_VAEncPictureParameterBufferType,
                pic_param,
                "vaCreateBuffer (H.264 picture)",
            )
            .context("Failed to create pic buffer")?;
        picture_buffers.push(pic_buffer);

        let slice_param = self.build_slice_params(num_macroblocks, is_idr, idr_pic_id, poc, role);
        let slice_buffer = self
            .context
            .create_buffer(
                va::VABufferType_VAEncSliceParameterBufferType,
                slice_param,
                "vaCreateBuffer (H.264 slice)",
            )
            .context("Failed to create slice buffer")?;
        picture_buffers.push(slice_buffer);
        if packed_header_plan.slice {
            picture_buffers
                .extend(self.create_packed_slice_buffers(is_idr, frame_num, idr_pic_id, poc)?);
        }

        self.context
            .render_picture(self.input_surfaces[input_idx].id(), &picture_buffers)?;

        // Update DPB
        self.references.record(
            role,
            DpbEntry {
                surface_id: self.recon_surfaces[recon_idx].id(),
                frame_num,
                poc,
            },
        );

        // Read encoded bitstream
        let mut data = self.coded_buffers[coded_idx].read_coded()?;

        // SPS/PPS handling: extract from IDR output or generate if missing
        if is_idr {
            if let Some(sps_pps) = super::extract_sps_pps(&data) {
                self.cached_sps_pps = Some(sps_pps);
            } else {
                // VA-API driver did not include SPS/PPS; synthesize it here.
                tracing::trace!("VA-API IDR missing SPS/PPS, synthesizing parameter sets");
                let sps_pps = self.generate_sps_pps();
                // Prepend to IDR frame
                let mut combined = Vec::with_capacity(sps_pps.len() + data.len());
                combined.extend_from_slice(&sps_pps);
                combined.extend_from_slice(&data);
                data = combined;
                self.cached_sps_pps = Some(sps_pps);
            }
        } else if let Some(ref sps_pps) = self.cached_sps_pps {
            let mut combined = Vec::with_capacity(sps_pps.len() + data.len());
            combined.extend_from_slice(sps_pps);
            combined.extend_from_slice(&data);
            data = combined;
        }

        if self.force_idr {
            self.force_idr = false;
        }
        self.frame_count += 1;

        Ok(data)
    }

    /// Encode from an NV12 DMA-BUF (zero-copy path).
    ///
    /// Imports the NV12 DMA-BUF as an encoder input surface (cached after first call),
    /// then encodes using the same pipeline as the BGRA path but skipping the color conversion.
    #[allow(clippy::too_many_arguments)]
    pub fn encode_dmabuf(
        &mut self,
        nv12_fd: std::os::unix::io::RawFd,
        width: u32,
        height: u32,
        stride: u32,
        offset: u32,
        modifier: u64,
        uv_stride: u32,
        uv_offset: u32,
    ) -> Result<Vec<u8>> {
        if self.dmabuf_input_surface.is_none() {
            let mut desc: VADRMPRIMESurfaceDescriptor = unsafe { mem::zeroed() };
            desc.fourcc = sys::fourcc(b"NV12");
            desc.width = width;
            desc.height = height;
            desc.num_objects = 1;
            desc.objects[0].fd = nv12_fd;
            let y_end = offset + height * stride;
            let uv_end = uv_offset + (height / 2) * uv_stride;
            desc.objects[0].size = y_end.max(uv_end);
            desc.objects[0].drm_format_modifier = modifier;
            desc.num_layers = 2;
            desc.layers[0].drm_format = sys::fourcc(b"NV12");
            desc.layers[0].num_planes = 1;
            desc.layers[0].object_index[0] = 0;
            desc.layers[0].offset[0] = offset;
            desc.layers[0].pitch[0] = stride;
            desc.layers[1].drm_format = sys::fourcc(b"NV12");
            desc.layers[1].num_planes = 1;
            desc.layers[1].object_index[0] = 0;
            desc.layers[1].offset[0] = uv_offset;
            desc.layers[1].pitch[0] = uv_stride;

            let surface = self
                .display
                .import_prime_surface(VA_RT_FORMAT_YUV420, width, height, &mut desc)
                .map_err(|e| anyhow::anyhow!("Failed to import NV12 DMA-BUF surface: {}", e))?;
            tracing::trace!(
                surface_id = surface.id(),
                "Encoder: imported NV12 DMA-BUF surface"
            );
            self.dmabuf_input_surface = Some(surface);
        }

        let dmabuf_surface_id = self
            .dmabuf_input_surface
            .as_ref()
            .expect("dmabuf_input_surface must be set before encode")
            .id();
        let role = VaapiEncodeRole::Generic;
        let is_idr = self.is_idr_frame();

        let recon_idx = self.current_recon_surface;
        self.current_recon_surface = (self.current_recon_surface + 1) % self.recon_surfaces.len();

        let coded_idx = self.current_coded_buffer;
        self.current_coded_buffer = (self.current_coded_buffer + 1) % self.coded_buffers.len();

        let mb_width = self.width.div_ceil(16);
        let mb_height = self.height.div_ceil(16);
        let num_macroblocks = mb_width * mb_height;
        if is_idr {
            self.idr_base = self.frame_count;
        }
        let (frame_num, poc) = slice_numbering(self.frame_count - self.idr_base);
        let idr_pic_id = h264_idr_pic_id(self.idr_count);
        if is_idr {
            self.idr_count += 1;
        }
        let packed_header_plan = packed_header_plan(self.packed_headers, is_idr);

        let mut picture_buffers = Vec::new();

        if is_idr {
            self.references.clear();
            let seq_param = self.build_sequence_params(mb_width as u16, mb_height as u16);
            let seq_buffer = self
                .context
                .create_buffer(
                    va::VABufferType_VAEncSequenceParameterBufferType,
                    seq_param,
                    "vaCreateBuffer (H.264 sequence)",
                )
                .context("Failed to create seq buffer")?;
            picture_buffers.push(seq_buffer);

            picture_buffers.extend(self.create_rate_control_buffers()?);
            if packed_header_plan.sequence {
                picture_buffers.extend(self.create_packed_sequence_buffers()?);
            }
        }

        let pic_param = self.build_picture_params(
            self.recon_surfaces[recon_idx].id(),
            self.coded_buffers[coded_idx].id(),
            is_idr,
            frame_num,
            poc,
            role,
        );
        let pic_buffer = self
            .context
            .create_buffer(
                va::VABufferType_VAEncPictureParameterBufferType,
                pic_param,
                "vaCreateBuffer (H.264 picture)",
            )
            .context("Failed to create pic buffer")?;
        picture_buffers.push(pic_buffer);

        let slice_param = self.build_slice_params(num_macroblocks, is_idr, idr_pic_id, poc, role);
        let slice_buffer = self
            .context
            .create_buffer(
                va::VABufferType_VAEncSliceParameterBufferType,
                slice_param,
                "vaCreateBuffer (H.264 slice)",
            )
            .context("Failed to create slice buffer")?;
        picture_buffers.push(slice_buffer);
        if packed_header_plan.slice {
            picture_buffers
                .extend(self.create_packed_slice_buffers(is_idr, frame_num, idr_pic_id, poc)?);
        }

        self.context
            .render_picture(dmabuf_surface_id, &picture_buffers)
            .map_err(|e| anyhow::anyhow!("VA-API render failed (dmabuf): {}", e))?;

        self.references.record(
            role,
            DpbEntry {
                surface_id: self.recon_surfaces[recon_idx].id(),
                frame_num,
                poc,
            },
        );

        let mut data = self.coded_buffers[coded_idx].read_coded()?;

        // SPS/PPS handling (same as encode())
        if is_idr {
            if let Some(sps_pps) = super::extract_sps_pps(&data) {
                self.cached_sps_pps = Some(sps_pps);
            } else {
                tracing::trace!("VA-API IDR missing SPS/PPS, synthesizing parameter sets");
                let sps_pps = self.generate_sps_pps();
                let mut combined = Vec::with_capacity(sps_pps.len() + data.len());
                combined.extend_from_slice(&sps_pps);
                combined.extend_from_slice(&data);
                data = combined;
                self.cached_sps_pps = Some(sps_pps);
            }
        } else if let Some(ref sps_pps) = self.cached_sps_pps {
            let mut combined = Vec::with_capacity(sps_pps.len() + data.len());
            combined.extend_from_slice(sps_pps);
            combined.extend_from_slice(&data);
            data = combined;
        }

        if self.force_idr {
            self.force_idr = false;
        }
        self.frame_count += 1;

        Ok(data)
    }

    /// Convert BGRA to NV12 (BT.709 full range) directly into VA image buffer,
    /// respecting the image's pitch and plane offsets.
    #[allow(clippy::too_many_arguments)]
    fn bgra_to_nv12(
        bgra: &[u8],
        dst: &mut [u8],
        w: usize,
        h: usize,
        src_stride: usize,
        y_offset: usize,
        y_pitch: usize,
        uv_offset: usize,
        uv_pitch: usize,
    ) {
        if bgra.len() < h * src_stride {
            tracing::warn!(
                bgra_len = bgra.len(),
                expected = h * src_stride,
                "BGRA buffer too small, skipping conversion"
            );
            return;
        }
        // Y plane (BT.709 full range).
        for row in 0..h {
            let dst_start = y_offset + row * y_pitch;
            for col in 0..w {
                let idx = row * src_stride + col * 4;
                let b = bgra[idx] as i32;
                let g = bgra[idx + 1] as i32;
                let r = bgra[idx + 2] as i32;
                let y = (54 * r + 183 * g + 18 * b) >> 8;
                dst[dst_start + col] = y.clamp(0, 255) as u8;
            }
        }

        // UV plane (NV12 interleaved, BT.709 full range, half resolution)
        for row in 0..(h / 2) {
            let dst_start = uv_offset + row * uv_pitch;
            for col in 0..(w / 2) {
                let src_row = row * 2;
                let src_col = col * 2;

                let mut r_sum = 0i32;
                let mut g_sum = 0i32;
                let mut b_sum = 0i32;

                for dy in 0..2 {
                    for dx in 0..2 {
                        let idx = (src_row + dy) * src_stride + (src_col + dx) * 4;
                        b_sum += bgra[idx] as i32;
                        g_sum += bgra[idx + 1] as i32;
                        r_sum += bgra[idx + 2] as i32;
                    }
                }

                let r = r_sum / 4;
                let g = g_sum / 4;
                let b = b_sum / 4;

                let u = ((-29 * r - 99 * g + 128 * b) >> 8) + 128;
                let v = ((128 * r - 116 * g - 12 * b) >> 8) + 128;

                dst[dst_start + col * 2] = u.clamp(0, 255) as u8;
                dst[dst_start + col * 2 + 1] = v.clamp(0, 255) as u8;
            }
        }
    }

    fn build_sequence_params(
        &self,
        mb_width: u16,
        mb_height: u16,
    ) -> va::VAEncSequenceParameterBufferH264 {
        let mut params: va::VAEncSequenceParameterBufferH264 = unsafe { mem::zeroed() };
        params.seq_parameter_set_id = 0;
        params.level_idc = self.level_idc;
        params.intra_period = IDR_INTERVAL;
        params.intra_idr_period = IDR_INTERVAL;
        params.ip_period = 1;
        params.bits_per_second = self.bitrate;
        params.max_num_ref_frames = self.reference_mode.max_num_ref_frames();
        params.picture_width_in_mbs = mb_width;
        params.picture_height_in_mbs = mb_height;
        params.seq_fields = va::_VAEncSequenceParameterBufferH264__bindgen_ty_1 {
            value: h264_seq_fields_value(),
        };
        params.bit_depth_luma_minus8 = 0;
        params.bit_depth_chroma_minus8 = 0;
        params.num_ref_frames_in_pic_order_cnt_cycle = 0;
        params.offset_for_non_ref_pic = 0;
        params.offset_for_top_to_bottom_field = 0;
        params.offset_for_ref_frame = [0; 256];
        params.frame_cropping_flag = 0;
        params.vui_parameters_present_flag = 1;
        params.vui_fields = va::_VAEncSequenceParameterBufferH264__bindgen_ty_2 {
            value: h264_vui_fields_value(),
        };
        params.aspect_ratio_idc = 0;
        params.sar_width = 1;
        params.sar_height = 1;
        params.num_units_in_tick = 1;
        params.time_scale = self.fps * 2;
        params
    }

    fn build_picture_params(
        &self,
        recon_surface_id: u32,
        coded_buf_id: u32,
        is_idr: bool,
        frame_num: u16,
        poc: i32,
        role: VaapiEncodeRole,
    ) -> va::VAEncPictureParameterBufferH264 {
        let curr_pic = picture_h264(
            recon_surface_id,
            frame_num as u32,
            VA_PICTURE_H264_SHORT_TERM_REFERENCE,
            poc,
            poc,
        );

        let mut reference_frames: [va::VAPictureH264; 16] = std::array::from_fn(|_| {
            picture_h264(VA_INVALID_SURFACE, 0, VA_PICTURE_H264_INVALID, 0, 0)
        });

        let refs = if is_idr {
            Vec::new()
        } else {
            self.references.reference_frames(self.reference_mode, role)
        };
        for (index, entry) in refs.iter().take(reference_frames.len()).enumerate() {
            reference_frames[index] = picture_h264(
                entry.surface_id,
                entry.frame_num as u32,
                VA_PICTURE_H264_SHORT_TERM_REFERENCE,
                entry.poc,
                entry.poc,
            );
        }

        let transform_8x8 = if self.profile == sys::VA_PROFILE_H264_HIGH {
            1
        } else {
            0
        };

        let mut params: va::VAEncPictureParameterBufferH264 = unsafe { mem::zeroed() };
        params.CurrPic = curr_pic;
        params.ReferenceFrames = reference_frames;
        params.coded_buf = coded_buf_id;
        params.pic_parameter_set_id = 0;
        params.seq_parameter_set_id = 0;
        params.last_picture = 0;
        params.frame_num = frame_num;
        params.pic_init_qp =
            vaapi_rate_control_policy(self.bitrate, self.quality, self.rate_control).pic_init_qp;
        params.num_ref_idx_l0_active_minus1 = 0;
        params.num_ref_idx_l1_active_minus1 = 0;
        params.chroma_qp_index_offset = 0;
        params.second_chroma_qp_index_offset = 0;
        params.pic_fields = va::_VAEncPictureParameterBufferH264__bindgen_ty_1 {
            value: h264_pic_fields_value(is_idr, transform_8x8),
        };
        params
    }

    fn build_slice_params(
        &self,
        num_macroblocks: u32,
        is_idr: bool,
        idr_pic_id: u16,
        poc: i32,
        role: VaapiEncodeRole,
    ) -> va::VAEncSliceParameterBufferH264 {
        let slice_type = if is_idr { SLICE_TYPE_I } else { SLICE_TYPE_P };

        let mut ref_pic_list_0: [va::VAPictureH264; 32] = std::array::from_fn(|_| {
            picture_h264(VA_INVALID_SURFACE, 0, VA_PICTURE_H264_INVALID, 0, 0)
        });
        let ref_pic_list_1: [va::VAPictureH264; 32] = std::array::from_fn(|_| {
            picture_h264(VA_INVALID_SURFACE, 0, VA_PICTURE_H264_INVALID, 0, 0)
        });

        let primary_ref = if is_idr {
            None
        } else {
            self.references.primary_reference(role)
        };
        let (num_ref_override, num_ref_l0) = if let Some(entry) = primary_ref {
            ref_pic_list_0[0] = picture_h264(
                entry.surface_id,
                entry.frame_num as u32,
                VA_PICTURE_H264_SHORT_TERM_REFERENCE,
                entry.poc,
                entry.poc,
            );
            (1u8, 0u8)
        } else {
            (0u8, 0u8)
        };

        let mut params: va::VAEncSliceParameterBufferH264 = unsafe { mem::zeroed() };
        params.macroblock_address = 0;
        params.num_macroblocks = num_macroblocks;
        params.macroblock_info = VA_INVALID_ID;
        params.slice_type = slice_type;
        params.pic_parameter_set_id = 0;
        params.idr_pic_id = idr_pic_id;
        params.pic_order_cnt_lsb = poc as u16;
        params.delta_pic_order_cnt_bottom = 0;
        params.delta_pic_order_cnt = [0, 0];
        params.direct_spatial_mv_pred_flag = 0;
        params.num_ref_idx_active_override_flag = num_ref_override;
        params.num_ref_idx_l0_active_minus1 = num_ref_l0;
        params.num_ref_idx_l1_active_minus1 = 0;
        params.RefPicList0 = ref_pic_list_0;
        params.RefPicList1 = ref_pic_list_1;
        params.luma_log2_weight_denom = 0;
        params.chroma_log2_weight_denom = 0;
        params.luma_weight_l0_flag = 0;
        params.chroma_weight_l0_flag = 0;
        params.luma_weight_l1_flag = 0;
        params.chroma_weight_l1_flag = 0;
        params.cabac_init_idc = 0;
        params.slice_qp_delta = 0;
        params.disable_deblocking_filter_idc = 0;
        params.slice_alpha_c0_offset_div2 = 0;
        params.slice_beta_offset_div2 = 0;
        params
    }

    fn create_rate_control_buffers(&self) -> Result<Vec<VaBuffer>> {
        let mut buffers = Vec::with_capacity(3);

        let policy = vaapi_rate_control_policy(self.bitrate, self.quality, self.rate_control);
        let parameters = vaapi_rate_control_parameters(policy, self.fps);
        buffers.push(
            self.context
                .create_misc_buffer(
                    va::VAEncMiscParameterType_VAEncMiscParameterTypeRateControl,
                    parameters.rate_control,
                    "vaCreateBuffer (rate control)",
                )
                .context("Failed to create rate control buffer")?,
        );

        if let Some(hrd) = parameters.hrd {
            buffers.push(
                self.context
                    .create_misc_buffer(
                        va::VAEncMiscParameterType_VAEncMiscParameterTypeHRD,
                        hrd,
                        "vaCreateBuffer (HRD)",
                    )
                    .context("Failed to create HRD buffer")?,
            );
        }

        buffers.push(
            self.context
                .create_misc_buffer(
                    va::VAEncMiscParameterType_VAEncMiscParameterTypeFrameRate,
                    parameters.frame_rate,
                    "vaCreateBuffer (frame rate)",
                )
                .context("Failed to create frame rate buffer")?,
        );

        Ok(buffers)
    }

    /// Generate SPS and PPS NAL units matching our encoder configuration.
    /// Used when the VA-API driver doesn't include them in the coded output.
    fn generate_sps_pps(&self) -> Vec<u8> {
        let profile_idc: u32 = match self.profile {
            sys::VA_PROFILE_H264_HIGH => 100,
            sys::VA_PROFILE_H264_MAIN => 77,
            _ => 100,
        };
        let is_high_profile = profile_idc >= 100;

        let mb_width = self.width.div_ceil(16);
        let mb_height = self.height.div_ceil(16);
        let coded_width = mb_width * 16;
        let coded_height = mb_height * 16;
        let need_crop = coded_width != self.width || coded_height != self.height;
        let crop_right = if coded_width != self.width {
            (coded_width - self.width) / 2
        } else {
            0
        };
        let crop_bottom = if coded_height != self.height {
            (coded_height - self.height) / 2
        } else {
            0
        };

        let mut buf = Vec::with_capacity(64);

        buf.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
        let mut bs = BitWriter::new();
        bs.write_bits(0, 1);
        bs.write_bits(3, 2);
        bs.write_bits(7, 5);

        bs.write_bits(profile_idc, 8);
        bs.write_bits(0, 8);
        bs.write_bits(self.level_idc as u32, 8);
        bs.write_ue(0);

        if is_high_profile {
            bs.write_ue(1); // chroma_format_idc
            bs.write_ue(0); // bit_depth_luma_minus8
            bs.write_ue(0); // bit_depth_chroma_minus8
            bs.write_bits(0, 1); // qpprime_y_zero_transform_bypass_flag
            bs.write_bits(0, 1); // seq_scaling_matrix_present_flag
        }

        bs.write_ue(4); // log2_max_frame_num_minus4
        bs.write_ue(0); // pic_order_cnt_type
        bs.write_ue(4); // log2_max_pic_order_cnt_lsb_minus4
        let max_num_ref_frames = self.reference_mode.max_num_ref_frames();
        bs.write_ue(max_num_ref_frames); // max_num_ref_frames
        bs.write_bits(0, 1); // gaps_in_frame_num_value_allowed_flag
        bs.write_ue(mb_width - 1);
        bs.write_ue(mb_height - 1);
        bs.write_bits(1, 1); // frame_mbs_only_flag
        bs.write_bits(1, 1); // direct_8x8_inference_flag

        if need_crop {
            bs.write_bits(1, 1);
            bs.write_ue(0);
            bs.write_ue(crop_right);
            bs.write_ue(0);
            bs.write_ue(crop_bottom);
        } else {
            bs.write_bits(0, 1);
        }

        bs.write_bits(1, 1); // vui_parameters_present_flag
        bs.write_bits(0, 1); // aspect_ratio_info_present_flag
        bs.write_bits(0, 1); // overscan_info_present_flag
        bs.write_bits(1, 1); // video_signal_type_present_flag
        bs.write_bits(5, 3); // video_format: unspecified
        bs.write_bits(1, 1); // video_full_range_flag
        bs.write_bits(1, 1); // colour_description_present_flag
        bs.write_bits(1, 8); // colour_primaries: BT.709
        bs.write_bits(1, 8); // transfer_characteristics: BT.709
        bs.write_bits(1, 8); // matrix_coefficients: BT.709
        bs.write_bits(0, 1); // chroma_loc_info_present_flag
        bs.write_bits(1, 1); // timing_info_present_flag
        bs.write_bits(1, 32); // num_units_in_tick
        bs.write_bits(self.fps * 2, 32);
        bs.write_bits(0, 1); // fixed_frame_rate_flag
        bs.write_bits(0, 1); // nal_hrd_parameters_present_flag
        bs.write_bits(0, 1); // vcl_hrd_parameters_present_flag
        bs.write_bits(0, 1); // pic_struct_present_flag
        bs.write_bits(1, 1); // bitstream_restriction_flag
        bs.write_bits(1, 1); // motion_vectors_over_pic_boundaries_flag
        bs.write_ue(0);
        bs.write_ue(0);
        bs.write_ue(16);
        bs.write_ue(16);
        bs.write_ue(0); // max_num_reorder_frames
        bs.write_ue(max_num_ref_frames); // max_dec_frame_buffering

        bs.write_rbsp_trailing_bits();
        buf.extend_from_slice(&bs.finish_with_emulation_prevention());

        buf.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
        let mut bs = BitWriter::new();
        bs.write_bits(0, 1);
        bs.write_bits(3, 2);
        bs.write_bits(8, 5);

        bs.write_ue(0); // pic_parameter_set_id
        bs.write_ue(0); // seq_parameter_set_id
        bs.write_bits(1, 1); // entropy_coding_mode_flag (CABAC)
        bs.write_bits(0, 1); // bottom_field_pic_order_in_frame_present_flag
        bs.write_ue(0); // num_slice_groups_minus1
        bs.write_ue(0); // num_ref_idx_l0_default_active_minus1
        bs.write_ue(0); // num_ref_idx_l1_default_active_minus1
        bs.write_bits(0, 1); // weighted_pred_flag
        bs.write_bits(0, 2); // weighted_bipred_idc
        let pic_init_qp = vaapi_rate_control_policy(self.bitrate, self.quality, self.rate_control)
            .pic_init_qp as i32;
        bs.write_se(pic_init_qp - 26); // pic_init_qp_minus26
        bs.write_se(0); // pic_init_qs_minus26
        bs.write_se(0); // chroma_qp_index_offset
        bs.write_bits(1, 1); // deblocking_filter_control_present_flag
        bs.write_bits(0, 1); // constrained_intra_pred_flag
        bs.write_bits(0, 1); // redundant_pic_cnt_present_flag

        if is_high_profile {
            bs.write_bits(1, 1); // transform_8x8_mode_flag
            bs.write_bits(0, 1); // pic_scaling_matrix_present_flag
            bs.write_se(0); // second_chroma_qp_index_offset
        } else {
            bs.write_bits(0, 1); // transform_8x8_mode_flag
            bs.write_bits(0, 1); // pic_scaling_matrix_present_flag
            bs.write_se(0); // second_chroma_qp_index_offset
        }

        bs.write_rbsp_trailing_bits();
        buf.extend_from_slice(&bs.finish_with_emulation_prevention());

        buf
    }
}

fn picture_h264(
    picture_id: u32,
    frame_idx: u32,
    flags: u32,
    top_field_order_cnt: i32,
    bottom_field_order_cnt: i32,
) -> va::VAPictureH264 {
    let mut picture: va::VAPictureH264 = unsafe { mem::zeroed() };
    picture.picture_id = picture_id;
    picture.frame_idx = frame_idx;
    picture.flags = flags;
    picture.TopFieldOrderCnt = top_field_order_cnt;
    picture.BottomFieldOrderCnt = bottom_field_order_cnt;
    picture
}

fn h264_seq_fields_value() -> u32 {
    let chroma_format_idc = 1; // 4:2:0
    let frame_mbs_only_flag = 1;
    let direct_8x8_inference_flag = 1;
    let log2_max_frame_num_minus4 = 4;
    let pic_order_cnt_type = 0;
    let log2_max_pic_order_cnt_lsb_minus4 = 4;

    chroma_format_idc
        | (frame_mbs_only_flag << 2)
        | (direct_8x8_inference_flag << 5)
        | (log2_max_frame_num_minus4 << 6)
        | (pic_order_cnt_type << 10)
        | (log2_max_pic_order_cnt_lsb_minus4 << 12)
}

fn h264_vui_fields_value() -> u32 {
    let timing_info_present_flag = 1;
    let bitstream_restriction_flag = 1;
    let log2_max_mv_length_horizontal = 16;
    let log2_max_mv_length_vertical = 16;
    let motion_vectors_over_pic_boundaries_flag = 1;

    (timing_info_present_flag << 1)
        | (bitstream_restriction_flag << 2)
        | (log2_max_mv_length_horizontal << 3)
        | (log2_max_mv_length_vertical << 8)
        | (motion_vectors_over_pic_boundaries_flag << 15)
}

fn h264_pic_fields_value(is_idr: bool, transform_8x8_mode_flag: u32) -> u32 {
    let idr_pic_flag = u32::from(is_idr);
    let reference_pic_flag = 1;
    let entropy_coding_mode_flag = 1;
    let deblocking_filter_control_present_flag = 1;

    idr_pic_flag
        | (reference_pic_flag << 1)
        | (entropy_coding_mode_flag << 3)
        | (transform_8x8_mode_flag << 8)
        | (deblocking_filter_control_present_flag << 9)
}

/// Bitstream writer for H.264 NAL unit construction.
struct BitWriter {
    data: Vec<u8>,
    current_byte: u8,
    bits_in_byte: u8,
}

impl BitWriter {
    fn new() -> Self {
        Self {
            data: Vec::with_capacity(32),
            current_byte: 0,
            bits_in_byte: 0,
        }
    }

    fn write_bits(&mut self, value: u32, num_bits: u8) {
        for i in (0..num_bits).rev() {
            let bit = (value >> i) & 1;
            self.current_byte = (self.current_byte << 1) | bit as u8;
            self.bits_in_byte += 1;
            if self.bits_in_byte == 8 {
                self.data.push(self.current_byte);
                self.current_byte = 0;
                self.bits_in_byte = 0;
            }
        }
    }

    /// Exp-Golomb unsigned encoding
    fn write_ue(&mut self, value: u32) {
        let value = value + 1;
        let bits = 32 - value.leading_zeros(); // number of significant bits
                                               // Write (bits-1) leading zeros, then the value in `bits` bits
        for _ in 0..(bits - 1) {
            self.write_bits(0, 1);
        }
        self.write_bits(value, bits as u8);
    }

    /// Exp-Golomb signed encoding
    fn write_se(&mut self, value: i32) {
        let mapped = if value > 0 {
            (value * 2 - 1) as u32
        } else if value < 0 {
            (-value * 2) as u32
        } else {
            0
        };
        self.write_ue(mapped);
    }

    fn bit_len(&self) -> u32 {
        (self.data.len() as u32 * 8) + u32::from(self.bits_in_byte)
    }

    fn write_rbsp_trailing_bits(&mut self) {
        self.write_bits(1, 1); // rbsp_stop_one_bit
                               // Pad to byte boundary with zeros
        if self.bits_in_byte > 0 {
            let padding = 8 - self.bits_in_byte;
            self.write_bits(0, padding);
        }
    }

    /// Flush remaining bits and apply emulation prevention (0x03 stuffing).
    fn finish_with_emulation_prevention(mut self) -> Vec<u8> {
        // Flush partial byte
        if self.bits_in_byte > 0 {
            self.current_byte <<= 8 - self.bits_in_byte;
            self.data.push(self.current_byte);
        }

        // Insert emulation prevention bytes: 00 00 {00,01,02,03} → 00 00 03 {00,01,02,03}
        let mut result = Vec::with_capacity(self.data.len() + 4);
        let mut zero_count = 0u32;
        for &byte in &self.data {
            if zero_count >= 2 && byte <= 0x03 {
                result.push(0x03); // emulation prevention byte
                zero_count = 0;
            }
            result.push(byte);
            if byte == 0x00 {
                zero_count += 1;
            } else {
                zero_count = 0;
            }
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tiled_bgra_frame(width: usize, height: usize, phase: usize) -> Vec<u8> {
        let stride = width * 4;
        let mut frame = vec![0u8; stride * height];
        for y in 0..height {
            for x in 0..width {
                let tile_x = x / 48;
                let tile_y = y / 40;
                let seed = tile_x
                    .wrapping_mul(73_856_093)
                    .wrapping_add(tile_y.wrapping_mul(19_349_663))
                    .wrapping_add(phase.wrapping_mul(83_492_791));
                let mut rgb = [
                    24 + (seed & 0x9f) as u8,
                    24 + ((seed >> 8) & 0x9f) as u8,
                    24 + ((seed >> 16) & 0x9f) as u8,
                ];
                if (x + phase * 13).is_multiple_of(17) || (y + phase * 7).is_multiple_of(19) {
                    rgb = [224, 226, 232];
                }
                let offset = y * stride + x * 4;
                frame[offset] = rgb[2];
                frame[offset + 1] = rgb[1];
                frame[offset + 2] = rgb[0];
                frame[offset + 3] = 255;
            }
        }
        frame
    }

    fn expected_luma(bgra: &[u8]) -> Vec<u8> {
        bgra.as_chunks::<4>()
            .0
            .iter()
            .map(|pixel| {
                let b = i32::from(pixel[0]);
                let g = i32::from(pixel[1]);
                let r = i32::from(pixel[2]);
                ((54 * r + 183 * g + 18 * b) >> 8).clamp(0, 255) as u8
            })
            .collect()
    }

    fn decoded_luma_mse(picture: &impl openh264::formats::YUVSource, expected: &[u8]) -> f64 {
        let (width, height) = picture.dimensions();
        let (stride, _, _) = picture.strides();
        let mut squared_error = 0u64;
        for (row, expected_row) in expected.chunks_exact(width).enumerate().take(height) {
            let decoded_row = &picture.y()[row * stride..row * stride + width];
            for (&actual, &target) in decoded_row.iter().zip(expected_row) {
                let error = i32::from(actual) - i32::from(target);
                squared_error += (error * error) as u64;
            }
        }
        squared_error as f64 / (width * height) as f64
    }

    #[test]
    #[ignore = "requires a VA-API H.264 encoder"]
    fn vaapi_vbr_abrupt_scene_recovers_decoded_quality_without_human_viewing() {
        use openh264::decoder::{Decoder, DecoderConfig};
        use openh264::OpenH264API;

        const WIDTH: usize = 1920;
        const HEIGHT: usize = 1200;
        const FPS: u32 = 30;
        const MAX_RECOVERY_FRAMES: usize = 3;
        const RECOVERED_LUMA_MSE: f64 = 50.0;

        let mut encoder = VaapiEncoder::new(
            WIDTH as u32,
            HEIGHT as u32,
            10_000_000,
            FPS,
            23,
            H264RateControl::Vbr,
        )
        .expect("raw VA-API VBR encoder must initialize");
        let api = OpenH264API::from_source();
        let mut decoder = Decoder::with_api_config(api, DecoderConfig::default())
            .expect("OpenH264 decoder must initialize");
        let before = tiled_bgra_frame(WIDTH, HEIGHT, 0);
        let settled = tiled_bgra_frame(WIDTH, HEIGHT, 100);

        for _ in 0..FPS / 2 {
            let packet = encoder
                .encode(&before, WIDTH * 4)
                .expect("pre-transition frame must encode");
            decoder
                .decode(&packet)
                .expect("pre-transition packet must be accepted")
                .expect("pre-transition packet must produce a picture");
        }

        let mut recovered_at = None;
        let mut worst_transition_mse = 0.0f64;
        for frame_index in 0..FPS as usize {
            let changing;
            let current = if frame_index < FPS as usize / 2 {
                changing = tiled_bgra_frame(WIDTH, HEIGHT, frame_index + 1);
                &changing
            } else {
                &settled
            };
            let expected = expected_luma(current);
            let packet = encoder
                .encode(current, WIDTH * 4)
                .expect("post-transition frame must encode");
            let picture = decoder
                .decode(&packet)
                .expect("post-transition packet must be accepted")
                .expect("post-transition packet must produce a picture");
            let mse = decoded_luma_mse(&picture, &expected);
            eprintln!(
                "post-transition frame {frame_index}: bytes={}, luma_mse={mse:.2}",
                packet.len()
            );
            if frame_index < FPS as usize / 2 {
                worst_transition_mse = worst_transition_mse.max(mse);
            } else if mse <= RECOVERED_LUMA_MSE {
                recovered_at.get_or_insert(frame_index - FPS as usize / 2);
            }
        }

        assert!(
            worst_transition_mse <= RECOVERED_LUMA_MSE,
            "decoded luma exceeded MSE {RECOVERED_LUMA_MSE} during the transition: {worst_transition_mse:.2}"
        );
        assert!(
            recovered_at.is_some_and(|frame| frame <= MAX_RECOVERY_FRAMES),
            "decoded luma did not recover below MSE {RECOVERED_LUMA_MSE} within {MAX_RECOVERY_FRAMES} frames; first recovery: {recovered_at:?}"
        );
    }

    struct TestBitReader {
        data: Vec<u8>,
        bit_pos: usize,
    }

    impl TestBitReader {
        fn from_escaped(data: &[u8]) -> Self {
            let mut rbsp = Vec::with_capacity(data.len());
            let mut zero_count = 0;
            for &byte in data {
                if zero_count >= 2 && byte == 0x03 {
                    zero_count = 0;
                    continue;
                }
                rbsp.push(byte);
                if byte == 0 {
                    zero_count += 1;
                } else {
                    zero_count = 0;
                }
            }
            Self {
                data: rbsp,
                bit_pos: 0,
            }
        }

        fn read_bits(&mut self, count: usize) -> u32 {
            let mut value = 0;
            for _ in 0..count {
                let byte = self.data[self.bit_pos / 8];
                let bit = (byte >> (7 - self.bit_pos % 8)) & 1;
                value = (value << 1) | u32::from(bit);
                self.bit_pos += 1;
            }
            value
        }

        fn read_ue(&mut self) -> u32 {
            let mut leading_zeros = 0;
            while self.read_bits(1) == 0 {
                leading_zeros += 1;
            }
            if leading_zeros == 0 {
                return 0;
            }
            (1u32 << leading_zeros) - 1 + self.read_bits(leading_zeros)
        }

        fn read_se(&mut self) -> i32 {
            let code_num = self.read_ue();
            if code_num.is_multiple_of(2) {
                -(code_num as i32 / 2)
            } else {
                (code_num as i32 + 1) / 2
            }
        }
    }

    #[test]
    fn vaapi_packed_header_config_selects_only_supported_headers_we_supply() {
        assert!(packed_header_config_attrib(va::VA_ATTRIB_NOT_SUPPORTED).is_none());
        assert!(packed_header_config_attrib(va::VA_ENC_PACKED_HEADER_NONE).is_none());

        let all_driver_headers = va::VA_ENC_PACKED_HEADER_SEQUENCE
            | va::VA_ENC_PACKED_HEADER_PICTURE
            | va::VA_ENC_PACKED_HEADER_SLICE
            | va::VA_ENC_PACKED_HEADER_MISC;
        let attrib = packed_header_config_attrib(all_driver_headers).unwrap();
        assert_eq!(
            attrib.type_,
            va::VAConfigAttribType_VAConfigAttribEncPackedHeaders
        );
        assert_eq!(attrib.value, VAAPI_DESIRED_PACKED_HEADERS);

        let sequence_only = packed_header_config_attrib(va::VA_ENC_PACKED_HEADER_SEQUENCE).unwrap();
        assert_eq!(sequence_only.value, va::VA_ENC_PACKED_HEADER_SEQUENCE);

        let all_known_flags = va::VA_ENC_PACKED_HEADER_RAW_DATA
            | va::VA_ENC_PACKED_HEADER_MISC
            | va::VA_ENC_PACKED_HEADER_SLICE
            | va::VA_ENC_PACKED_HEADER_PICTURE
            | va::VA_ENC_PACKED_HEADER_SEQUENCE;
        for supported in 0..=all_known_flags {
            let expected = supported & VAAPI_DESIRED_PACKED_HEADERS;
            assert_eq!(
                packed_header_config_attrib(supported).map(|attrib| attrib.value),
                (expected != 0).then_some(expected),
                "supported packed-header mask {supported:#x}"
            );
        }
    }

    #[test]
    fn vaapi_packed_header_plan_matches_idr_and_delta_submissions() {
        assert_eq!(
            packed_header_plan(VAAPI_DESIRED_PACKED_HEADERS, true),
            PackedHeaderPlan {
                sequence: true,
                slice: true,
            }
        );
        assert_eq!(
            packed_header_plan(VAAPI_DESIRED_PACKED_HEADERS, false),
            PackedHeaderPlan {
                sequence: false,
                slice: true,
            }
        );
        assert_eq!(
            packed_header_plan(va::VA_ENC_PACKED_HEADER_NONE, true),
            PackedHeaderPlan {
                sequence: false,
                slice: false,
            }
        );
    }

    #[test]
    fn h264_level_respects_both_throughput_and_frame_size_limits() {
        let high = sys::VA_PROFILE_H264_HIGH;
        assert_eq!(h264_level(high, 10_000_000, 1280, 720, 30, 1), Some(31));
        assert_eq!(h264_level(high, 10_000_000, 1920, 1080, 30, 1), Some(40));
        // 3840x2160 is 32400 macroblocks: past level 5.0's MaxFS of 22080
        // regardless of frame rate.
        assert_eq!(h264_level(high, 10_000_000, 3840, 2160, 30, 1), Some(51));
        assert_eq!(h264_level(high, 10_000_000, 3840, 2160, 60, 1), Some(52));
        // Non-multiple-of-16 sizes round the macroblock count up.
        assert_eq!(h264_level(high, 10_000_000, 1366, 768, 30, 1), Some(32));
    }

    #[test]
    fn h264_level_respects_bitrate_dpb_axis_and_supported_range() {
        let high = sys::VA_PROFILE_H264_HIGH;
        let main = sys::VA_PROFILE_H264_MAIN;

        // Level 4.0 cannot advertise this configured rate; 4.1 can.
        assert_eq!(h264_level(high, 40_000_000, 1920, 1080, 30, 1), Some(41));
        // Main profile has the lower Annex A-2 bitrate factor.
        assert_eq!(h264_level(main, 25_000_000, 1920, 1080, 30, 1), Some(41));
        // Five buffered 1080p pictures exceed level 4.x MaxDpbMbs.
        assert_eq!(h264_level(high, 10_000_000, 1920, 1080, 30, 5), Some(50));
        // Annex A constrains each macroblock axis as well as total MaxFS.
        assert_eq!(h264_level(high, 0, 65_534, 2, 30, 1), None);
        assert_eq!(h264_level(high, 10_000_000, 7680, 4320, 60, 1), None);
        assert_eq!(h264_level(high, 0, 0, 1080, 30, 1), None);
    }

    #[test]
    fn slice_numbering_rebases_at_idr_and_wraps() {
        assert_eq!(slice_numbering(0), (0, 0));
        assert_eq!(slice_numbering(1), (1, 2));
        assert_eq!(slice_numbering(255), (255, 254));
        assert_eq!(slice_numbering(256), (0, 0));
    }

    #[test]
    fn consecutive_idr_pictures_carry_distinct_idr_pic_ids() {
        assert_ne!(h264_idr_pic_id(0), h264_idr_pic_id(1));
        assert_eq!(h264_idr_pic_id(0), h264_idr_pic_id(16));
    }

    #[test]
    fn vaapi_packed_idr_slice_header_matches_va_slice_fields() {
        // An IDR picture carries frame_num 0 (H.264 7.4.3).
        let frame_num = 0;
        let idr_pic_id = h264_idr_pic_id(3);
        let poc = 0;
        let (header, bit_length) = generate_packed_slice_header(true, frame_num, idr_pic_id, poc);
        assert_eq!(&header[..5], &[0, 0, 0, 1, 0x65]);

        let mut bits = TestBitReader::from_escaped(&header[5..]);
        assert_eq!(bits.read_ue(), 0); // first_mb_in_slice
        assert_eq!(bits.read_ue() % 5, u32::from(SLICE_TYPE_I));
        assert_eq!(bits.read_ue(), 0); // pic_parameter_set_id
        assert_eq!(bits.read_bits(8), u32::from(frame_num));
        assert_eq!(bits.read_ue(), u32::from(idr_pic_id));
        assert_eq!(bits.read_bits(8), poc as u32);
        assert_eq!(bits.read_bits(1), 0); // no_output_of_prior_pics_flag
        assert_eq!(bits.read_bits(1), 0); // long_term_reference_flag
        assert_eq!(bits.read_se(), 0); // slice_qp_delta
        assert_eq!(bits.read_ue(), 0); // disable_deblocking_filter_idc
        assert_eq!(bits.read_se(), 0);
        assert_eq!(bits.read_se(), 0);
        assert_eq!(40 + bits.bit_pos as u32, bit_length);
    }

    #[test]
    fn vaapi_packed_p_slice_header_matches_va_slice_fields() {
        let frame_num = 31;
        let poc = 62;
        let (header, bit_length) = generate_packed_slice_header(false, frame_num, 0, poc);
        assert_eq!(&header[..5], &[0, 0, 0, 1, 0x61]);

        let mut bits = TestBitReader::from_escaped(&header[5..]);
        assert_eq!(bits.read_ue(), 0); // first_mb_in_slice
        assert_eq!(bits.read_ue() % 5, u32::from(SLICE_TYPE_P));
        assert_eq!(bits.read_ue(), 0); // pic_parameter_set_id
        assert_eq!(bits.read_bits(8), u32::from(frame_num));
        assert_eq!(bits.read_bits(8), poc as u32);
        assert_eq!(bits.read_bits(1), 0); // num_ref_idx_active_override_flag
        assert_eq!(bits.read_bits(1), 0); // ref_pic_list_modification_flag_l0
        assert_eq!(bits.read_bits(1), 0); // adaptive_ref_pic_marking_mode_flag
        assert_eq!(bits.read_ue(), 0); // cabac_init_idc
        assert_eq!(bits.read_se(), 0); // slice_qp_delta
        assert_eq!(bits.read_ue(), 0); // disable_deblocking_filter_idc
        assert_eq!(bits.read_se(), 0);
        assert_eq!(bits.read_se(), 0);
        assert_eq!(40 + bits.bit_pos as u32, bit_length);
    }

    #[test]
    fn vaapi_packed_slice_headers_preserve_frame_and_poc_boundaries() {
        for is_idr in [false, true] {
            for frame_num in [0u16, 1, 127, 255] {
                for poc in [0i32, 2, 126, 254] {
                    let idr_pic_id = h264_idr_pic_id(u64::from(frame_num));
                    let (header, _) =
                        generate_packed_slice_header(is_idr, frame_num, idr_pic_id, poc);
                    let mut bits = TestBitReader::from_escaped(&header[5..]);
                    assert_eq!(bits.read_ue(), 0);
                    assert_eq!(
                        bits.read_ue() % 5,
                        u32::from(if is_idr { SLICE_TYPE_I } else { SLICE_TYPE_P })
                    );
                    assert_eq!(bits.read_ue(), 0);
                    assert_eq!(bits.read_bits(8), u32::from(frame_num));
                    if is_idr {
                        assert_eq!(bits.read_ue(), u32::from(idr_pic_id));
                    }
                    assert_eq!(bits.read_bits(8), poc as u32);
                }
            }
        }
    }

    /// Offline probe for the white-screen report (#21): encode solid red
    /// frames and dump the Annex-B stream for external analysis
    /// (`ffmpeg -i <file> …`). Requires VA-API hardware.
    #[test]
    #[ignore = "requires VA-API hardware; writes a stream for offline analysis"]
    fn vaapi_encode_probe_writes_stream_to_disk() {
        let (width, height) = (704u32, 396u32);
        let mut encoder = VaapiEncoder::new(width, height, 5_000_000, 30, 23, H264RateControl::Vbr)
            .expect("VA-API encoder initializes");

        let stride = (width * 4) as usize;
        let mut frame = vec![0u8; stride * height as usize];
        for px in frame.as_chunks_mut::<4>().0 {
            px[0] = 0; // B
            px[1] = 0; // G
            px[2] = 255; // R
            px[3] = 255;
        }

        let mut stream = Vec::new();
        for _ in 0..30 {
            stream.extend_from_slice(&encoder.encode(&frame, stride).expect("frame encodes"));
        }

        let path = std::env::temp_dir().join("hypr-rdp-vaapi-probe.h264");
        std::fs::write(&path, &stream).expect("probe stream written");
        eprintln!("probe: {} bytes -> {}", stream.len(), path.display());
    }

    fn dpb(surface_id: u32, frame_num: u16, poc: i32) -> DpbEntry {
        DpbEntry {
            surface_id,
            frame_num,
            poc,
        }
    }

    #[test]
    fn vaapi_avc444_reference_state_uses_single_h264_sequence_reference() {
        let mut references = VaapiReferenceState::default();
        let luma_0 = dpb(10, 0, 0);
        let chroma_1 = dpb(11, 1, 2);

        references.record(VaapiEncodeRole::Avc444(VaapiAvc444Subframe::Luma), luma_0);
        assert_eq!(
            references.primary_reference(VaapiEncodeRole::Avc444(VaapiAvc444Subframe::Chroma)),
            Some(luma_0),
            "the first chroma subframe after an IDR luma frame must continue the same H.264 sequence"
        );

        references.record(
            VaapiEncodeRole::Avc444(VaapiAvc444Subframe::Chroma),
            chroma_1,
        );

        assert_eq!(
            references.primary_reference(VaapiEncodeRole::Avc444(VaapiAvc444Subframe::Luma)),
            Some(chroma_1),
            "the next luma subframe must use the immediately preceding chroma subframe as the single H.264 sequence reference"
        );
        assert_eq!(
            references.primary_reference(VaapiEncodeRole::Avc444(VaapiAvc444Subframe::Chroma)),
            Some(chroma_1),
            "the next chroma subframe must also use the immediately preceding H.264 sequence reference"
        );

        let luma_refs = references.reference_frames(
            VaapiReferenceMode::Avc444Subframes,
            VaapiEncodeRole::Avc444(VaapiAvc444Subframe::Luma),
        );
        assert_eq!(luma_refs, vec![chroma_1]);
    }

    #[test]
    fn vaapi_avc444_reference_mode_matches_single_h264_sequence() {
        assert_eq!(VaapiReferenceMode::Single.max_num_ref_frames(), 1);
        assert_eq!(VaapiReferenceMode::Avc444Subframes.max_num_ref_frames(), 1);
        assert!(VaapiReferenceMode::Single.uses_periodic_idr());
        assert!(
            !VaapiReferenceMode::Avc444Subframes.uses_periodic_idr(),
            "AVC444 must not inject fixed IDR at subframe cadence"
        );
    }

    #[test]
    fn vaapi_avc444_recon_pool_keeps_current_surface_outside_single_ref() {
        assert_eq!(VaapiReferenceMode::Single.recon_surface_pool_size(), 2);
        assert_eq!(
            VaapiReferenceMode::Avc444Subframes.recon_surface_pool_size(),
            2
        );

        let pool_size = VaapiReferenceMode::Avc444Subframes.recon_surface_pool_size();
        let previous_recon = 0;
        let next_recon = 1 % pool_size;

        assert_ne!(
            next_recon, previous_recon,
            "next CurrPic must not reuse the single active reference surface"
        );
    }

    #[test]
    fn vaapi_vbr_policy_honors_bitrate_and_quality_ceiling() {
        let policy = vaapi_rate_control_policy(10_000_000, 23, H264RateControl::Vbr);

        assert_eq!(policy.config_mode, sys::VA_RC_VBR);
        assert_eq!(policy.bits_per_second, 10_000_000);
        assert_eq!(policy.pic_init_qp, H264_DEFAULT_PIC_INIT_QP);
        assert_eq!(policy.initial_qp, 23);
        assert_eq!(policy.min_qp, 0);
        assert_eq!(policy.target_percentage, 100);
        assert_eq!(policy.rc_flags, 0);
        assert_eq!(policy.max_qp, 23);

        let parameters = vaapi_rate_control_parameters(policy, 30);
        let hrd = parameters.hrd.expect("VBR bitrate target requires HRD");
        assert_eq!(parameters.rate_control.bits_per_second, 10_000_000);
        assert_eq!(parameters.rate_control.target_percentage, 100);
        assert_eq!(parameters.rate_control.window_size, 1000);
        assert_eq!(parameters.rate_control.initial_qp, 23);
        assert_eq!(parameters.rate_control.max_qp, 23);
        assert_eq!(hrd.initial_buffer_fullness, 5_000_000);
        assert_eq!(hrd.buffer_size, 10_000_000);
        assert_eq!(parameters.frame_rate.framerate, 30);
    }

    #[test]
    fn vaapi_vbr_policy_represents_quality_zero_with_active_ceiling() {
        let policy = vaapi_rate_control_policy(10_000_000, 0, H264RateControl::Vbr);
        assert_eq!(policy.initial_qp, 0);
        assert_eq!(policy.max_qp, 1);
    }

    #[test]
    fn vaapi_cqp_policy_pins_qp_and_disables_bitrate_target() {
        let policy = vaapi_rate_control_policy(10_000_000, 32, H264RateControl::Cqp);

        assert_eq!(policy.config_mode, sys::VA_RC_CQP);
        assert_eq!(policy.bits_per_second, 0);
        assert_eq!(policy.pic_init_qp, 32);
        assert_eq!(policy.initial_qp, 32);
        assert_eq!(policy.min_qp, 32);
        assert_eq!(policy.rc_flags, 1 << 1);
        assert_eq!(policy.max_qp, 32);

        let parameters = vaapi_rate_control_parameters(policy, 30);
        assert!(
            parameters.hrd.is_none(),
            "CQP must not submit an HRD buffer"
        );
        assert_eq!(parameters.rate_control.bits_per_second, 0);
        assert_eq!(parameters.frame_rate.framerate, 30);
    }

    #[test]
    fn vaapi_quality_is_clamped_to_h264_qp_range() {
        let policy = vaapi_rate_control_policy(10_000_000, 99, H264RateControl::Cqp);

        assert_eq!(policy.pic_init_qp, 51);
        assert_eq!(policy.initial_qp, 51);
        assert_eq!(policy.min_qp, 51);
        assert_eq!(policy.max_qp, 51);
    }
}
