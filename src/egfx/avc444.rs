use anyhow::{bail, Result};
use std::sync::LazyLock;
use std::time::{Duration, Instant};

#[cfg(test)]
use super::h264::H264FrameType;
use super::h264::{
    annex_b_nal_types, avc444_h264_encoder_options, is_h264_keyframe, EncodedH264, H264Encoder,
    H264EncoderOptions,
};
use super::H264RateControl;

pub(crate) fn avc444_dimensions_supported(width: u32, height: u32) -> bool {
    width != 0 && height != 0 && width.is_multiple_of(4) && height.is_multiple_of(2)
}

pub struct Avc444EncodedFrame {
    pub encoding: Avc444FrameEncoding,
    pub stream1: Vec<u8>,
    pub stream2: Vec<u8>,
    pub stream1_regions: Vec<(i32, i32, i32, i32)>,
    pub stream2_regions: Vec<(i32, i32, i32, i32)>,
}

impl Avc444EncodedFrame {
    pub fn stream1_nal_types(&self) -> Vec<u8> {
        annex_b_nal_types(&self.stream1)
    }

    pub fn stream2_nal_types(&self) -> Vec<u8> {
        annex_b_nal_types(&self.stream2)
    }

    pub fn stream1_has_idr(&self) -> bool {
        self.stream1_nal_types().contains(&5)
    }

    pub fn stream2_has_idr(&self) -> bool {
        self.stream2_nal_types().contains(&5)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Avc444FrameEncoding {
    LumaAndChroma,
    Luma,
    Chroma,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Avc444SubframeRole {
    Luma,
    Chroma,
}

pub struct Avc444Encoder {
    encoder: Avc444H264Encoder,
    width: usize,
    height: usize,
    y444: Vec<u8>,
    main_u: Vec<u8>,
    main_v: Vec<u8>,
    aux_y: Vec<u8>,
    aux_u: Vec<u8>,
    aux_v: Vec<u8>,
    luma_reference: Option<Yuv420Reference>,
    chroma_reference: Option<Yuv420Reference>,
    last_chroma_encoded: bool,
    last_luma_reference_regions: Regions,
    last_chroma_reference_regions: Regions,
    frame_index: u64,
    force_chroma_on_next_frame: bool,
    perf_stats: Avc444PerfStats,
}

impl Avc444Encoder {
    pub fn new(
        width: u32,
        height: u32,
        bitrate: u32,
        fps: u32,
        qp: u8,
        rate_control: H264RateControl,
    ) -> Result<Self> {
        Self::new_with_h264_options(
            width,
            height,
            bitrate,
            fps,
            qp,
            rate_control,
            avc444_h264_encoder_options(),
        )
    }

    fn new_with_h264_options(
        width: u32,
        height: u32,
        bitrate: u32,
        fps: u32,
        qp: u8,
        rate_control: H264RateControl,
        h264_options: H264EncoderOptions,
    ) -> Result<Self> {
        if !avc444_dimensions_supported(width, height) {
            bail!(
                "AVC444v2 dimensions must be non-zero, width must be divisible by 4, and height must be even: {}x{}",
                width,
                height
            );
        }

        let encoder = H264Encoder::new_with_options(
            width,
            height,
            bitrate,
            fps,
            qp,
            rate_control,
            h264_options,
        )?;

        Self::new_with_encoder(
            width,
            height,
            Avc444H264Encoder::Software(Box::new(encoder)),
        )
    }

    fn new_with_encoder(width: u32, height: u32, encoder: Avc444H264Encoder) -> Result<Self> {
        let w = width as usize;
        let h = height as usize;
        let y_len = w * h;
        let uv_len = (w / 2) * (h / 2);

        Ok(Self {
            encoder,
            width: w,
            height: h,
            y444: vec![0; y_len],
            main_u: vec![128; uv_len],
            main_v: vec![128; uv_len],
            aux_y: vec![128; y_len],
            aux_u: vec![128; uv_len],
            aux_v: vec![128; uv_len],
            luma_reference: None,
            chroma_reference: None,
            last_chroma_encoded: false,
            last_luma_reference_regions: Vec::new(),
            last_chroma_reference_regions: Vec::new(),
            frame_index: 0,
            force_chroma_on_next_frame: true,
            perf_stats: Avc444PerfStats::new(),
        })
    }

    pub(crate) fn backend_name(&self) -> &'static str {
        self.encoder.backend_name()
    }

    pub(crate) fn is_vaapi(&self) -> bool {
        self.encoder.is_vaapi()
    }

    pub fn encode(
        &mut self,
        bgra: &[u8],
        stride: usize,
        candidate_regions: &[(i32, i32, i32, i32)],
    ) -> Result<Avc444EncodedFrame> {
        anyhow::ensure!(
            bgra.len() >= self.height * stride,
            "BGRA buffer too small: {} < {}",
            bgra.len(),
            self.height * stride,
        );
        let total_start = Instant::now();
        let candidate_regions =
            align_avc444_v2_protocol_regions(self.width, self.height, candidate_regions);
        let force_full_frame = std::env::var_os("HYPR_RDP_AVC444_FULL_FRAME").is_some()
            || self.force_chroma_on_next_frame;
        if self.force_chroma_on_next_frame {
            self.force_chroma_on_next_frame = false;
        }
        let candidate_regions = if force_full_frame {
            vec![(0, 0, self.width as i32, self.height as i32)]
        } else {
            candidate_regions
        };
        bgra_to_avc444_v2_plane_regions_selective(
            self.width,
            self.height,
            bgra,
            stride,
            &candidate_regions,
            true,
            true,
            &mut self.y444,
            &mut self.main_u,
            &mut self.main_v,
            &mut self.aux_y,
            &mut self.aux_u,
            &mut self.aux_v,
        );

        let mut luma_regions = detect_yuv420_regions(
            self.width,
            self.height,
            &self.y444,
            &self.main_u,
            &self.main_v,
            self.luma_reference.as_ref(),
            &candidate_regions,
        );
        let (detected_chroma_regions, detected_chroma_protocol_regions) =
            detect_avc444_v2_chroma_regions(
                self.width,
                self.height,
                &self.aux_y,
                &self.aux_u,
                &self.aux_v,
                self.chroma_reference.as_ref(),
                &candidate_regions,
            );
        if force_full_frame {
            luma_regions = vec![(0, 0, self.width as i32, self.height as i32)];
        }
        let convert_elapsed = total_start.elapsed();
        let detected_chroma_regions = if force_full_frame {
            vec![(0, 0, self.width as i32, self.height as i32)]
        } else {
            detected_chroma_regions
        };
        let detected_chroma_protocol_regions = if force_full_frame {
            vec![(0, 0, self.width as i32, self.height as i32)]
        } else {
            detected_chroma_protocol_regions
        };
        let chroma_changed = !detected_chroma_protocol_regions.is_empty();
        let encode_chroma = chroma_changed;
        let (chroma_regions, chroma_protocol_regions) = if encode_chroma {
            (detected_chroma_regions, detected_chroma_protocol_regions)
        } else {
            (Vec::new(), Vec::new())
        };

        if luma_regions.is_empty() && chroma_protocol_regions.is_empty() {
            self.last_chroma_encoded = false;
            self.last_luma_reference_regions.clear();
            self.last_chroma_reference_regions.clear();
            return Ok(Avc444EncodedFrame {
                encoding: Avc444FrameEncoding::Luma,
                stream1: Vec::new(),
                stream2: Vec::new(),
                stream1_regions: luma_regions,
                stream2_regions: chroma_protocol_regions,
            });
        }

        self.last_luma_reference_regions = luma_regions.clone();
        self.last_chroma_reference_regions = chroma_regions;

        let (
            encoding,
            stream1,
            stream2,
            stream1_regions,
            stream2_regions,
            stream1_encode_elapsed,
            stream2_encode_elapsed,
        ) = if !luma_regions.is_empty() {
            let stream1_encode_start = Instant::now();
            let luma = self.encoder.encode_avc444_yuv420_raw(
                Avc444SubframeRole::Luma,
                &self.y444,
                &self.main_u,
                &self.main_v,
            )?;
            let stream1_encode_elapsed = stream1_encode_start.elapsed();
            if chroma_protocol_regions.is_empty() {
                self.last_chroma_encoded = false;
                self.debug_log_frame(&luma, &EncodedH264::empty(), &luma_regions, &[]);
                (
                    Avc444FrameEncoding::Luma,
                    luma,
                    EncodedH264::empty(),
                    luma_regions,
                    Vec::new(),
                    stream1_encode_elapsed,
                    Duration::ZERO,
                )
            } else {
                let stream2_encode_start = Instant::now();
                let chroma = self.encoder.encode_avc444_yuv420_raw(
                    Avc444SubframeRole::Chroma,
                    &self.aux_y,
                    &self.aux_u,
                    &self.aux_v,
                )?;
                let stream2_encode_elapsed = stream2_encode_start.elapsed();
                self.last_chroma_encoded = true;
                self.debug_log_frame(&luma, &chroma, &luma_regions, &chroma_protocol_regions);
                (
                    Avc444FrameEncoding::LumaAndChroma,
                    luma,
                    chroma,
                    luma_regions,
                    chroma_protocol_regions,
                    stream1_encode_elapsed,
                    stream2_encode_elapsed,
                )
            }
        } else {
            let stream1_encode_start = Instant::now();
            let chroma = self.encoder.encode_avc444_yuv420_raw(
                Avc444SubframeRole::Chroma,
                &self.aux_y,
                &self.aux_u,
                &self.aux_v,
            )?;
            let stream1_encode_elapsed = stream1_encode_start.elapsed();
            self.last_chroma_encoded = true;
            self.debug_log_frame(
                &EncodedH264::empty(),
                &chroma,
                &[],
                &chroma_protocol_regions,
            );
            (
                Avc444FrameEncoding::Chroma,
                chroma,
                EncodedH264::empty(),
                chroma_protocol_regions,
                Vec::new(),
                stream1_encode_elapsed,
                Duration::ZERO,
            )
        };

        let stream1_is_intra = is_h264_keyframe(stream1.frame_type);
        let stream2_is_intra = is_h264_keyframe(stream2.frame_type);
        let stream1_is_idr = annex_b_nal_types(&stream1.data).contains(&5);
        let stream2_is_idr = annex_b_nal_types(&stream2.data).contains(&5);

        let (frame, commit_chroma_reference) = normalize_avc444_encoded_frame(
            encoding,
            stream1,
            stream2,
            stream1_regions,
            stream2_regions,
        );
        self.last_chroma_encoded = commit_chroma_reference;

        let total_elapsed = total_start.elapsed();
        self.perf_stats.record(
            frame.encoding,
            self.width,
            self.height,
            &candidate_regions,
            &frame.stream1_regions,
            &frame.stream2_regions,
            frame.stream1.len(),
            frame.stream2.len(),
            convert_elapsed,
            stream1_encode_elapsed,
            stream2_encode_elapsed,
            stream1_is_intra,
            stream2_is_intra,
            stream1_is_idr,
            stream2_is_idr,
            total_elapsed,
        );

        if std::env::var_os("HYPR_RDP_AVC444_TRACE").is_some() {
            tracing::trace!(
                frame = self.frame_index,
                convert_ms = convert_elapsed.as_secs_f64() * 1000.0,
                total_ms = total_elapsed.as_secs_f64() * 1000.0,
                candidate_regions = candidate_regions.len(),
                encoding = ?frame.encoding,
                stream1_bytes = frame.stream1.len(),
                stream2_bytes = frame.stream2.len(),
                "AVC444v2 encode timing"
            );
        }

        self.frame_index = self.frame_index.wrapping_add(1);
        Ok(frame)
    }

    pub fn commit_reference(&mut self) {
        update_yuv420_reference_regions(
            &mut self.luma_reference,
            self.width,
            self.height,
            &self.y444,
            &self.main_u,
            &self.main_v,
            &self.last_luma_reference_regions,
        );
        if self.last_chroma_encoded {
            update_yuv420_reference_regions(
                &mut self.chroma_reference,
                self.width,
                self.height,
                &self.aux_y,
                &self.aux_u,
                &self.aux_v,
                &self.last_chroma_reference_regions,
            );
        }
    }

    #[cfg(test)]
    pub(crate) fn luma_reference_y_for_test(&self) -> Option<&[u8]> {
        self.luma_reference
            .as_ref()
            .map(|reference| reference.y.as_slice())
    }

    #[cfg(test)]
    pub(crate) fn last_reference_regions_for_test(&self) -> (&[Region], &[Region]) {
        (
            &self.last_luma_reference_regions,
            &self.last_chroma_reference_regions,
        )
    }

    #[cfg(test)]
    pub(crate) fn force_idr_requests_for_test(&self) -> u32 {
        self.encoder.force_idr_requests_for_test()
    }

    pub fn force_idr(&mut self) {
        self.encoder.force_idr();
        self.force_full_frame();
    }

    pub fn force_full_frame(&mut self) {
        self.force_chroma_on_next_frame = true;
    }

    fn debug_log_frame(
        &self,
        luma: &EncodedH264,
        chroma: &EncodedH264,
        luma_regions: &[Region],
        chroma_regions: &[Region],
    ) {
        if self.frame_index >= 8 && std::env::var_os("HYPR_RDP_AVC444_TRACE").is_none() {
            return;
        }

        tracing::trace!(
            frame = self.frame_index,
            luma_bytes = luma.data.len(),
            chroma_bytes = chroma.data.len(),
            luma_frame_type = ?luma.frame_type,
            chroma_frame_type = ?chroma.frame_type,
            luma_nals = ?annex_b_nal_types(&luma.data),
            chroma_nals = ?annex_b_nal_types(&chroma.data),
            luma_regions = luma_regions.len(),
            chroma_regions = chroma_regions.len(),
            "AVC444v2 encoded frame"
        );
    }
}

enum Avc444H264Encoder {
    Software(Box<H264Encoder>),
}

impl Avc444H264Encoder {
    fn encode_avc444_yuv420_raw(
        &mut self,
        role: Avc444SubframeRole,
        y: &[u8],
        u: &[u8],
        v: &[u8],
    ) -> Result<EncodedH264> {
        match self {
            Self::Software(encoder) => {
                let _ = role;
                encoder.encode_yuv420_raw(y, u, v)
            }
        }
    }

    fn force_idr(&mut self) {
        match self {
            Self::Software(encoder) => encoder.force_idr(),
        }
    }

    fn backend_name(&self) -> &'static str {
        match self {
            Self::Software(_) => "openh264-avc444",
        }
    }

    fn is_vaapi(&self) -> bool {
        match self {
            Self::Software(_) => false,
        }
    }

    #[cfg(test)]
    fn force_idr_requests_for_test(&self) -> u32 {
        match self {
            Self::Software(encoder) => encoder.force_idr_requests_for_test(),
        }
    }
}

fn normalize_avc444_encoded_frame(
    encoding: Avc444FrameEncoding,
    stream1: EncodedH264,
    stream2: EncodedH264,
    stream1_regions: Regions,
    stream2_regions: Regions,
) -> (Avc444EncodedFrame, bool) {
    if stream1_regions.is_empty() || stream1.data.is_empty() {
        return (
            Avc444EncodedFrame {
                encoding: Avc444FrameEncoding::Luma,
                stream1: Vec::new(),
                stream2: Vec::new(),
                stream1_regions: Vec::new(),
                stream2_regions: Vec::new(),
            },
            false,
        );
    }

    match encoding {
        Avc444FrameEncoding::Luma => (
            Avc444EncodedFrame {
                encoding,
                stream1: stream1.data,
                stream2: Vec::new(),
                stream1_regions,
                stream2_regions: Vec::new(),
            },
            false,
        ),
        Avc444FrameEncoding::Chroma => (
            Avc444EncodedFrame {
                encoding,
                stream1: stream1.data,
                stream2: Vec::new(),
                stream1_regions,
                stream2_regions: Vec::new(),
            },
            true,
        ),
        Avc444FrameEncoding::LumaAndChroma => {
            if stream2_regions.is_empty() || stream2.data.is_empty() {
                (
                    Avc444EncodedFrame {
                        encoding: Avc444FrameEncoding::Luma,
                        stream1: stream1.data,
                        stream2: Vec::new(),
                        stream1_regions,
                        stream2_regions: Vec::new(),
                    },
                    false,
                )
            } else {
                (
                    Avc444EncodedFrame {
                        encoding,
                        stream1: stream1.data,
                        stream2: stream2.data,
                        stream1_regions,
                        stream2_regions,
                    },
                    true,
                )
            }
        }
    }
}

struct Avc444PerfStats {
    window_start: Instant,
    frames: u64,
    luma_and_chroma: u64,
    luma_only: u64,
    chroma_only: u64,
    stream1_bytes: u64,
    stream2_bytes: u64,
    candidate_regions: u64,
    stream1_regions: u64,
    stream2_regions: u64,
    candidate_area_pixels: u64,
    stream1_area_pixels: u64,
    stream2_area_pixels: u64,
    convert_us_total: u128,
    stream1_encode_us_total: u128,
    stream2_encode_us_total: u128,
    total_us_total: u128,
    stream1_intra_frames: u64,
    stream2_intra_frames: u64,
    stream1_idr_frames: u64,
    stream2_idr_frames: u64,
}

impl Avc444PerfStats {
    fn new() -> Self {
        Self {
            window_start: Instant::now(),
            frames: 0,
            luma_and_chroma: 0,
            luma_only: 0,
            chroma_only: 0,
            stream1_bytes: 0,
            stream2_bytes: 0,
            candidate_regions: 0,
            stream1_regions: 0,
            stream2_regions: 0,
            candidate_area_pixels: 0,
            stream1_area_pixels: 0,
            stream2_area_pixels: 0,
            convert_us_total: 0,
            stream1_encode_us_total: 0,
            stream2_encode_us_total: 0,
            total_us_total: 0,
            stream1_intra_frames: 0,
            stream2_intra_frames: 0,
            stream1_idr_frames: 0,
            stream2_idr_frames: 0,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn record(
        &mut self,
        encoding: Avc444FrameEncoding,
        width: usize,
        height: usize,
        candidate_regions: &[Region],
        stream1_regions: &[Region],
        stream2_regions: &[Region],
        stream1_bytes: usize,
        stream2_bytes: usize,
        convert_elapsed: Duration,
        stream1_encode_elapsed: Duration,
        stream2_encode_elapsed: Duration,
        stream1_is_intra: bool,
        stream2_is_intra: bool,
        stream1_is_idr: bool,
        stream2_is_idr: bool,
        total_elapsed: Duration,
    ) {
        if !avc444_perf_logging_enabled() {
            return;
        }

        self.frames = self.frames.saturating_add(1);
        match encoding {
            Avc444FrameEncoding::LumaAndChroma => {
                self.luma_and_chroma = self.luma_and_chroma.saturating_add(1);
            }
            Avc444FrameEncoding::Luma => {
                self.luma_only = self.luma_only.saturating_add(1);
            }
            Avc444FrameEncoding::Chroma => {
                self.chroma_only = self.chroma_only.saturating_add(1);
            }
        }
        self.stream1_bytes = self.stream1_bytes.saturating_add(stream1_bytes as u64);
        self.stream2_bytes = self.stream2_bytes.saturating_add(stream2_bytes as u64);
        self.candidate_regions = self
            .candidate_regions
            .saturating_add(candidate_regions.len() as u64);
        self.stream1_regions = self
            .stream1_regions
            .saturating_add(stream1_regions.len() as u64);
        self.stream2_regions = self
            .stream2_regions
            .saturating_add(stream2_regions.len() as u64);
        self.candidate_area_pixels = self
            .candidate_area_pixels
            .saturating_add(region_area_pixels(candidate_regions, width, height));
        self.stream1_area_pixels = self.stream1_area_pixels.saturating_add(region_area_pixels(
            stream1_regions,
            width,
            height,
        ));
        self.stream2_area_pixels = self.stream2_area_pixels.saturating_add(region_area_pixels(
            stream2_regions,
            width,
            height,
        ));
        self.convert_us_total = self
            .convert_us_total
            .saturating_add(convert_elapsed.as_micros());
        self.stream1_encode_us_total = self
            .stream1_encode_us_total
            .saturating_add(stream1_encode_elapsed.as_micros());
        self.stream2_encode_us_total = self
            .stream2_encode_us_total
            .saturating_add(stream2_encode_elapsed.as_micros());
        self.total_us_total = self
            .total_us_total
            .saturating_add(total_elapsed.as_micros());
        if stream1_is_intra {
            self.stream1_intra_frames = self.stream1_intra_frames.saturating_add(1);
        }
        if stream2_is_intra {
            self.stream2_intra_frames = self.stream2_intra_frames.saturating_add(1);
        }
        if stream1_is_idr {
            self.stream1_idr_frames = self.stream1_idr_frames.saturating_add(1);
        }
        if stream2_is_idr {
            self.stream2_idr_frames = self.stream2_idr_frames.saturating_add(1);
        }

        let elapsed = self.window_start.elapsed();
        if elapsed < Duration::from_secs(1) {
            return;
        }

        let frames = self.frames.max(1);
        let seconds = elapsed.as_secs_f64();
        let frame_pixels = (width as u64).saturating_mul(height as u64).max(1);
        let window_pixels = frame_pixels.saturating_mul(frames);
        tracing::info!(
            target: "hypr_rdp::avc444_perf",
            fps = self.frames as f64 / seconds,
            avg_convert_ms = self.convert_us_total as f64 / frames as f64 / 1000.0,
            avg_stream1_encode_ms =
                self.stream1_encode_us_total as f64 / frames as f64 / 1000.0,
            avg_stream2_encode_ms =
                self.stream2_encode_us_total as f64 / frames as f64 / 1000.0,
            avg_total_ms = self.total_us_total as f64 / frames as f64 / 1000.0,
            avg_stream1_kb = self.stream1_bytes as f64 / frames as f64 / 1024.0,
            avg_stream2_kb = self.stream2_bytes as f64 / frames as f64 / 1024.0,
            avg_candidate_regions = self.candidate_regions as f64 / frames as f64,
            avg_stream1_regions = self.stream1_regions as f64 / frames as f64,
            avg_stream2_regions = self.stream2_regions as f64 / frames as f64,
            avg_candidate_area_pct =
                self.candidate_area_pixels as f64 * 100.0 / window_pixels as f64,
            avg_stream1_area_pct =
                self.stream1_area_pixels as f64 * 100.0 / window_pixels as f64,
            avg_stream2_area_pct =
                self.stream2_area_pixels as f64 * 100.0 / window_pixels as f64,
            luma_and_chroma = self.luma_and_chroma,
            luma_only = self.luma_only,
            chroma_only = self.chroma_only,
            stream1_intra_frames = self.stream1_intra_frames,
            stream2_intra_frames = self.stream2_intra_frames,
            stream1_idr_frames = self.stream1_idr_frames,
            stream2_idr_frames = self.stream2_idr_frames,
            "AVC444v2 perf"
        );

        *self = Self::new();
    }
}

fn avc444_perf_logging_enabled() -> bool {
    avc444_perf_logging_enabled_with(|name| std::env::var_os(name).is_some())
}

fn avc444_perf_logging_enabled_with(mut is_set: impl FnMut(&str) -> bool) -> bool {
    is_set("HYPR_RDP_AVC444_PERF")
}

struct Yuv420Reference {
    y: Vec<u8>,
    u: Vec<u8>,
    v: Vec<u8>,
}

type Region = (i32, i32, i32, i32);
type Regions = Vec<Region>;

pub(super) fn region_area_pixels(
    regions: &[(i32, i32, i32, i32)],
    width: usize,
    height: usize,
) -> u64 {
    regions
        .iter()
        .filter_map(|&(x, y, w, h)| {
            let (left, top, right, bottom) = clamp_region(x, y, w, h, width, height)?;
            Some(
                u64::try_from(right - left).unwrap_or(0) * u64::try_from(bottom - top).unwrap_or(0),
            )
        })
        .sum()
}

fn copy_yuv420_reference(
    reference: &mut Option<Yuv420Reference>,
    width: usize,
    height: usize,
    y: &[u8],
    u: &[u8],
    v: &[u8],
) {
    let y_len = width * height;
    let uv_len = (width / 2) * (height / 2);
    let reference = reference.get_or_insert_with(|| Yuv420Reference {
        y: vec![0; y_len],
        u: vec![0; uv_len],
        v: vec![0; uv_len],
    });
    reference.y[..y_len].copy_from_slice(&y[..y_len]);
    reference.u[..uv_len].copy_from_slice(&u[..uv_len]);
    reference.v[..uv_len].copy_from_slice(&v[..uv_len]);
}

fn update_yuv420_reference_regions(
    reference: &mut Option<Yuv420Reference>,
    width: usize,
    height: usize,
    y: &[u8],
    u: &[u8],
    v: &[u8],
    regions: &[(i32, i32, i32, i32)],
) {
    if reference.is_none() {
        copy_yuv420_reference(reference, width, height, y, u, v);
        return;
    }

    let Some(reference) = reference.as_mut() else {
        return;
    };

    for &(x, y_pos, w, h) in regions {
        let Some((left, top, right, bottom)) = clamp_region(x, y_pos, w, h, width, height) else {
            continue;
        };

        copy_plane_region(width, y, &mut reference.y, left, top, right, bottom);

        let chroma_width = width / 2;
        let chroma_left = left / 2;
        let chroma_top = top / 2;
        let chroma_right = right.div_ceil(2);
        let chroma_bottom = bottom.div_ceil(2);
        copy_plane_region(
            chroma_width,
            u,
            &mut reference.u,
            chroma_left,
            chroma_top,
            chroma_right,
            chroma_bottom,
        );
        copy_plane_region(
            chroma_width,
            v,
            &mut reference.v,
            chroma_left,
            chroma_top,
            chroma_right,
            chroma_bottom,
        );
    }
}

fn copy_plane_region(
    stride: usize,
    src: &[u8],
    dst: &mut [u8],
    left: usize,
    top: usize,
    right: usize,
    bottom: usize,
) {
    let width = right.saturating_sub(left);
    if width == 0 {
        return;
    }

    for row in top..bottom {
        let start = row * stride + left;
        let end = start + width;
        dst[start..end].copy_from_slice(&src[start..end]);
    }
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
fn bgra_to_avc444_v2_plane_regions(
    width: usize,
    height: usize,
    bgra: &[u8],
    bgra_stride: usize,
    regions: &[(i32, i32, i32, i32)],
    y444: &mut [u8],
    main_u: &mut [u8],
    main_v: &mut [u8],
    aux_y: &mut [u8],
    aux_u: &mut [u8],
    aux_v: &mut [u8],
) {
    bgra_to_avc444_v2_plane_regions_selective(
        width,
        height,
        bgra,
        bgra_stride,
        regions,
        true,
        true,
        y444,
        main_u,
        main_v,
        aux_y,
        aux_u,
        aux_v,
    );
}

#[allow(clippy::too_many_arguments)]
fn bgra_to_avc444_v2_plane_regions_selective(
    width: usize,
    height: usize,
    bgra: &[u8],
    bgra_stride: usize,
    regions: &[(i32, i32, i32, i32)],
    convert_luma: bool,
    convert_chroma: bool,
    y444: &mut [u8],
    main_u: &mut [u8],
    main_v: &mut [u8],
    aux_y: &mut [u8],
    aux_u: &mut [u8],
    aux_v: &mut [u8],
) {
    let chroma_w = width / 2;
    let quarter_w = width / 4;
    let tables = &*BGRA_TO_YUV_TABLES;

    for &(x, y, w, h) in regions {
        let Some((left, top, right, bottom)) = clamp_region(x, y, w, h, width, height) else {
            continue;
        };
        let left = align_down(left as i32, 4) as usize;
        let top = align_down(top as i32, 2) as usize;
        let right = align_up(right as i32, 4).clamp(0, width as i32) as usize;
        let bottom = align_up(bottom as i32, 2).clamp(0, height as i32) as usize;

        for cy in (top / 2)..(bottom / 2) {
            let even_y = cy * 2;
            let odd_y = even_y + 1;
            let even_row = &bgra[even_y * bgra_stride..(even_y + 1) * bgra_stride];
            let odd_row = &bgra[odd_y * bgra_stride..(odd_y + 1) * bgra_stride];
            let y_even_base = even_y * width;
            let y_odd_base = odd_y * width;
            let main_base = cy * chroma_w;

            for cx in (left / 2)..(right / 2) {
                let x = cx * 2;
                let (ya, ua, va) = bgra_row_pixel_to_yuv(tables, even_row, x);
                let (yb, ub, vb) = bgra_row_pixel_to_yuv(tables, even_row, x + 1);
                let (yc, uc, vc) = bgra_row_pixel_to_yuv(tables, odd_row, x);
                let (yd, ud, vd) = bgra_row_pixel_to_yuv(tables, odd_row, x + 1);

                if convert_luma {
                    y444[y_even_base + x] = ya;
                    y444[y_even_base + x + 1] = yb;
                    y444[y_odd_base + x] = yc;
                    y444[y_odd_base + x + 1] = yd;

                    let dst = main_base + cx;
                    main_u[dst] = avg4_floor(ua, ub, uc, ud);
                    main_v[dst] = avg4_floor(va, vb, vc, vd);
                }

                if convert_chroma {
                    let aux_y_even = y_even_base + cx;
                    let aux_y_odd = y_odd_base + cx;
                    aux_y[aux_y_even] = ub;
                    aux_y[aux_y_even + chroma_w] = vb;
                    aux_y[aux_y_odd] = ud;
                    aux_y[aux_y_odd + chroma_w] = vd;

                    let aux_x = cx / 2;
                    if cx.is_multiple_of(2) {
                        aux_u[main_base + aux_x] = uc;
                        aux_u[main_base + aux_x + quarter_w] = vc;
                    } else {
                        aux_v[main_base + aux_x] = uc;
                        aux_v[main_base + aux_x + quarter_w] = vc;
                    }
                }
            }
        }
    }
}

#[inline(always)]
fn bgra_row_pixel_to_yuv(tables: &BgraToYuvTables, row: &[u8], x: usize) -> (u8, u8, u8) {
    let offset = x * 4;
    let b = row[offset] as usize;
    let g = row[offset + 1] as usize;
    let r = row[offset + 2] as usize;

    bgra_components_to_yuv(tables, r, g, b)
}

#[cfg(test)]
fn bgra_pixel_to_yuv(
    tables: &BgraToYuvTables,
    bgra: &[u8],
    stride: usize,
    x: usize,
    y: usize,
) -> (u8, u8, u8) {
    let offset = y * stride + x * 4;
    let b = bgra[offset] as usize;
    let g = bgra[offset + 1] as usize;
    let r = bgra[offset + 2] as usize;

    bgra_components_to_yuv(tables, r, g, b)
}

#[inline(always)]
fn bgra_components_to_yuv(tables: &BgraToYuvTables, r: usize, g: usize, b: usize) -> (u8, u8, u8) {
    let y = ((tables.y_r[r] + tables.y_g[g] + tables.y_b[b]) >> 8).clamp(0, 255) as u8;
    let u = (((tables.u_r[r] + tables.u_g[g] + tables.u_b[b]) >> 8) + 128).clamp(0, 255) as u8;
    let v = (((tables.v_r[r] + tables.v_g[g] + tables.v_b[b]) >> 8) + 128).clamp(0, 255) as u8;

    (y, u, v)
}

static BGRA_TO_YUV_TABLES: LazyLock<BgraToYuvTables> = LazyLock::new(BgraToYuvTables::new);

struct BgraToYuvTables {
    y_r: [i32; 256],
    y_g: [i32; 256],
    y_b: [i32; 256],
    u_r: [i32; 256],
    u_g: [i32; 256],
    u_b: [i32; 256],
    v_r: [i32; 256],
    v_g: [i32; 256],
    v_b: [i32; 256],
}

impl BgraToYuvTables {
    fn new() -> Self {
        let mut tables = Self {
            y_r: [0; 256],
            y_g: [0; 256],
            y_b: [0; 256],
            u_r: [0; 256],
            u_g: [0; 256],
            u_b: [0; 256],
            v_r: [0; 256],
            v_g: [0; 256],
            v_b: [0; 256],
        };

        for value in 0..256 {
            let c = value as i32;
            tables.y_r[value] = 54 * c;
            tables.y_g[value] = 183 * c;
            tables.y_b[value] = 18 * c;
            tables.u_r[value] = -29 * c;
            tables.u_g[value] = -99 * c;
            tables.u_b[value] = 128 * c;
            tables.v_r[value] = 128 * c;
            tables.v_g[value] = -116 * c;
            tables.v_b[value] = -12 * c;
        }

        tables
    }
}

fn avg4_floor(a: u8, b: u8, c: u8, d: u8) -> u8 {
    ((u32::from(a) + u32::from(b) + u32::from(c) + u32::from(d)) / 4) as u8
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
fn pack_avc444_v2_planes(
    width: usize,
    height: usize,
    u444: &[u8],
    v444: &[u8],
    main_u: &mut [u8],
    main_v: &mut [u8],
    aux_y: &mut [u8],
    aux_u: &mut [u8],
    aux_v: &mut [u8],
) {
    let chroma_w = width / 2;
    let chroma_h = height / 2;
    let quarter_w = width / 4;

    for cy in 0..chroma_h {
        for cx in 0..chroma_w {
            let x = cx * 2;
            let y = cy * 2;
            let dst = cy * chroma_w + cx;
            main_u[dst] = avg_2x2_floor(u444, width, x, y);
            main_v[dst] = avg_2x2_floor(v444, width, x, y);
        }
    }

    aux_y.fill(128);
    aux_u.fill(128);
    aux_v.fill(128);

    for y in 0..height {
        for x in (1..width).step_by(2) {
            let dst_x = x / 2;
            let src = y * width + x;
            let dst = y * width + dst_x;
            aux_y[dst] = u444[src];
            aux_y[dst + chroma_w] = v444[src];
        }
    }

    for cy in 0..chroma_h {
        let src_y = cy * 2 + 1;
        for x in (0..width).step_by(4) {
            let dst_x = x / 4;
            let src = src_y * width + x;
            let dst = cy * chroma_w + dst_x;
            aux_u[dst] = u444[src];
            aux_u[dst + quarter_w] = v444[src];

            if x + 2 < width {
                let src = src + 2;
                aux_v[dst] = u444[src];
                aux_v[dst + quarter_w] = v444[src];
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
fn combine_avc444_v2_planes(
    width: usize,
    height: usize,
    y444: &[u8],
    main_u: &[u8],
    main_v: &[u8],
    aux_y: &[u8],
    aux_u: &[u8],
    aux_v: &[u8],
) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    let chroma_w = width / 2;
    let chroma_h = height / 2;
    let quarter_w = width / 4;
    let y = y444.to_vec();
    let mut u = vec![0; width * height];
    let mut v = vec![0; width * height];

    for cy in 0..chroma_h {
        for cx in 0..chroma_w {
            let src = cy * chroma_w + cx;
            let x = cx * 2;
            let row0 = (cy * 2) * width;
            let row1 = row0 + width;
            for dst in [row0 + x, row0 + x + 1, row1 + x, row1 + x + 1] {
                u[dst] = main_u[src];
                v[dst] = main_v[src];
            }
        }
    }

    for row in 0..height {
        let aux_base = row * width;
        let out_base = row * width;
        for cx in 0..chroma_w {
            let x = cx * 2 + 1;
            u[out_base + x] = aux_y[aux_base + cx];
            v[out_base + x] = aux_y[aux_base + cx + chroma_w];
        }
    }

    for cy in 0..chroma_h {
        let aux_base = cy * chroma_w;
        let out_base = (cy * 2 + 1) * width;
        for qx in 0..quarter_w {
            let src = aux_base + qx;
            let x = qx * 4;
            u[out_base + x] = aux_u[src];
            v[out_base + x] = aux_u[src + quarter_w];
            u[out_base + x + 2] = aux_v[src];
            v[out_base + x + 2] = aux_v[src + quarter_w];
        }
    }

    (y, u, v)
}

#[allow(clippy::too_many_arguments)]
#[cfg(test)]
fn bgra_to_avc444_v2_planes(
    width: usize,
    height: usize,
    bgra: &[u8],
    bgra_stride: usize,
    y444: &mut [u8],
    main_u: &mut [u8],
    main_v: &mut [u8],
    aux_y: &mut [u8],
    aux_u: &mut [u8],
    aux_v: &mut [u8],
) {
    aux_y.fill(128);
    aux_u.fill(128);
    aux_v.fill(128);
    bgra_to_avc444_v2_plane_regions(
        width,
        height,
        bgra,
        bgra_stride,
        &[(0, 0, width as i32, height as i32)],
        y444,
        main_u,
        main_v,
        aux_y,
        aux_u,
        aux_v,
    );
}

#[cfg(test)]
fn avg_2x2_floor(plane: &[u8], stride: usize, x: usize, y: usize) -> u8 {
    let a = u32::from(plane[y * stride + x]);
    let b = u32::from(plane[y * stride + x + 1]);
    let c = u32::from(plane[(y + 1) * stride + x]);
    let d = u32::from(plane[(y + 1) * stride + x + 1]);
    ((a + b + c + d) / 4) as u8
}

fn detect_yuv420_regions(
    width: usize,
    height: usize,
    y: &[u8],
    u: &[u8],
    v: &[u8],
    reference: Option<&Yuv420Reference>,
    candidate_regions: &[(i32, i32, i32, i32)],
) -> Vec<(i32, i32, i32, i32)> {
    let Some(reference) = reference else {
        return vec![(0, 0, width as i32, height as i32)];
    };
    if candidate_regions.is_empty() {
        return Vec::new();
    }

    let mut regions = Vec::new();
    for &(x, y_pos, w, h) in candidate_regions {
        let Some((left, top, right, bottom)) = clamp_region(x, y_pos, w, h, width, height) else {
            continue;
        };
        let mut tile_y = top;
        while tile_y < bottom {
            let tile_bottom = (tile_y + 64).min(bottom);
            let mut tile_x = left;
            while tile_x < right {
                let tile_right = (tile_x + 64).min(right);
                if yuv420_tile_changed(
                    width,
                    y,
                    u,
                    v,
                    reference,
                    tile_x,
                    tile_y,
                    tile_right,
                    tile_bottom,
                ) {
                    push_unique_region(
                        &mut regions,
                        (
                            tile_x as i32,
                            tile_y as i32,
                            (tile_right - tile_x) as i32,
                            (tile_bottom - tile_y) as i32,
                        ),
                    );
                }
                tile_x += 64;
            }
            tile_y += 64;
        }
    }
    regions
}

fn detect_avc444_v2_chroma_regions(
    width: usize,
    height: usize,
    y: &[u8],
    u: &[u8],
    v: &[u8],
    reference: Option<&Yuv420Reference>,
    protocol_candidates: &[(i32, i32, i32, i32)],
) -> (Regions, Regions) {
    if reference.is_none() {
        return (
            vec![(0, 0, width as i32, height as i32)],
            vec![(0, 0, width as i32, height as i32)],
        );
    }

    let mut packed_regions = Vec::new();
    let mut protocol_regions = Vec::new();

    for &candidate in protocol_candidates {
        let packed_candidates = avc444_v2_chroma_packed_candidates(width, height, candidate);
        if packed_candidates.is_empty() {
            continue;
        }

        let changed = detect_yuv420_regions(width, height, y, u, v, reference, &packed_candidates);
        if changed.is_empty() {
            continue;
        }

        for region in changed {
            push_unique_region(&mut packed_regions, region);
            for protocol_region in
                avc444_v2_chroma_packed_region_to_protocol_regions(width, height, region)
            {
                push_unique_region(&mut protocol_regions, protocol_region);
            }
        }
    }

    (packed_regions, protocol_regions)
}

fn align_avc444_v2_protocol_regions(
    width: usize,
    height: usize,
    regions: &[(i32, i32, i32, i32)],
) -> Vec<(i32, i32, i32, i32)> {
    align_regions(width, height, regions, 4, 2)
}

fn align_regions(
    width: usize,
    height: usize,
    regions: &[(i32, i32, i32, i32)],
    x_alignment: i32,
    y_alignment: i32,
) -> Vec<(i32, i32, i32, i32)> {
    let mut aligned = Vec::new();
    let Ok(width) = i32::try_from(width) else {
        return aligned;
    };
    let Ok(height) = i32::try_from(height) else {
        return aligned;
    };

    for &(x, y, w, h) in regions {
        if w <= 0 || h <= 0 {
            continue;
        }

        let left = align_down(x.clamp(0, width), x_alignment);
        let top = align_down(y.clamp(0, height), y_alignment);
        let right = align_up(x.saturating_add(w).clamp(0, width), x_alignment).clamp(0, width);
        let bottom = align_up(y.saturating_add(h).clamp(0, height), y_alignment).clamp(0, height);

        if right > left && bottom > top {
            merge_region(&mut aligned, (left, top, right - left, bottom - top));
        }
    }

    aligned
}

fn align_down(value: i32, alignment: i32) -> i32 {
    value - value.rem_euclid(alignment)
}

fn align_up(value: i32, alignment: i32) -> i32 {
    let rem = value.rem_euclid(alignment);
    if rem == 0 {
        value
    } else {
        value.saturating_add(alignment - rem)
    }
}

fn avc444_v2_chroma_packed_candidates(
    width: usize,
    height: usize,
    region: (i32, i32, i32, i32),
) -> Vec<(i32, i32, i32, i32)> {
    let Some((left, top, right, bottom)) =
        clamp_region(region.0, region.1, region.2, region.3, width, height)
    else {
        return Vec::new();
    };

    let packed_left = left / 2;
    let packed_right = right.div_ceil(2);
    let packed_width = packed_right.saturating_sub(packed_left);
    if packed_width == 0 {
        return Vec::new();
    }

    let half_width = width / 2;
    vec![
        (
            packed_left as i32,
            top as i32,
            packed_width as i32,
            (bottom - top) as i32,
        ),
        (
            (half_width + packed_left) as i32,
            top as i32,
            packed_width as i32,
            (bottom - top) as i32,
        ),
    ]
}

fn avc444_v2_chroma_packed_region_to_protocol_regions(
    width: usize,
    height: usize,
    region: (i32, i32, i32, i32),
) -> Vec<(i32, i32, i32, i32)> {
    let Some((left, top, right, bottom)) =
        clamp_region(region.0, region.1, region.2, region.3, width, height)
    else {
        return Vec::new();
    };
    let half_width = width / 2;
    if half_width == 0 {
        return Vec::new();
    }

    let mut regions = Vec::new();
    for (half_left, half_right, x_offset) in [(0, half_width, 0), (half_width, width, half_width)] {
        let segment_left = left.max(half_left);
        let segment_right = right.min(half_right);
        if segment_right <= segment_left {
            continue;
        }

        let packed_left = segment_left - x_offset;
        let packed_right = segment_right - x_offset;
        let protocol_left = align_down((packed_left * 2) as i32, 4);
        let protocol_top = align_down(top as i32, 2);
        let protocol_right = align_up((packed_right * 2) as i32, 4).clamp(0, width as i32);
        let protocol_bottom = align_up(bottom as i32, 2).clamp(0, height as i32);
        if protocol_right > protocol_left && protocol_bottom > protocol_top {
            merge_region(
                &mut regions,
                (
                    protocol_left,
                    protocol_top,
                    protocol_right - protocol_left,
                    protocol_bottom - protocol_top,
                ),
            );
        }
    }

    regions
}

fn clamp_region(
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    width: usize,
    height: usize,
) -> Option<(usize, usize, usize, usize)> {
    if w <= 0 || h <= 0 {
        return None;
    }
    let width = i32::try_from(width).ok()?;
    let height = i32::try_from(height).ok()?;
    let left = x.clamp(0, width);
    let top = y.clamp(0, height);
    let right = x.saturating_add(w).clamp(0, width);
    let bottom = y.saturating_add(h).clamp(0, height);
    (right > left && bottom > top).then_some((
        left as usize,
        top as usize,
        right as usize,
        bottom as usize,
    ))
}

#[allow(clippy::too_many_arguments)]
fn yuv420_tile_changed(
    width: usize,
    y: &[u8],
    u: &[u8],
    v: &[u8],
    reference: &Yuv420Reference,
    left: usize,
    top: usize,
    right: usize,
    bottom: usize,
) -> bool {
    for row in top..bottom {
        let start = row * width + left;
        let end = row * width + right;
        if y[start..end] != reference.y[start..end] {
            return true;
        }
    }

    let chroma_width = width / 2;
    let chroma_left = left / 2;
    let chroma_right = right.div_ceil(2);
    let chroma_top = top / 2;
    let chroma_bottom = bottom.div_ceil(2);
    for row in chroma_top..chroma_bottom {
        let start = row * chroma_width + chroma_left;
        let end = row * chroma_width + chroma_right;
        if u[start..end] != reference.u[start..end] || v[start..end] != reference.v[start..end] {
            return true;
        }
    }

    false
}

fn merge_region(regions: &mut Vec<(i32, i32, i32, i32)>, region: (i32, i32, i32, i32)) {
    let mut merged = region;
    let mut index = 0;
    while index < regions.len() {
        if regions_overlap_or_touch(regions[index], merged) {
            merged = union_region(regions[index], merged);
            regions.swap_remove(index);
        } else {
            index += 1;
        }
    }
    regions.push(merged);
}

fn push_unique_region(regions: &mut Vec<(i32, i32, i32, i32)>, region: (i32, i32, i32, i32)) {
    if !regions.contains(&region) {
        regions.push(region);
    }
}

fn regions_overlap_or_touch(a: (i32, i32, i32, i32), b: (i32, i32, i32, i32)) -> bool {
    let a_right = a.0.saturating_add(a.2);
    let a_bottom = a.1.saturating_add(a.3);
    let b_right = b.0.saturating_add(b.2);
    let b_bottom = b.1.saturating_add(b.3);
    a.0 <= b_right && b.0 <= a_right && a.1 <= b_bottom && b.1 <= a_bottom
}

fn union_region(a: (i32, i32, i32, i32), b: (i32, i32, i32, i32)) -> (i32, i32, i32, i32) {
    let left = a.0.min(b.0);
    let top = a.1.min(b.1);
    let right = a.0.saturating_add(a.2).max(b.0.saturating_add(b.2));
    let bottom = a.1.saturating_add(a.3).max(b.1.saturating_add(b.3));
    (left, top, right - left, bottom - top)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn avc444_dimension_gate_matches_local_packing_constraints() {
        assert!(avc444_dimensions_supported(1920, 1200));
        assert!(!avc444_dimensions_supported(18, 16));
        assert!(!avc444_dimensions_supported(64, 15));
        assert!(!avc444_dimensions_supported(0, 64));
    }

    #[test]
    fn avc444_v2_encoder_options_prevent_subframe_loss() {
        let options = avc444_h264_encoder_options();

        assert!(!options.scene_change_detect);
        assert!(!options.frame_skip);
    }

    #[test]
    fn bgra_to_yuv_uses_bt709_full_range_reference_points() {
        let tables = &*BGRA_TO_YUV_TABLES;
        let colors: [(u8, u8, u8); 9] = [
            (0, 0, 0),
            (255, 255, 255),
            (128, 128, 128),
            (255, 0, 0),
            (0, 255, 0),
            (0, 0, 255),
            (255, 255, 0),
            (0, 255, 255),
            (255, 0, 255),
        ];

        for (r, g, b) in colors {
            let actual =
                bgra_components_to_yuv(tables, usize::from(r), usize::from(g), usize::from(b));
            let expected = bt709_full_range_reference_yuv(r, g, b);

            assert_yuv_close(actual, expected, (r, g, b));
        }
    }

    #[test]
    fn bgra_to_yuv_keeps_mid_tone_colors_within_bt709_tolerance() {
        let tables = &*BGRA_TO_YUV_TABLES;
        let colors: [(u8, u8, u8); 8] = [
            (16, 32, 48),
            (48, 96, 144),
            (80, 40, 200),
            (120, 200, 32),
            (160, 128, 96),
            (192, 64, 16),
            (224, 180, 72),
            (240, 20, 220),
        ];

        for (r, g, b) in colors {
            let actual =
                bgra_components_to_yuv(tables, usize::from(r), usize::from(g), usize::from(b));
            let expected = bt709_full_range_reference_yuv(r, g, b);

            assert_yuv_close(actual, expected, (r, g, b));
        }
    }

    fn bt709_full_range_reference_yuv(r: u8, g: u8, b: u8) -> (u8, u8, u8) {
        let r = f64::from(r);
        let g = f64::from(g);
        let b = f64::from(b);
        let y = 0.2126 * r + 0.7152 * g + 0.0722 * b;
        let u = -0.114_572 * r - 0.385_428 * g + 0.5 * b + 128.0;
        let v = 0.5 * r - 0.454_153 * g - 0.045_847 * b + 128.0;

        (
            y.round().clamp(0.0, 255.0) as u8,
            u.round().clamp(0.0, 255.0) as u8,
            v.round().clamp(0.0, 255.0) as u8,
        )
    }

    fn assert_yuv_close(actual: (u8, u8, u8), expected: (u8, u8, u8), rgb: (u8, u8, u8)) {
        for (channel, actual, expected) in [
            ("Y", actual.0, expected.0),
            ("U", actual.1, expected.1),
            ("V", actual.2, expected.2),
        ] {
            let diff = actual.abs_diff(expected);
            assert!(
                diff <= 2,
                "{channel} differs for RGB {rgb:?}: actual={actual} expected={expected}"
            );
        }
    }

    #[test]
    fn avc444_perf_logging_is_opt_in() {
        assert!(!avc444_perf_logging_enabled_with(|_| false));
        assert!(avc444_perf_logging_enabled_with(
            |name| name == "HYPR_RDP_AVC444_PERF"
        ));
    }

    #[test]
    fn initial_avc444_v2_frame_requires_luma_and_chroma() {
        let width = 16;
        let height = 16;
        let y = vec![0; width * height];
        let u = vec![128; (width / 2) * (height / 2)];
        let v = vec![128; (width / 2) * (height / 2)];
        let candidates = vec![(4, 4, 4, 4)];

        let luma_regions = detect_yuv420_regions(width, height, &y, &u, &v, None, &candidates);
        let (chroma_regions, chroma_protocol_regions) =
            detect_avc444_v2_chroma_regions(width, height, &y, &u, &v, None, &candidates);

        assert_eq!(luma_regions, vec![(0, 0, 16, 16)]);
        assert_eq!(chroma_regions, vec![(0, 0, 16, 16)]);
        assert_eq!(chroma_protocol_regions, vec![(0, 0, 16, 16)]);
        assert!(!chroma_protocol_regions.is_empty());
    }

    proptest! {
        #[test]
        fn generated_avc444_aligned_protocol_regions_stay_inside_frame(
            regions in proptest::collection::vec(
                (any::<i32>(), any::<i32>(), any::<i32>(), any::<i32>()),
                0..32
            ),
            width in 1usize..=4096,
            height in 1usize..=4096,
        ) {
            let aligned = align_avc444_v2_protocol_regions(width, height, &regions);

            for (left, top, region_width, region_height) in aligned {
                prop_assert!(left >= 0);
                prop_assert!(top >= 0);
                prop_assert!(region_width > 0);
                prop_assert!(region_height > 0);
                prop_assert!((left as usize + region_width as usize) <= width);
                prop_assert!((top as usize + region_height as usize) <= height);
                prop_assert_eq!(left.rem_euclid(4), 0);
                prop_assert_eq!(top.rem_euclid(2), 0);
            }
        }

        #[test]
        fn generated_avc444_chroma_region_mapping_stays_inside_protocol_frame(
            x in any::<i32>(),
            y in any::<i32>(),
            w in any::<i32>(),
            h in any::<i32>(),
            width in 2usize..=4096,
            height in 1usize..=4096,
        ) {
            let width = width & !1;
            prop_assume!(width > 0);

            for packed_region in avc444_v2_chroma_packed_candidates(width, height, (x, y, w, h)) {
                let protocol_regions =
                    avc444_v2_chroma_packed_region_to_protocol_regions(width, height, packed_region);

                for (left, top, region_width, region_height) in protocol_regions {
                    prop_assert!(left >= 0);
                    prop_assert!(top >= 0);
                    prop_assert!(region_width > 0);
                    prop_assert!(region_height > 0);
                    prop_assert!((left as usize + region_width as usize) <= width);
                    prop_assert!((top as usize + region_height as usize) <= height);
                    prop_assert_eq!(left.rem_euclid(4), 0);
                    prop_assert_eq!(top.rem_euclid(2), 0);
                }
            }
        }
    }

    #[test]
    fn chroma_only_avc444_v2_frame_uses_chroma_as_stream1() {
        let frame = Avc444EncodedFrame {
            encoding: Avc444FrameEncoding::Chroma,
            stream1: vec![1, 2, 3, 4],
            stream2: Vec::new(),
            stream1_regions: vec![(8, 4, 4, 2)],
            stream2_regions: Vec::new(),
        };

        assert_eq!(frame.encoding, Avc444FrameEncoding::Chroma);
        assert_eq!(frame.stream1, [1, 2, 3, 4]);
        assert!(frame.stream2.is_empty());
        assert_eq!(frame.stream1_regions, vec![(8, 4, 4, 2)]);
        assert!(frame.stream2_regions.is_empty());
    }

    #[test]
    fn avc444_v2_encoder_first_frame_shape_uses_luma_and_chroma_when_available() {
        let width = 64;
        let height = 64;
        let stride = width * 4;
        let mut bgra = vec![0; stride * height];
        for y in 0..height {
            for x in 0..width {
                let offset = y * stride + x * 4;
                bgra[offset] = (x * 11 + y * 3) as u8;
                bgra[offset + 1] = (x * 5 + y * 17) as u8;
                bgra[offset + 2] = (x * 19 + y * 7) as u8;
                bgra[offset + 3] = 255;
            }
        }

        let mut encoder = match Avc444Encoder::new(
            width as u32,
            height as u32,
            1_000_000,
            30,
            23,
            H264RateControl::Cqp,
        ) {
            Ok(encoder) => encoder,
            Err(error) => panic!("AVC444v2 encoder initialization failed: {error:#}"),
        };

        let frame = encoder
            .encode(&bgra, stride, &[(0, 0, width as i32, height as i32)])
            .expect("AVC444v2 first frame encodes");

        assert_eq!(frame.encoding, Avc444FrameEncoding::LumaAndChroma);
        assert!(!frame.stream1.is_empty());
        assert!(!frame.stream2.is_empty());
        assert_eq!(
            frame.stream1_regions,
            vec![(0, 0, width as i32, height as i32)]
        );
        assert_eq!(
            frame.stream2_regions,
            vec![(0, 0, width as i32, height as i32)]
        );
    }

    #[test]
    fn avc444_v2_vbr_first_frame_produces_sendable_luma_and_chroma_payloads() {
        let width = 16;
        let height = 16;
        let stride = width * 4;
        let mut bgra = vec![0; stride * height];
        for y in 0..height {
            for x in 0..width {
                let offset = y * stride + x * 4;
                bgra[offset] = (x * 11 + y * 3) as u8;
                bgra[offset + 1] = (x * 5 + y * 17) as u8;
                bgra[offset + 2] = (x * 19 + y * 7) as u8;
                bgra[offset + 3] = 255;
            }
        }

        let mut encoder = match Avc444Encoder::new(
            width as u32,
            height as u32,
            1_000_000,
            30,
            23,
            H264RateControl::Vbr,
        ) {
            Ok(encoder) => encoder,
            Err(error) => panic!("AVC444v2 encoder initialization failed: {error:#}"),
        };

        let frame = encoder
            .encode(&bgra, stride, &[(0, 0, width as i32, height as i32)])
            .expect("VBR first frame encodes");

        assert_eq!(frame.encoding, Avc444FrameEncoding::LumaAndChroma);
        assert!(!frame.stream1.is_empty());
        assert!(!frame.stream2.is_empty());
        assert_eq!(
            frame.stream1_regions,
            vec![(0, 0, width as i32, height as i32)]
        );
        assert_eq!(
            frame.stream2_regions,
            vec![(0, 0, width as i32, height as i32)]
        );
    }

    #[test]
    fn avc444_v2_vbr_large_first_frame_produces_sendable_luma_and_chroma_payloads() {
        let width = 1920;
        let height = 1200;
        let stride = width * 4;
        let mut bgra = vec![0; stride * height];
        for y in 0..height {
            for x in 0..width {
                let offset = y * stride + x * 4;
                bgra[offset] = ((x / 7 + y / 3) & 0xff) as u8;
                bgra[offset + 1] = ((x / 5 + y / 11) & 0xff) as u8;
                bgra[offset + 2] = ((x / 13 + y / 17) & 0xff) as u8;
                bgra[offset + 3] = 255;
            }
        }

        let mut encoder = match Avc444Encoder::new(
            width as u32,
            height as u32,
            10_000_000,
            30,
            23,
            H264RateControl::Vbr,
        ) {
            Ok(encoder) => encoder,
            Err(error) => panic!("AVC444v2 encoder initialization failed: {error:#}"),
        };

        let frame = encoder
            .encode(&bgra, stride, &[(0, 0, width as i32, height as i32)])
            .expect("large VBR first frame encodes");

        assert_eq!(frame.encoding, Avc444FrameEncoding::LumaAndChroma);
        assert!(
            frame.stream1.len() > 32,
            "stream1 is not sendable: {} bytes",
            frame.stream1.len()
        );
        assert!(
            frame.stream2.len() > 32,
            "stream2 is not sendable: {} bytes",
            frame.stream2.len()
        );
        assert_eq!(
            frame.stream1_regions,
            vec![(0, 0, width as i32, height as i32)]
        );
        assert_eq!(
            frame.stream2_regions,
            vec![(0, 0, width as i32, height as i32)]
        );
    }

    #[test]
    fn avc444_v2_encoder_luma_only_change_uses_luma_stream_role() {
        let width = 16;
        let height = 16;
        let stride = width * 4;
        let mut first = vec![0; stride * height];
        let mut second = first.clone();
        for y in 0..height {
            for x in 0..width {
                write_bgra_pixel(&mut first, stride, x, y, 128, 128, 128);
                write_bgra_pixel(&mut second, stride, x, y, 128, 128, 128);
            }
        }
        for y in 0..2 {
            for x in 0..4 {
                write_bgra_pixel(&mut second, stride, x, y, 192, 192, 192);
            }
        }

        let mut encoder = match Avc444Encoder::new(
            width as u32,
            height as u32,
            1_000_000,
            30,
            23,
            H264RateControl::Cqp,
        ) {
            Ok(encoder) => encoder,
            Err(error) => panic!("AVC444v2 encoder initialization failed: {error:#}"),
        };

        let first_frame = encoder
            .encode(&first, stride, &[(0, 0, width as i32, height as i32)])
            .expect("first frame encodes");
        assert_eq!(first_frame.encoding, Avc444FrameEncoding::LumaAndChroma);
        encoder.commit_reference();

        let second_frame = encoder
            .encode(&second, stride, &[(0, 0, 4, 2)])
            .expect("second frame encodes");

        assert_eq!(second_frame.encoding, Avc444FrameEncoding::Luma);
        assert!(!second_frame.stream1.is_empty());
        assert!(second_frame.stream2.is_empty());
        assert_eq!(second_frame.stream1_regions, vec![(0, 0, 4, 2)]);
        assert!(second_frame.stream2_regions.is_empty());
    }

    #[test]
    fn avc444_v2_encoder_luma_and_chroma_change_sends_both_streams_immediately() {
        let width = 16;
        let height = 16;
        let stride = width * 4;
        let mut first = vec![0; stride * height];
        let mut second = first.clone();
        for y in 0..height {
            for x in 0..width {
                write_bgra_pixel(&mut first, stride, x, y, 64, 64, 64);
                write_bgra_pixel(&mut second, stride, x, y, 64, 64, 64);
            }
        }
        for y in 0..2 {
            for x in 0..4 {
                write_bgra_pixel(&mut second, stride, x, y, 255, 0, 255);
            }
        }

        let mut encoder = match Avc444Encoder::new(
            width as u32,
            height as u32,
            1_000_000,
            30,
            23,
            H264RateControl::Cqp,
        ) {
            Ok(encoder) => encoder,
            Err(error) => panic!("AVC444v2 encoder initialization failed: {error:#}"),
        };

        let first_frame = encoder
            .encode(&first, stride, &[(0, 0, width as i32, height as i32)])
            .expect("first frame encodes");
        assert_eq!(first_frame.encoding, Avc444FrameEncoding::LumaAndChroma);
        encoder.commit_reference();

        let second_frame = encoder
            .encode(&second, stride, &[(0, 0, 4, 2)])
            .expect("second frame encodes");

        assert_eq!(second_frame.encoding, Avc444FrameEncoding::LumaAndChroma);
        assert!(!second_frame.stream1.is_empty());
        assert!(!second_frame.stream2.is_empty());
        assert_eq!(second_frame.stream1_regions, vec![(0, 0, 4, 2)]);
        assert_eq!(second_frame.stream2_regions, vec![(0, 0, 4, 2)]);
    }

    #[test]
    fn avc444_commit_reference_updates_only_sent_partial_luma_regions() {
        let width = 16;
        let height = 16;
        let stride = width * 4;
        let first = gradient_bgra_frame(width, height, stride);
        let mut second = first.clone();
        for y in 4..6 {
            for x in 4..8 {
                write_bgra_pixel(&mut second, stride, x, y, 255, 255, 255);
            }
        }

        let mut encoder = match new_test_avc444_encoder(width, height) {
            Ok(encoder) => encoder,
            Err(error) => panic!("AVC444v2 encoder initialization failed: {error}"),
        };

        let first_frame = encoder
            .encode(&first, stride, &[(0, 0, width as i32, height as i32)])
            .expect("first frame encodes");
        assert_eq!(first_frame.encoding, Avc444FrameEncoding::LumaAndChroma);
        encoder.commit_reference();

        let before = encoder
            .luma_reference_y_for_test()
            .expect("initial reference is committed")
            .to_vec();
        let second_frame = encoder
            .encode(&second, stride, &[(4, 4, 4, 2)])
            .expect("partial frame encodes");
        assert!(!second_frame.stream1_regions.is_empty());
        let luma_regions = encoder.last_reference_regions_for_test().0.to_vec();
        assert_eq!(second_frame.stream1_regions, luma_regions);

        encoder.commit_reference();

        let after = encoder
            .luma_reference_y_for_test()
            .expect("partial reference is committed");
        let mut changed_inside_sent_region = false;
        for y in 0..height {
            for x in 0..width {
                let index = y * width + x;
                if region_list_contains_point(&luma_regions, x, y) {
                    changed_inside_sent_region |= before[index] != after[index];
                } else {
                    assert_eq!(
                        before[index], after[index],
                        "reference changed outside sent region at ({x}, {y})"
                    );
                }
            }
        }
        assert!(
            changed_inside_sent_region,
            "partial reference commit did not update any pixel inside the sent luma region"
        );
    }

    #[test]
    fn avc444_v2_encoder_maps_full_candidate_to_changed_stream_regions() {
        let width = 256;
        let height = 128;
        let stride = width * 4;
        let mut first = vec![0; stride * height];
        let mut second = first.clone();
        for y in 0..height {
            for x in 0..width {
                write_bgra_pixel(&mut first, stride, x, y, 64, 64, 64);
                write_bgra_pixel(&mut second, stride, x, y, 64, 64, 64);
            }
        }
        for y in 10..12 {
            for x in 40..44 {
                write_bgra_pixel(&mut second, stride, x, y, 255, 0, 255);
            }
        }

        let mut encoder = match Avc444Encoder::new(
            width as u32,
            height as u32,
            1_000_000,
            30,
            23,
            H264RateControl::Cqp,
        ) {
            Ok(encoder) => encoder,
            Err(error) => panic!("AVC444v2 encoder initialization failed: {error:#}"),
        };

        let first_frame = encoder
            .encode(&first, stride, &[(0, 0, width as i32, height as i32)])
            .expect("first frame encodes");
        assert_eq!(first_frame.encoding, Avc444FrameEncoding::LumaAndChroma);
        encoder.commit_reference();

        let second_frame = encoder
            .encode(&second, stride, &[(0, 0, width as i32, height as i32)])
            .expect("second frame encodes");

        assert_eq!(second_frame.encoding, Avc444FrameEncoding::LumaAndChroma);
        assert_eq!(second_frame.stream1_regions, vec![(0, 0, 64, 64)]);
        assert_eq!(second_frame.stream2_regions, vec![(0, 0, 128, 64)]);
    }

    #[test]
    fn avc444_v2_force_idr_targets_single_h264_sequence() {
        let mut encoder = match Avc444Encoder::new(16, 16, 1_000_000, 30, 23, H264RateControl::Cqp)
        {
            Ok(encoder) => encoder,
            Err(error) => panic!("AVC444v2 encoder initialization failed: {error:#}"),
        };

        encoder.force_idr();

        assert_eq!(encoder.force_idr_requests_for_test(), 1);
    }

    #[test]
    fn avc444_lc0_empty_chroma_payload_becomes_luma_only_without_chroma_commit() {
        let (frame, commit_chroma) = normalize_avc444_encoded_frame(
            Avc444FrameEncoding::LumaAndChroma,
            EncodedH264 {
                data: vec![0x00, 0x00, 0x01, 0x65],
                frame_type: H264FrameType::Idr,
            },
            EncodedH264::empty(),
            vec![(0, 0, 16, 16)],
            vec![(0, 0, 16, 16)],
        );

        assert_eq!(frame.encoding, Avc444FrameEncoding::Luma);
        assert_eq!(frame.stream1, vec![0x00, 0x00, 0x01, 0x65]);
        assert!(frame.stream2.is_empty());
        assert_eq!(frame.stream1_regions, vec![(0, 0, 16, 16)]);
        assert!(frame.stream2_regions.is_empty());
        assert!(!commit_chroma);
    }

    #[test]
    fn avc444_lc0_with_both_payloads_keeps_chroma_commit() {
        let (frame, commit_chroma) = normalize_avc444_encoded_frame(
            Avc444FrameEncoding::LumaAndChroma,
            EncodedH264 {
                data: vec![0x00, 0x00, 0x01, 0x65],
                frame_type: H264FrameType::Idr,
            },
            EncodedH264 {
                data: vec![0x00, 0x00, 0x01, 0x41],
                frame_type: H264FrameType::P,
            },
            vec![(0, 0, 16, 16)],
            vec![(0, 0, 16, 16)],
        );

        assert_eq!(frame.encoding, Avc444FrameEncoding::LumaAndChroma);
        assert_eq!(frame.stream2, vec![0x00, 0x00, 0x01, 0x41]);
        assert_eq!(frame.stream2_regions, vec![(0, 0, 16, 16)]);
        assert!(commit_chroma);
    }

    #[derive(Debug)]
    struct Avc444WireProfile {
        stream1_len: usize,
        stream2_len: usize,
        stream1_nals: Vec<u8>,
        stream2_nals: Vec<u8>,
        stream1_regions: Vec<(i32, i32, i32, i32)>,
        stream2_regions: Vec<(i32, i32, i32, i32)>,
    }

    impl From<Avc444EncodedFrame> for Avc444WireProfile {
        fn from(frame: Avc444EncodedFrame) -> Self {
            Self {
                stream1_len: frame.stream1.len(),
                stream2_len: frame.stream2.len(),
                stream1_nals: annex_b_nal_types(&frame.stream1),
                stream2_nals: annex_b_nal_types(&frame.stream2),
                stream1_regions: frame.stream1_regions,
                stream2_regions: frame.stream2_regions,
            }
        }
    }

    fn avc444_profiles_with_options(
        options: H264EncoderOptions,
    ) -> std::result::Result<Vec<Avc444WireProfile>, String> {
        let width = 64;
        let height = 64;
        let stride = width * 4;
        let mut first = gradient_bgra_frame(width, height, stride);
        let mut second = first.clone();
        let mut third = second.clone();
        for y in 0..2 {
            for x in 0..4 {
                write_bgra_pixel(&mut second, stride, x, y, 192, 192, 192);
                write_bgra_pixel(&mut third, stride, x, y, 224, 224, 224);
            }
        }
        for y in 2..4 {
            for x in 4..8 {
                write_bgra_pixel(&mut third, stride, x, y, 64, 192, 64);
            }
        }
        first[3] = 255;

        let mut encoder = Avc444Encoder::new_with_h264_options(
            width as u32,
            height as u32,
            1_000_000,
            30,
            23,
            H264RateControl::Vbr,
            options,
        )
        .map_err(|error| format!("{error:#}"))?;

        let first_frame = encoder
            .encode(&first, stride, &[(0, 0, width as i32, height as i32)])
            .map_err(|error| format!("{error:#}"))?;
        encoder.commit_reference();
        let second_frame = encoder
            .encode(&second, stride, &[(0, 0, 4, 2)])
            .map_err(|error| format!("{error:#}"))?;
        encoder.commit_reference();
        let third_frame = encoder
            .encode(&third, stride, &[(0, 0, 8, 4)])
            .map_err(|error| format!("{error:#}"))?;

        Ok(vec![
            first_frame.into(),
            second_frame.into(),
            third_frame.into(),
        ])
    }

    #[test]
    fn avc444_v2_configured_encoder_uses_delta_slices_after_initial_frame() {
        let profiles = match avc444_profiles_with_options(avc444_h264_encoder_options()) {
            Ok(profiles) => profiles,
            Err(error) => panic!("AVC444v2 encoder failed: {error}"),
        };

        assert!(
            profiles[0].stream1_nals.contains(&5),
            "initial luma stream must start with an IDR: {:?}",
            profiles[0].stream1_nals
        );
        assert!(
            profiles.iter().skip(1).all(|profile| {
                !profile.stream1_nals.contains(&5) && !profile.stream2_nals.contains(&5)
            }),
            "steady AVC444 streams must remain in the same H.264 sequence without per-frame IDR: {profiles:?}"
        );
    }

    #[test]
    fn avc444_v2_configured_encoder_keeps_initial_payload_sendable() {
        let profiles = match avc444_profiles_with_options(avc444_h264_encoder_options()) {
            Ok(profiles) => profiles,
            Err(error) => panic!("AVC444v2 encoder failed: {error}"),
        };

        let profile = &profiles[0];
        assert!(
            profile.stream1_len > 32 && !profile.stream1_regions.is_empty(),
            "initial AVC444 frame must produce a sendable stream1 payload: {profiles:?}"
        );
        assert!(
            profile.stream2_regions.is_empty() || profile.stream2_len > 32,
            "initial AVC444 frame must produce sendable stream2 payload when stream2 regions are present: {profiles:?}"
        );
    }

    fn write_bgra_pixel(bgra: &mut [u8], stride: usize, x: usize, y: usize, r: u8, g: u8, b: u8) {
        let offset = y * stride + x * 4;
        bgra[offset] = b;
        bgra[offset + 1] = g;
        bgra[offset + 2] = r;
        bgra[offset + 3] = 255;
    }

    fn gradient_bgra_frame(width: usize, height: usize, stride: usize) -> Vec<u8> {
        let mut bgra = vec![0; stride * height];
        for y in 0..height {
            for x in 0..width {
                let offset = y * stride + x * 4;
                bgra[offset] = (x * 11 + y * 3) as u8;
                bgra[offset + 1] = (x * 5 + y * 17) as u8;
                bgra[offset + 2] = (x * 19 + y * 7) as u8;
                bgra[offset + 3] = 255;
            }
        }
        bgra
    }

    fn region_list_contains_point(regions: &[Region], x: usize, y: usize) -> bool {
        regions.iter().any(|&(left, top, width, height)| {
            let right = left.saturating_add(width);
            let bottom = top.saturating_add(height);
            left <= x as i32 && (x as i32) < right && top <= y as i32 && (y as i32) < bottom
        })
    }

    fn new_test_avc444_encoder(
        width: usize,
        height: usize,
    ) -> std::result::Result<Avc444Encoder, String> {
        Avc444Encoder::new(
            width as u32,
            height as u32,
            1_000_000,
            30,
            23,
            H264RateControl::Cqp,
        )
        .map_err(|error| format!("{error:#}"))
    }

    #[test]
    fn avc444_force_idr_after_empty_output_recovers_with_full_lc0_and_stream1_idr() {
        let width = 16;
        let height = 16;
        let stride = width * 4;
        let bgra = gradient_bgra_frame(width, height, stride);
        let mut encoder = match new_test_avc444_encoder(width, height) {
            Ok(encoder) => encoder,
            Err(error) => panic!("AVC444v2 encoder initialization failed: {error}"),
        };

        let first = encoder
            .encode(&bgra, stride, &[(0, 0, width as i32, height as i32)])
            .expect("first frame encodes");
        assert_eq!(first.encoding, Avc444FrameEncoding::LumaAndChroma);
        encoder.commit_reference();

        let empty = encoder
            .encode(&bgra, stride, &[(0, 0, width as i32, height as i32)])
            .expect("unchanged frame encodes as no-op");
        assert!(empty.stream1.is_empty());
        assert!(empty.stream2.is_empty());
        assert!(empty.stream1_regions.is_empty());
        assert!(empty.stream2_regions.is_empty());

        encoder.force_idr();
        let recovered = encoder
            .encode(&bgra, stride, &[(0, 0, 4, 2)])
            .expect("forced recovery frame encodes");
        assert_eq!(recovered.encoding, Avc444FrameEncoding::LumaAndChroma);
        assert_eq!(
            recovered.stream1_regions,
            vec![(0, 0, width as i32, height as i32)]
        );
        assert_eq!(
            recovered.stream2_regions,
            vec![(0, 0, width as i32, height as i32)]
        );
        assert!(annex_b_nal_types(&recovered.stream1).contains(&5));
        assert!(
            !annex_b_nal_types(&recovered.stream2).contains(&5),
            "forced recovery must not insert a second IDR on the chroma subframe: {:?}",
            annex_b_nal_types(&recovered.stream2)
        );
        assert!(!recovered.stream2.is_empty());
    }

    #[test]
    fn avc444_force_idr_after_chroma_only_role_switch_recovers_with_full_lc0() {
        let width = 16;
        let height = 16;
        let stride = width * 4;
        let tables = &*BGRA_TO_YUV_TABLES;
        let color_a = (0u8, 0u8, 187u8);
        let color_b = (0u8, 17u8, 17u8);
        let yuv_a = bgra_components_to_yuv(
            tables,
            usize::from(color_a.0),
            usize::from(color_a.1),
            usize::from(color_a.2),
        );
        let yuv_b = bgra_components_to_yuv(
            tables,
            usize::from(color_b.0),
            usize::from(color_b.1),
            usize::from(color_b.2),
        );
        assert_eq!(yuv_a.0, yuv_b.0);
        assert_ne!(yuv_a.1, yuv_b.1);

        let mut first = vec![0; stride * height];
        for y in 0..height {
            for x in 0..width {
                write_bgra_pixel(&mut first, stride, x, y, 128, 128, 128);
            }
        }
        let mut second = first.clone();
        let mut third = second.clone();
        write_bgra_pixel(&mut first, stride, 0, 0, color_a.0, color_a.1, color_a.2);
        write_bgra_pixel(&mut first, stride, 1, 0, color_b.0, color_b.1, color_b.2);
        write_bgra_pixel(&mut second, stride, 0, 0, color_b.0, color_b.1, color_b.2);
        write_bgra_pixel(&mut second, stride, 1, 0, color_a.0, color_a.1, color_a.2);
        write_bgra_pixel(&mut third, stride, 2, 0, color_a.0, color_a.1, color_a.2);

        let mut encoder = match new_test_avc444_encoder(width, height) {
            Ok(encoder) => encoder,
            Err(error) => panic!("AVC444v2 encoder initialization failed: {error}"),
        };

        let first_frame = encoder
            .encode(&first, stride, &[(0, 0, width as i32, height as i32)])
            .expect("first frame encodes");
        assert_eq!(first_frame.encoding, Avc444FrameEncoding::LumaAndChroma);
        encoder.commit_reference();

        let second_frame = encoder
            .encode(&second, stride, &[(0, 0, 4, 2)])
            .expect("chroma-only frame encodes");
        assert_eq!(second_frame.encoding, Avc444FrameEncoding::Chroma);

        encoder.force_idr();
        let recovered = encoder
            .encode(&third, stride, &[(0, 0, 4, 2)])
            .expect("forced recovery frame encodes");
        assert_eq!(recovered.encoding, Avc444FrameEncoding::LumaAndChroma);
        assert_eq!(
            recovered.stream1_regions,
            vec![(0, 0, width as i32, height as i32)]
        );
        assert_eq!(
            recovered.stream2_regions,
            vec![(0, 0, width as i32, height as i32)]
        );
        assert!(annex_b_nal_types(&recovered.stream1).contains(&5));
        assert!(
            !annex_b_nal_types(&recovered.stream2).contains(&5),
            "forced recovery must not insert a second IDR on the chroma subframe: {:?}",
            annex_b_nal_types(&recovered.stream2)
        );
        assert!(!recovered.stream2.is_empty());
    }

    #[test]
    fn avc444_vbr_force_idr_refresh_does_not_insert_mid_lc_chroma_idr() {
        let width = 64;
        let height = 64;
        let stride = width * 4;
        let frame = gradient_bgra_frame(width, height, stride);
        let mut encoder = match Avc444Encoder::new(
            width as u32,
            height as u32,
            1_000_000,
            30,
            23,
            H264RateControl::Vbr,
        ) {
            Ok(encoder) => encoder,
            Err(error) => panic!("AVC444v2 encoder initialization failed: {error:#}"),
        };

        let initial = encoder
            .encode(&frame, stride, &[(0, 0, width as i32, height as i32)])
            .expect("initial frame encodes");
        assert_eq!(initial.encoding, Avc444FrameEncoding::LumaAndChroma);
        encoder.commit_reference();

        encoder.force_idr();
        let refresh = encoder
            .encode(&frame, stride, &[(0, 0, width as i32, height as i32)])
            .expect("forced refresh encodes");

        assert_eq!(refresh.encoding, Avc444FrameEncoding::LumaAndChroma);
        assert_eq!(
            refresh.stream1_regions,
            vec![(0, 0, width as i32, height as i32)]
        );
        assert_eq!(
            refresh.stream2_regions,
            vec![(0, 0, width as i32, height as i32)]
        );
        assert!(
            annex_b_nal_types(&refresh.stream1).contains(&5),
            "refresh luma must be IDR: {:?}",
            annex_b_nal_types(&refresh.stream1)
        );
        assert!(
            !annex_b_nal_types(&refresh.stream2).contains(&5),
            "LC=0 chroma must not receive an extra mid-frame IDR after luma: {:?}",
            annex_b_nal_types(&refresh.stream2)
        );
    }

    #[test]
    fn avc444_vbr_motion_does_not_emit_stream2_only_idr() {
        let width = 128;
        let height = 128;
        let stride = width * 4;
        let mut encoder = match Avc444Encoder::new(
            width as u32,
            height as u32,
            1_000_000,
            30,
            23,
            H264RateControl::Vbr,
        ) {
            Ok(encoder) => encoder,
            Err(error) => panic!("AVC444v2 encoder initialization failed: {error:#}"),
        };

        for frame_index in 0..90 {
            let mut frame = gradient_bgra_frame(width, height, stride);
            let offset = (frame_index * 7) % (width - 32);
            for y in 32..96 {
                for x in offset..offset + 32 {
                    let color = if frame_index % 2 == 0 { 24 } else { 224 };
                    write_bgra_pixel(&mut frame, stride, x, y, color, 255 - color, color / 2);
                }
            }

            let encoded = encoder
                .encode(&frame, stride, &[(0, 0, width as i32, height as i32)])
                .expect("motion frame encodes");
            assert_eq!(encoded.encoding, Avc444FrameEncoding::LumaAndChroma);
            let stream1_has_idr = annex_b_nal_types(&encoded.stream1).contains(&5);
            let stream2_has_idr = annex_b_nal_types(&encoded.stream2).contains(&5);

            if frame_index == 0 {
                assert!(
                    stream1_has_idr,
                    "initial luma stream must establish the H.264 sequence"
                );
            } else {
                assert!(
                    !stream2_has_idr || stream1_has_idr,
                    "steady AVC444 motion must not emit a stream2-only IDR at frame {frame_index}: stream1={:?} stream2={:?}",
                    annex_b_nal_types(&encoded.stream1),
                    annex_b_nal_types(&encoded.stream2)
                );
            }

            encoder.commit_reference();
        }
    }

    #[test]
    fn avc444_v2_encoder_chroma_only_change_uses_chroma_stream_role() {
        let width = 16;
        let height = 16;
        let stride = width * 4;
        let tables = &*BGRA_TO_YUV_TABLES;
        let color_a = (0u8, 0u8, 187u8);
        let color_b = (0u8, 17u8, 17u8);
        let yuv_a = bgra_components_to_yuv(
            tables,
            usize::from(color_a.0),
            usize::from(color_a.1),
            usize::from(color_a.2),
        );
        let yuv_b = bgra_components_to_yuv(
            tables,
            usize::from(color_b.0),
            usize::from(color_b.1),
            usize::from(color_b.2),
        );
        assert_eq!(yuv_a.0, yuv_b.0);
        assert_ne!(yuv_a.1, yuv_b.1);

        let mut first = vec![0; stride * height];
        for y in 0..height {
            for x in 0..width {
                write_bgra_pixel(&mut first, stride, x, y, 128, 128, 128);
            }
        }
        let mut second = first.clone();

        write_bgra_pixel(&mut first, stride, 0, 0, color_a.0, color_a.1, color_a.2);
        write_bgra_pixel(&mut first, stride, 1, 0, color_b.0, color_b.1, color_b.2);
        write_bgra_pixel(&mut second, stride, 0, 0, color_b.0, color_b.1, color_b.2);
        write_bgra_pixel(&mut second, stride, 1, 0, color_a.0, color_a.1, color_a.2);

        let mut encoder = match Avc444Encoder::new(
            width as u32,
            height as u32,
            1_000_000,
            30,
            23,
            H264RateControl::Cqp,
        ) {
            Ok(encoder) => encoder,
            Err(error) => panic!("AVC444v2 encoder initialization failed: {error:#}"),
        };

        let first_frame = encoder
            .encode(&first, stride, &[(0, 0, width as i32, height as i32)])
            .expect("first frame encodes");
        assert_eq!(first_frame.encoding, Avc444FrameEncoding::LumaAndChroma);
        assert!(!first_frame.stream1.is_empty());
        assert!(!first_frame.stream2.is_empty());
        encoder.commit_reference();

        let second_frame = encoder
            .encode(&second, stride, &[(0, 0, 4, 2)])
            .expect("second frame encodes");

        assert_eq!(second_frame.encoding, Avc444FrameEncoding::Chroma);
        assert!(!second_frame.stream1.is_empty());
        assert!(second_frame.stream2.is_empty());
        assert_eq!(second_frame.stream1_regions, vec![(0, 0, 4, 2)]);
        assert!(second_frame.stream2_regions.is_empty());
    }

    #[test]
    fn reference_update_keeps_untransmitted_chroma_state_unchanged() {
        let width = 8;
        let height = 4;
        let mut reference = Some(Yuv420Reference {
            y: vec![1; width * height],
            u: vec![2; (width / 2) * (height / 2)],
            v: vec![3; (width / 2) * (height / 2)],
        });
        let y = vec![9; width * height];
        let u = vec![10; (width / 2) * (height / 2)];
        let v = vec![11; (width / 2) * (height / 2)];

        update_yuv420_reference_regions(&mut reference, width, height, &y, &u, &v, &[]);
        let reference = reference.expect("reference remains initialized");

        assert_eq!(reference.y, vec![1; width * height]);
        assert_eq!(reference.u, vec![2; (width / 2) * (height / 2)]);
        assert_eq!(reference.v, vec![3; (width / 2) * (height / 2)]);
    }

    #[test]
    fn avc444_v2_packing_matches_protocol_plane_layout() {
        let width = 4;
        let height = 4;
        let u444: Vec<u8> = (0..16).map(|v| v as u8).collect();
        let v444: Vec<u8> = (100..116).map(|v| v as u8).collect();
        let mut main_u = vec![0; 4];
        let mut main_v = vec![0; 4];
        let mut aux_y = vec![0; 16];
        let mut aux_u = vec![0; 4];
        let mut aux_v = vec![0; 4];

        pack_avc444_v2_planes(
            width,
            height,
            &u444,
            &v444,
            &mut main_u,
            &mut main_v,
            &mut aux_y,
            &mut aux_u,
            &mut aux_v,
        );

        assert_eq!(main_u, vec![2, 4, 10, 12]);
        assert_eq!(main_v, vec![102, 104, 110, 112]);
        assert_eq!(&aux_y[0..4], &[1, 3, 101, 103]);
        assert_eq!(&aux_y[4..8], &[5, 7, 105, 107]);
        assert_eq!(&aux_y[8..12], &[9, 11, 109, 111]);
        assert_eq!(&aux_y[12..16], &[13, 15, 113, 115]);
        assert_eq!(aux_u, vec![4, 104, 12, 112]);
        assert_eq!(aux_v, vec![6, 106, 14, 114]);
    }

    #[test]
    fn avc444_v2_packing_combines_to_expected_client_yuv444_layout() {
        let width = 4;
        let height = 4;
        let y444: Vec<u8> = (200..216).map(|v| v as u8).collect();
        let u444: Vec<u8> = (0..16).map(|v| v as u8).collect();
        let v444: Vec<u8> = (100..116).map(|v| v as u8).collect();
        let mut main_u = vec![0; 4];
        let mut main_v = vec![0; 4];
        let mut aux_y = vec![0; 16];
        let mut aux_u = vec![0; 4];
        let mut aux_v = vec![0; 4];

        pack_avc444_v2_planes(
            width,
            height,
            &u444,
            &v444,
            &mut main_u,
            &mut main_v,
            &mut aux_y,
            &mut aux_u,
            &mut aux_v,
        );
        let (actual_y, actual_u, actual_v) = combine_avc444_v2_planes(
            width, height, &y444, &main_u, &main_v, &aux_y, &aux_u, &aux_v,
        );

        assert_eq!(actual_y, y444);
        assert_eq!(
            actual_u,
            vec![2, 1, 4, 3, 4, 5, 6, 7, 10, 9, 12, 11, 12, 13, 14, 15]
        );
        assert_eq!(
            actual_v,
            vec![102, 101, 104, 103, 104, 105, 106, 107, 110, 109, 112, 111, 112, 113, 114, 115]
        );
    }

    #[test]
    fn avc444_v2_bgra_path_matches_plane_packing() {
        let width = 8;
        let height = 4;
        let stride = width * 4;
        let mut bgra = vec![0; stride * height];
        for y in 0..height {
            for x in 0..width {
                let offset = y * stride + x * 4;
                bgra[offset] = (x * 17 + y * 3) as u8;
                bgra[offset + 1] = (x * 5 + y * 29) as u8;
                bgra[offset + 2] = (x * 11 + y * 7) as u8;
                bgra[offset + 3] = 255;
            }
        }

        let mut y444 = vec![0; width * height];
        let mut u444 = vec![0; width * height];
        let mut v444 = vec![0; width * height];
        let tables = &*BGRA_TO_YUV_TABLES;
        for y in 0..height {
            for x in 0..width {
                let (yy, uu, vv) = bgra_pixel_to_yuv(tables, &bgra, stride, x, y);
                let offset = y * width + x;
                y444[offset] = yy;
                u444[offset] = uu;
                v444[offset] = vv;
            }
        }

        let uv_len = (width / 2) * (height / 2);
        let mut expected_main_u = vec![0; uv_len];
        let mut expected_main_v = vec![0; uv_len];
        let mut expected_aux_y = vec![0; width * height];
        let mut expected_aux_u = vec![0; uv_len];
        let mut expected_aux_v = vec![0; uv_len];
        pack_avc444_v2_planes(
            width,
            height,
            &u444,
            &v444,
            &mut expected_main_u,
            &mut expected_main_v,
            &mut expected_aux_y,
            &mut expected_aux_u,
            &mut expected_aux_v,
        );

        let mut actual_y = vec![0; width * height];
        let mut actual_main_u = vec![0; uv_len];
        let mut actual_main_v = vec![0; uv_len];
        let mut actual_aux_y = vec![0; width * height];
        let mut actual_aux_u = vec![0; uv_len];
        let mut actual_aux_v = vec![0; uv_len];
        bgra_to_avc444_v2_planes(
            width,
            height,
            &bgra,
            stride,
            &mut actual_y,
            &mut actual_main_u,
            &mut actual_main_v,
            &mut actual_aux_y,
            &mut actual_aux_u,
            &mut actual_aux_v,
        );

        assert_eq!(actual_y, y444);
        assert_eq!(actual_main_u, expected_main_u);
        assert_eq!(actual_main_v, expected_main_v);
        assert_eq!(actual_aux_y, expected_aux_y);
        assert_eq!(actual_aux_u, expected_aux_u);
        assert_eq!(actual_aux_v, expected_aux_v);
    }

    #[test]
    fn avc444_v2_bgra_path_ignores_row_padding() {
        let width = 8;
        let height = 4;
        let tight_stride = width * 4;
        let padded_stride = tight_stride + 12;
        let tight = gradient_bgra_frame(width, height, tight_stride);
        let mut padded = vec![0xee; padded_stride * height];
        for y in 0..height {
            let tight_row = y * tight_stride;
            let padded_row = y * padded_stride;
            padded[padded_row..padded_row + tight_stride]
                .copy_from_slice(&tight[tight_row..tight_row + tight_stride]);
        }

        let y_len = width * height;
        let uv_len = (width / 2) * (height / 2);
        let mut tight_y = vec![0; y_len];
        let mut tight_main_u = vec![0; uv_len];
        let mut tight_main_v = vec![0; uv_len];
        let mut tight_aux_y = vec![0; y_len];
        let mut tight_aux_u = vec![0; uv_len];
        let mut tight_aux_v = vec![0; uv_len];
        bgra_to_avc444_v2_planes(
            width,
            height,
            &tight,
            tight_stride,
            &mut tight_y,
            &mut tight_main_u,
            &mut tight_main_v,
            &mut tight_aux_y,
            &mut tight_aux_u,
            &mut tight_aux_v,
        );

        let mut padded_y = vec![0; y_len];
        let mut padded_main_u = vec![0; uv_len];
        let mut padded_main_v = vec![0; uv_len];
        let mut padded_aux_y = vec![0; y_len];
        let mut padded_aux_u = vec![0; uv_len];
        let mut padded_aux_v = vec![0; uv_len];
        bgra_to_avc444_v2_planes(
            width,
            height,
            &padded,
            padded_stride,
            &mut padded_y,
            &mut padded_main_u,
            &mut padded_main_v,
            &mut padded_aux_y,
            &mut padded_aux_u,
            &mut padded_aux_v,
        );

        assert_eq!(padded_y, tight_y);
        assert_eq!(padded_main_u, tight_main_u);
        assert_eq!(padded_main_v, tight_main_v);
        assert_eq!(padded_aux_y, tight_aux_y);
        assert_eq!(padded_aux_u, tight_aux_u);
        assert_eq!(padded_aux_v, tight_aux_v);
    }

    #[test]
    fn avc444_v2_bgra_region_path_preserves_unchanged_planes() {
        let width = 8;
        let height = 4;
        let stride = width * 4;
        let mut bgra = vec![0; stride * height];
        for y in 0..height {
            for x in 0..width {
                let offset = y * stride + x * 4;
                bgra[offset] = (x * 13 + y * 7) as u8;
                bgra[offset + 1] = (x * 19 + y * 11) as u8;
                bgra[offset + 2] = (x * 23 + y * 17) as u8;
                bgra[offset + 3] = 255;
            }
        }

        let y_len = width * height;
        let uv_len = (width / 2) * (height / 2);
        let mut expected_y = vec![0; y_len];
        let mut expected_main_u = vec![0; uv_len];
        let mut expected_main_v = vec![0; uv_len];
        let mut expected_aux_y = vec![0; y_len];
        let mut expected_aux_u = vec![0; uv_len];
        let mut expected_aux_v = vec![0; uv_len];
        bgra_to_avc444_v2_planes(
            width,
            height,
            &bgra,
            stride,
            &mut expected_y,
            &mut expected_main_u,
            &mut expected_main_v,
            &mut expected_aux_y,
            &mut expected_aux_u,
            &mut expected_aux_v,
        );

        let mut actual_y = vec![11; y_len];
        let mut actual_main_u = vec![22; uv_len];
        let mut actual_main_v = vec![33; uv_len];
        let mut actual_aux_y = vec![44; y_len];
        let mut actual_aux_u = vec![55; uv_len];
        let mut actual_aux_v = vec![66; uv_len];
        bgra_to_avc444_v2_plane_regions(
            width,
            height,
            &bgra,
            stride,
            &[(4, 2, 1, 1)],
            &mut actual_y,
            &mut actual_main_u,
            &mut actual_main_v,
            &mut actual_aux_y,
            &mut actual_aux_u,
            &mut actual_aux_v,
        );

        for y in 0..height {
            for x in 0..width {
                let offset = y * width + x;
                if (4..8).contains(&x) && (2..4).contains(&y) {
                    assert_eq!(actual_y[offset], expected_y[offset]);
                } else {
                    assert_eq!(actual_y[offset], 11);
                }

                let aux_updated =
                    (2..4).contains(&y) && ((2..4).contains(&x) || (6..8).contains(&x));
                if aux_updated {
                    assert_eq!(actual_aux_y[offset], expected_aux_y[offset]);
                } else {
                    assert_eq!(actual_aux_y[offset], 44);
                }
            }
        }

        for cy in 0..(height / 2) {
            for cx in 0..(width / 2) {
                let offset = cy * (width / 2) + cx;
                if cy == 1 && (2..4).contains(&cx) {
                    assert_eq!(actual_main_u[offset], expected_main_u[offset]);
                    assert_eq!(actual_main_v[offset], expected_main_v[offset]);
                } else {
                    assert_eq!(actual_main_u[offset], 22);
                    assert_eq!(actual_main_v[offset], 33);
                }

                if cy == 1 && (cx == 1 || cx == 3) {
                    assert_eq!(actual_aux_u[offset], expected_aux_u[offset]);
                    assert_eq!(actual_aux_v[offset], expected_aux_v[offset]);
                } else {
                    assert_eq!(actual_aux_u[offset], 55);
                    assert_eq!(actual_aux_v[offset], 66);
                }
            }
        }
    }

    #[test]
    fn avc444_v2_selective_bgra_conversion_updates_only_requested_planes() {
        let width = 8;
        let height = 4;
        let stride = width * 4;
        let bgra = gradient_bgra_frame(width, height, stride);
        let y_len = width * height;
        let uv_len = (width / 2) * (height / 2);

        let mut y = vec![11; y_len];
        let mut main_u = vec![22; uv_len];
        let mut main_v = vec![33; uv_len];
        let mut aux_y = vec![44; y_len];
        let mut aux_u = vec![55; uv_len];
        let mut aux_v = vec![66; uv_len];
        bgra_to_avc444_v2_plane_regions_selective(
            width,
            height,
            &bgra,
            stride,
            &[(0, 0, width as i32, height as i32)],
            true,
            false,
            &mut y,
            &mut main_u,
            &mut main_v,
            &mut aux_y,
            &mut aux_u,
            &mut aux_v,
        );
        assert_ne!(y, vec![11; y_len]);
        assert_ne!(main_u, vec![22; uv_len]);
        assert_ne!(main_v, vec![33; uv_len]);
        assert_eq!(aux_y, vec![44; y_len]);
        assert_eq!(aux_u, vec![55; uv_len]);
        assert_eq!(aux_v, vec![66; uv_len]);

        let luma_y = y.clone();
        let luma_main_u = main_u.clone();
        let luma_main_v = main_v.clone();
        bgra_to_avc444_v2_plane_regions_selective(
            width,
            height,
            &bgra,
            stride,
            &[(0, 0, width as i32, height as i32)],
            false,
            true,
            &mut y,
            &mut main_u,
            &mut main_v,
            &mut aux_y,
            &mut aux_u,
            &mut aux_v,
        );
        assert_eq!(y, luma_y);
        assert_eq!(main_u, luma_main_u);
        assert_eq!(main_v, luma_main_v);
        assert_ne!(aux_y, vec![44; y_len]);
        assert_ne!(aux_u, vec![55; uv_len]);
        assert_ne!(aux_v, vec![66; uv_len]);
    }

    #[test]
    fn yuv420_region_detection_uses_candidate_area() {
        let width = 128;
        let height = 64;
        let y = vec![0; width * height];
        let u = vec![128; (width / 2) * (height / 2)];
        let v = vec![128; (width / 2) * (height / 2)];
        let reference = Yuv420Reference {
            y: y.clone(),
            u: u.clone(),
            v: v.clone(),
        };

        let mut changed_y = y;
        changed_y[10 * width + 90] = 1;
        let regions = detect_yuv420_regions(
            width,
            height,
            &changed_y,
            &u,
            &v,
            Some(&reference),
            &[(64, 0, 64, 64)],
        );

        assert_eq!(regions, vec![(64, 0, 64, 64)]);
    }

    #[test]
    fn yuv420_region_detection_preserves_l_shaped_tiles_without_outer_union() {
        let width = 128;
        let height = 128;
        let y = vec![0; width * height];
        let u = vec![128; (width / 2) * (height / 2)];
        let v = vec![128; (width / 2) * (height / 2)];
        let reference = Yuv420Reference {
            y: y.clone(),
            u: u.clone(),
            v: v.clone(),
        };

        let mut changed_y = y;
        changed_y[10 * width + 10] = 1;
        changed_y[10 * width + 70] = 1;
        changed_y[70 * width + 10] = 1;
        let regions = detect_yuv420_regions(
            width,
            height,
            &changed_y,
            &u,
            &v,
            Some(&reference),
            &[(0, 0, width as i32, height as i32)],
        );

        assert_eq!(
            regions,
            vec![(0, 0, 64, 64), (64, 0, 64, 64), (0, 64, 64, 64)]
        );
    }

    #[test]
    fn yuv420_region_detection_checks_chroma_planes() {
        let width = 128;
        let height = 64;
        let y = vec![0; width * height];
        let u = vec![128; (width / 2) * (height / 2)];
        let v = vec![128; (width / 2) * (height / 2)];
        let reference = Yuv420Reference {
            y: y.clone(),
            u: u.clone(),
            v: v.clone(),
        };

        let mut changed_u = u;
        changed_u[5 * (width / 2) + 45] = 127;
        let regions = detect_yuv420_regions(
            width,
            height,
            &y,
            &changed_u,
            &v,
            Some(&reference),
            &[(64, 0, 64, 64)],
        );

        assert_eq!(regions, vec![(64, 0, 64, 64)]);
    }

    #[test]
    fn avc444_v2_protocol_regions_are_aligned_for_chroma_decode() {
        let regions = align_avc444_v2_protocol_regions(1920, 1200, &[(101, 105, 17, 9)]);

        assert_eq!(regions, vec![(100, 104, 20, 10)]);
    }

    #[test]
    fn avc444_v2_chroma_detection_uses_packed_coordinates_but_returns_protocol_regions() {
        let width = 128;
        let height = 128;
        let y = vec![128; width * height];
        let u = vec![128; (width / 2) * (height / 2)];
        let v = vec![128; (width / 2) * (height / 2)];
        let reference = Yuv420Reference {
            y: y.clone(),
            u: u.clone(),
            v: v.clone(),
        };

        let mut changed_y = y;
        changed_y[100 * width + 55] = 127;
        let candidates = align_avc444_v2_protocol_regions(width, height, &[(110, 100, 1, 1)]);
        let (packed_regions, protocol_regions) = detect_avc444_v2_chroma_regions(
            width,
            height,
            &changed_y,
            &u,
            &v,
            Some(&reference),
            &candidates,
        );

        assert_eq!(packed_regions, vec![(54, 100, 2, 2)]);
        assert_eq!(protocol_regions, vec![(108, 100, 4, 2)]);
    }

    #[test]
    fn avc444_v2_chroma_detection_maps_changed_packed_tiles_not_whole_candidate() {
        let width = 256;
        let height = 128;
        let y = vec![128; width * height];
        let u = vec![128; (width / 2) * (height / 2)];
        let v = vec![128; (width / 2) * (height / 2)];
        let reference = Yuv420Reference {
            y: y.clone(),
            u: u.clone(),
            v: v.clone(),
        };

        let mut changed_y = y;
        changed_y[10 * width + 20] = 127;
        let candidates = align_avc444_v2_protocol_regions(width, height, &[(0, 0, 256, 128)]);
        let (packed_regions, protocol_regions) = detect_avc444_v2_chroma_regions(
            width,
            height,
            &changed_y,
            &u,
            &v,
            Some(&reference),
            &candidates,
        );

        assert_eq!(packed_regions, vec![(0, 0, 64, 64)]);
        assert_eq!(protocol_regions, vec![(0, 0, 128, 64)]);
    }
}
