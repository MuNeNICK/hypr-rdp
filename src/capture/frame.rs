use std::num::{NonZeroU16, NonZeroUsize};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use ironrdp_server::{BitmapUpdate, DesktopSize, DisplayUpdate, PixelFormat};
use tokio::sync::mpsc;

use crate::egfx::{
    avc444_dimensions_supported, EgfxFrameCodec as EgfxCodec, EgfxFrameFlowSnapshot,
    EgfxFrameReadiness, EgfxShared, EncodedFrameState, H264BackendPolicy, H264RateControl,
};

use super::damage::{
    clamp_damage_region, damage_area_pixels, merge_damage_region, FrameDiffDamageDetector,
};

/// Maximum consecutive encode failures before falling back to software encoder.
const MAX_ENCODE_FAILURES: u32 = 5;

#[cfg(not(test))]
const FRAME_STATS_LOG_INTERVAL: Duration = Duration::from_secs(1);
#[cfg(test)]
const FRAME_STATS_LOG_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// Common frame processing: EGFX AVC/ClearCodec encoding or bitmap fallback.
pub(super) struct FrameProcessor {
    egfx_shared: Option<Arc<EgfxShared>>,
    pub(super) h264_encoder: Option<crate::egfx::FrameEncoder>,
    egfx_handle: Option<ironrdp_server::GfxServerHandle>,
    pub(super) egfx_sender: Option<tokio::sync::mpsc::UnboundedSender<ironrdp_server::ServerEvent>>,
    pub(super) egfx_surface_id: Option<u16>,
    egfx_active: bool,
    egfx_ready: bool,
    pub(super) egfx_generation: u32,
    pub(super) egfx_codec: Option<EgfxCodec>,
    width: u32,
    height: u32,
    pixel_format: PixelFormat,
    stride: u32,
    bitrate: u32,
    quality: u8,
    rate_control: H264RateControl,
    h264_backend: H264BackendPolicy,
    fps: u32,
    /// Whether we've sent at least one frame (first frame always sent)
    pub(super) sent_first_frame: bool,
    /// Consecutive encode failure count for runtime VAAPI -> software fallback.
    encode_failures: u32,
    pub(super) pending_damage_regions: Vec<(i32, i32, i32, i32)>,
    damage_detector: FrameDiffDamageDetector,
    pub(super) stats: FrameStats,
    pending_initial_resize: Option<DesktopSize>,
}

pub(super) struct FrameStats {
    window_start: Instant,
    captured_frames: u32,
    sent_frames: u32,
    skipped_no_damage: u32,
    skipped_pacer: u32,
    skipped_encoder: u32,
    skipped_backpressure: u32,
    skipped_local_backpressure: u32,
    skipped_transport_unavailable: u32,
    skipped_transport_not_ready: u32,
    skipped_transport_no_channel: u32,
    skipped_transport_backpressure: u32,
    bytes: u64,
    encode_us_total: u128,
    send_us_total: u128,
    clearcodec_process_us_total: u128,
    damage_pixels: u64,
    capture_damage_regions: u32,
    promoted_full_scans: u32,
    damage_regions: u32,
    last_codec: Option<FrameStatsCodec>,
    last_surface_id: Option<u16>,
    last_frame_id: u32,
    last_acked_frame_id: u32,
    frames_in_flight: u32,
    client_queue_depth: u32,
    frame_ack_suspended: bool,
    frame_ack_stream_established: bool,
    total_queued_frames: u64,
    total_acked_frames: u64,
}

#[derive(Clone, Copy, Debug)]
enum FrameStatsCodec {
    Avc420,
    Avc444,
    ClearCodec,
}

impl From<EgfxCodec> for FrameStatsCodec {
    fn from(codec: EgfxCodec) -> Self {
        match codec {
            EgfxCodec::Avc420 => Self::Avc420,
            EgfxCodec::Avc444 => Self::Avc444,
        }
    }
}

struct SentFrameStats<'a> {
    width: u32,
    height: u32,
    codec: FrameStatsCodec,
    surface_id: u16,
    damage_regions: &'a [(i32, i32, i32, i32)],
    bytes: usize,
    encode_elapsed: Duration,
    send_elapsed: Duration,
    flow: EgfxFrameFlowSnapshot,
}

fn egfx_perf_logging_enabled() -> bool {
    egfx_perf_logging_enabled_with(|name| std::env::var_os(name).is_some())
}

pub(super) fn egfx_perf_logging_enabled_with(mut is_set: impl FnMut(&str) -> bool) -> bool {
    is_set("HYPR_RDP_EGFX_PERF")
}

impl FrameStats {
    fn new() -> Self {
        Self {
            window_start: Instant::now(),
            captured_frames: 0,
            sent_frames: 0,
            skipped_no_damage: 0,
            skipped_pacer: 0,
            skipped_encoder: 0,
            skipped_backpressure: 0,
            skipped_local_backpressure: 0,
            skipped_transport_unavailable: 0,
            skipped_transport_not_ready: 0,
            skipped_transport_no_channel: 0,
            skipped_transport_backpressure: 0,
            bytes: 0,
            encode_us_total: 0,
            send_us_total: 0,
            clearcodec_process_us_total: 0,
            damage_pixels: 0,
            capture_damage_regions: 0,
            promoted_full_scans: 0,
            damage_regions: 0,
            last_codec: None,
            last_surface_id: None,
            last_frame_id: 0,
            last_acked_frame_id: 0,
            frames_in_flight: 0,
            client_queue_depth: 0,
            frame_ack_suspended: false,
            frame_ack_stream_established: false,
            total_queued_frames: 0,
            total_acked_frames: 0,
        }
    }

    pub(super) fn record_capture(
        &mut self,
        width: u32,
        height: u32,
        capture_damage_regions: usize,
        promoted_full_scan: bool,
    ) {
        self.captured_frames = self.captured_frames.saturating_add(1);
        self.capture_damage_regions = self
            .capture_damage_regions
            .saturating_add(capture_damage_regions as u32);
        if promoted_full_scan {
            self.promoted_full_scans = self.promoted_full_scans.saturating_add(1);
        }
        self.maybe_log(width, height);
    }

    pub(super) fn record_no_damage_skip(&mut self, width: u32, height: u32) {
        self.skipped_no_damage = self.skipped_no_damage.saturating_add(1);
        self.maybe_log(width, height);
    }

    pub(super) fn record_pacer_skip(&mut self, width: u32, height: u32) {
        self.skipped_pacer = self.skipped_pacer.saturating_add(1);
        self.maybe_log(width, height);
    }

    pub(super) fn record_encoder_skip(&mut self, width: u32, height: u32) {
        self.skipped_encoder = self.skipped_encoder.saturating_add(1);
        self.maybe_log(width, height);
    }

    fn record_sent(&mut self, sent: SentFrameStats<'_>) {
        self.sent_frames = self.sent_frames.saturating_add(1);
        self.bytes = self.bytes.saturating_add(sent.bytes as u64);
        self.encode_us_total = self
            .encode_us_total
            .saturating_add(sent.encode_elapsed.as_micros());
        self.send_us_total = self
            .send_us_total
            .saturating_add(sent.send_elapsed.as_micros());
        self.damage_pixels = self.damage_pixels.saturating_add(damage_area_pixels(
            sent.damage_regions,
            sent.width,
            sent.height,
        ));
        self.damage_regions = self
            .damage_regions
            .saturating_add(sent.damage_regions.len() as u32);
        self.last_codec = Some(sent.codec);
        self.last_surface_id = Some(sent.surface_id);
        self.record_flow_snapshot(sent.flow);
        self.maybe_log(sent.width, sent.height);
    }

    pub(super) fn record_send_unavailable(
        &mut self,
        readiness: EgfxFrameReadiness,
        flow: EgfxFrameFlowSnapshot,
        width: u32,
        height: u32,
    ) {
        self.skipped_backpressure = self.skipped_backpressure.saturating_add(1);
        match readiness {
            EgfxFrameReadiness::Ready => {}
            EgfxFrameReadiness::LocalBackpressure { .. } => {
                self.skipped_local_backpressure = self.skipped_local_backpressure.saturating_add(1);
            }
            EgfxFrameReadiness::TransportUnavailable => {
                self.skipped_transport_unavailable =
                    self.skipped_transport_unavailable.saturating_add(1);
            }
            EgfxFrameReadiness::TransportNotReady => {
                self.skipped_transport_not_ready =
                    self.skipped_transport_not_ready.saturating_add(1);
            }
            EgfxFrameReadiness::TransportNoChannel => {
                self.skipped_transport_no_channel =
                    self.skipped_transport_no_channel.saturating_add(1);
            }
            EgfxFrameReadiness::TransportBackpressure { .. } => {
                self.skipped_transport_backpressure =
                    self.skipped_transport_backpressure.saturating_add(1);
            }
        }
        self.record_flow_snapshot(flow);
        self.maybe_log(width, height);
    }

    fn record_flow_snapshot(&mut self, flow: EgfxFrameFlowSnapshot) {
        self.last_frame_id = flow.last_queued_frame_id;
        self.last_acked_frame_id = flow.last_acked_frame_id;
        self.frames_in_flight = flow.frames_in_flight;
        self.client_queue_depth = flow.client_queue_depth;
        self.frame_ack_suspended = flow.frame_ack_suspended;
        self.frame_ack_stream_established = flow.frame_ack_stream_established;
        self.total_queued_frames = flow.total_queued_frames;
        self.total_acked_frames = flow.total_acked_frames;
    }

    fn maybe_log(&mut self, width: u32, height: u32) {
        let elapsed = self.window_start.elapsed();
        if elapsed < FRAME_STATS_LOG_INTERVAL {
            return;
        }

        let seconds = elapsed.as_secs_f64();
        let frames = self.sent_frames.max(1);
        let frame_pixels = u64::from(width) * u64::from(height);
        let avg_damage_pct = if frame_pixels == 0 || self.sent_frames == 0 {
            0.0
        } else {
            (self.damage_pixels as f64 * 100.0) / (frame_pixels as f64 * self.sent_frames as f64)
        };
        let avg_damage_regions = if self.sent_frames == 0 {
            0.0
        } else {
            f64::from(self.damage_regions) / f64::from(self.sent_frames)
        };
        let avg_capture_damage_regions = if self.captured_frames == 0 {
            0.0
        } else {
            f64::from(self.capture_damage_regions) / f64::from(self.captured_frames)
        };
        let ack_gap = self
            .total_queued_frames
            .saturating_sub(self.total_acked_frames);

        if egfx_perf_logging_enabled() {
            tracing::info!(
                target: "hypr_rdp::egfx_perf",
                captured_fps = self.captured_frames as f64 / seconds,
                fps = self.sent_frames as f64 / seconds,
                last_codec = ?self.last_codec,
                clearcodec_process_ms = self.clearcodec_process_us_total as f64 / 1000.0,
                last_surface_id = ?self.last_surface_id,
                last_frame_id = self.last_frame_id,
                last_acked_frame_id = self.last_acked_frame_id,
                frames_in_flight = self.frames_in_flight,
                client_queue_depth = self.client_queue_depth,
                frame_ack_suspended = self.frame_ack_suspended,
                frame_ack_stream_established = self.frame_ack_stream_established,
                total_queued_frames = self.total_queued_frames,
                total_acked_frames = self.total_acked_frames,
                ack_gap,
                mbps = (self.bytes as f64 * 8.0) / seconds / 1_000_000.0,
                avg_encode_ms = self.encode_us_total as f64 / f64::from(frames) / 1000.0,
                avg_send_ms = self.send_us_total as f64 / f64::from(frames) / 1000.0,
                avg_damage_pct,
                avg_damage_regions,
                avg_capture_damage_regions,
                promoted_full_scans = self.promoted_full_scans,
                skipped_no_damage = self.skipped_no_damage,
                skipped_pacer = self.skipped_pacer,
                skipped_encoder = self.skipped_encoder,
                skipped_backpressure = self.skipped_backpressure,
                skipped_local_backpressure = self.skipped_local_backpressure,
                skipped_transport_unavailable = self.skipped_transport_unavailable,
                skipped_transport_not_ready = self.skipped_transport_not_ready,
                skipped_transport_no_channel = self.skipped_transport_no_channel,
                skipped_transport_backpressure = self.skipped_transport_backpressure,
                "EGFX frame stats"
            );
        }

        *self = Self::new();
    }
}

/// Capture frame pacer using an absolute deadline while tolerating compositor
/// frame-time quantization.
pub(super) struct FramePacer {
    base_interval: Duration,
    frame_interval: Duration,
    next_send_at: Option<Instant>,
    last_send_at: Option<Instant>,
}

impl FramePacer {
    const SEND_EARLY_FRACTION: f64 = 0.10;

    fn interval_for(target_fps: u32) -> Duration {
        Duration::from_secs_f64(1.0 / f64::from(target_fps.max(1)))
    }

    pub(super) fn new(target_fps: u32, now: Instant) -> Self {
        let frame_interval = Self::interval_for(target_fps);
        Self {
            base_interval: frame_interval,
            frame_interval,
            next_send_at: Some(now),
            last_send_at: None,
        }
    }

    pub(super) fn should_send(
        &mut self,
        now: Instant,
        sent_first_frame: bool,
        has_damage: bool,
        target_fps: u32,
    ) -> bool {
        self.frame_interval = Self::interval_for(target_fps);

        if !sent_first_frame {
            self.next_send_at = Some(now + self.frame_interval);
            self.last_send_at = Some(now);
            return true;
        }

        if !has_damage {
            return false;
        }

        if self.frame_interval > self.base_interval {
            let next_send_at = self
                .last_send_at
                .map(|last| last + self.frame_interval)
                .unwrap_or(now);
            let send_early = self.frame_interval.mul_f64(Self::SEND_EARLY_FRACTION);
            if now + send_early < next_send_at {
                return false;
            }

            self.last_send_at = Some(now);
            self.next_send_at = Some(now + self.frame_interval);
            return true;
        }

        let next_send_at = self.next_send_at.unwrap_or(now);
        let send_early = self.frame_interval.mul_f64(Self::SEND_EARLY_FRACTION);
        if now + send_early < next_send_at {
            return false;
        }

        let mut next = next_send_at + self.frame_interval;
        while next <= now {
            next += self.frame_interval;
        }
        self.next_send_at = Some(next);
        self.last_send_at = Some(now);
        true
    }
}

impl FrameProcessor {
    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        egfx_shared: Option<Arc<EgfxShared>>,
        width: u32,
        height: u32,
        pixel_format: PixelFormat,
        stride: u32,
        bitrate: u32,
        quality: u8,
        rate_control: H264RateControl,
        fps: u32,
    ) -> Self {
        Self::new_with_h264_backend(
            egfx_shared,
            width,
            height,
            pixel_format,
            stride,
            bitrate,
            quality,
            rate_control,
            fps,
            H264BackendPolicy::Auto,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn new_with_h264_backend(
        egfx_shared: Option<Arc<EgfxShared>>,
        width: u32,
        height: u32,
        pixel_format: PixelFormat,
        stride: u32,
        bitrate: u32,
        quality: u8,
        rate_control: H264RateControl,
        fps: u32,
        h264_backend: H264BackendPolicy,
    ) -> Self {
        Self {
            egfx_shared,
            h264_encoder: None,
            egfx_handle: None,
            egfx_sender: None,
            egfx_surface_id: None,
            egfx_active: false,
            egfx_ready: false,
            egfx_generation: 0,
            egfx_codec: None,
            width,
            height,
            pixel_format,
            stride,
            bitrate,
            quality,
            rate_control,
            h264_backend,
            fps,
            sent_first_frame: false,
            encode_failures: 0,
            pending_damage_regions: Vec::new(),
            damage_detector: FrameDiffDamageDetector::new(),
            stats: FrameStats::new(),
            pending_initial_resize: None,
        }
    }

    pub(super) fn set_pending_initial_resize(&mut self, resize: Option<DesktopSize>) {
        self.pending_initial_resize = resize;
    }

    pub(super) fn has_pending_damage(&self) -> bool {
        !self.pending_damage_regions.is_empty()
    }

    pub(super) fn pacing_fps(&self) -> u32 {
        self.egfx_shared.as_ref().map_or(self.fps.max(1), |shared| {
            shared.preferred_frame_rate(self.fps)
        })
    }

    fn metadata_qp(&self) -> u8 {
        match self.rate_control {
            H264RateControl::Vbr => 0,
            H264RateControl::Cqp => self.quality.min(51),
        }
    }

    fn handle_encoder_skip(
        encode_failures: &mut u32,
        stats: &mut FrameStats,
        width: u32,
        height: u32,
    ) {
        *encode_failures = 0;
        stats.record_encoder_skip(width, height);
        tracing::trace!("H.264 encoder skipped frame; preserving pending damage for retry");
    }

    pub(super) fn queue_damage(&mut self, damage_regions: &[(i32, i32, i32, i32)]) {
        for &(x, y, w, h) in damage_regions {
            let Some(region) = clamp_damage_region(x, y, w, h, self.width, self.height) else {
                continue;
            };
            merge_damage_region(&mut self.pending_damage_regions, region);
        }
    }

    fn process_clearcodec(&mut self, data: &[u8]) -> bool {
        let shared = self.egfx_shared.as_ref().expect("ClearCodec requires EGFX");
        let generation = shared.generation();
        if generation != self.egfx_generation || self.h264_encoder.is_some() {
            self.egfx_generation = generation;
            self.h264_encoder = None;
            self.egfx_codec = None;
            self.egfx_active = false;
            self.egfx_surface_id = None;
            self.sent_first_frame = false;
            self.damage_detector.invalidate();
        }
        let Some(handle) = shared.get_handle() else {
            return true;
        };
        let Some(sender) = shared.get_event_sender() else {
            return true;
        };
        if sender.is_closed() {
            return false;
        }
        let readiness = if shared.full_frame_requested() {
            shared.full_frame_refresh_readiness()
        } else {
            shared.frame_readiness(&handle)
        };
        if !readiness.is_ready() {
            self.stats.record_send_unavailable(
                readiness,
                shared.frame_flow_snapshot(),
                self.width,
                self.height,
            );
            return true;
        }
        let (Ok(width), Ok(height)) = (u16::try_from(self.width), u16::try_from(self.height))
        else {
            return false;
        };
        let full_refresh = shared.full_frame_requested() || !self.sent_first_frame;
        let damage = if full_refresh {
            vec![(0, 0, self.width as i32, self.height as i32)]
        } else {
            self.damage_detector.detect(
                data,
                self.width,
                self.height,
                self.stride as usize,
                &self.pending_damage_regions,
            )
        };
        if damage.is_empty() {
            self.pending_damage_regions.clear();
            return true;
        }
        let Some(surface_id) = shared.init_or_reuse_surface(&handle, &sender, width, height) else {
            return true;
        };
        let process_start = Instant::now();
        match shared.send_clearcodec_damage(
            &handle,
            &sender,
            surface_id,
            data,
            width,
            height,
            self.stride as usize,
            &damage,
            0,
            self.pixel_format,
        ) {
            Ok(Some(bytes)) => {
                self.sent_first_frame = true;
                self.egfx_surface_id = Some(surface_id);
                self.damage_detector.update_reference_regions(
                    data,
                    self.width,
                    self.height,
                    self.stride as usize,
                    &damage,
                );
                self.pending_damage_regions.clear();
                shared.take_full_frame_request();
                // The ClearCodec API combines encode and enqueue in one transaction.
                self.stats.clearcodec_process_us_total = self
                    .stats
                    .clearcodec_process_us_total
                    .saturating_add(process_start.elapsed().as_micros());
                self.stats.record_sent(SentFrameStats {
                    width: self.width,
                    height: self.height,
                    codec: FrameStatsCodec::ClearCodec,
                    surface_id,
                    damage_regions: &damage,
                    bytes,
                    encode_elapsed: Duration::ZERO,
                    send_elapsed: Duration::ZERO,
                    flow: shared.frame_flow_snapshot(),
                });
            }
            Ok(None) => {}
            Err(error) => {
                tracing::error!(%error, "ClearCodec frame failed; ending capture session");
                return false;
            }
        }
        true
    }

    /// Process a captured frame. Returns true if the capture loop should continue.
    pub(super) fn process(&mut self, data: &[u8], tx: &mpsc::Sender<DisplayUpdate>) -> bool {
        self.process_at(data, tx, Instant::now())
    }

    fn process_at(&mut self, data: &[u8], tx: &mpsc::Sender<DisplayUpdate>, now: Instant) -> bool {
        let force_egfx_full_frame = self
            .egfx_shared
            .as_ref()
            .is_some_and(|shared| shared.full_frame_requested());

        // Skip frames with no damage (except the very first frame)
        if self.sent_first_frame && !self.has_pending_damage() && !force_egfx_full_frame {
            return true;
        }

        if let Some(size) = self.pending_initial_resize {
            let graphics_ready = self
                .egfx_shared
                .as_ref()
                .is_none_or(|shared| shared.is_ready());
            if graphics_ready {
                tracing::info!(
                    width = size.width,
                    height = size.height,
                    "Sending initial resize after graphics channel is ready"
                );
                self.pending_initial_resize = None;
                if tx.blocking_send(DisplayUpdate::Resize(size)).is_err() {
                    tracing::info!("Display update channel closed");
                }
                return false;
            }
        }

        if self
            .egfx_shared
            .as_ref()
            .is_some_and(|shared| shared.is_ready() && !shared.is_avc_enabled())
        {
            return self.process_clearcodec(data);
        }

        let mut sent_via_egfx = false;
        if let Some(shared) = &self.egfx_shared {
            let egfx_ready = shared.is_ready();
            let avc_enabled = shared.is_avc_enabled();
            let ready = egfx_ready && avc_enabled;
            let codec = if shared.is_avc444_enabled()
                && avc444_dimensions_supported(self.width, self.height)
            {
                Some(EgfxCodec::Avc444)
            } else if avc_enabled {
                Some(EgfxCodec::Avc420)
            } else {
                None
            };
            let gen = shared.generation();

            if ready != self.egfx_ready {
                self.egfx_ready = ready;
                if !ready {
                    self.egfx_active = false;
                    self.egfx_handle = None;
                    self.egfx_sender = None;
                    self.egfx_surface_id = None;
                    self.h264_encoder = None;
                    self.egfx_codec = None;
                    self.sent_first_frame = false;
                    self.damage_detector.invalidate();
                    if !egfx_ready {
                        tracing::trace!("EGFX channel became unavailable");
                    }
                }
            }

            if gen != self.egfx_generation || (ready && codec != self.egfx_codec) {
                self.egfx_generation = gen;
                self.egfx_surface_id = None;
                self.h264_encoder = None;
                self.egfx_codec = None;
                self.sent_first_frame = false;
                self.damage_detector.invalidate();
                if ready {
                    let selected_codec = codec.unwrap_or(EgfxCodec::Avc420);
                    let encoder_result = crate::egfx::FrameEncoder::new_for_egfx_codec(
                        self.h264_backend,
                        selected_codec,
                        self.width,
                        self.height,
                        self.bitrate,
                        self.fps,
                        self.quality,
                        self.rate_control,
                    );

                    match encoder_result {
                        Ok(enc) => {
                            tracing::info!(
                                width = self.width,
                                height = self.height,
                                backend = enc.backend_name(),
                                codec = ?selected_codec,
                                gen,
                                bitrate = self.bitrate,
                                "H.264 encoder initialized"
                            );
                            self.egfx_codec = Some(selected_codec);
                            self.h264_encoder = Some(enc);
                        }
                        Err(e) => tracing::warn!("Failed to initialize H.264 encoder: {:#}", e),
                    }
                }
            }

            if ready && shared.full_frame_requested() {
                let readiness = shared.full_frame_refresh_readiness();
                if !readiness.is_ready() {
                    tracing::trace!(
                        ?readiness,
                        reason = readiness.reason(),
                        "EGFX full-frame refresh waiting for ACK window to drain"
                    );
                    self.stats.record_send_unavailable(
                        readiness,
                        shared.frame_flow_snapshot(),
                        self.width,
                        self.height,
                    );
                    return true;
                }
            }

            let force_full_frame = if ready {
                shared.take_full_frame_request()
            } else {
                false
            };
            let frame_damage_regions = if force_full_frame {
                vec![(0, 0, self.width as i32, self.height as i32)]
            } else if self.sent_first_frame {
                self.damage_detector.detect(
                    data,
                    self.width,
                    self.height,
                    self.stride as usize,
                    &self.pending_damage_regions,
                )
            } else {
                self.damage_detector.detect(
                    data,
                    self.width,
                    self.height,
                    self.stride as usize,
                    &[(0, 0, self.width as i32, self.height as i32)],
                )
            };
            if self.sent_first_frame && frame_damage_regions.is_empty() {
                self.pending_damage_regions.clear();
                self.stats.record_no_damage_skip(self.width, self.height);
                return true;
            }

            if ready && !self.egfx_active {
                self.egfx_handle = shared.get_handle();
                self.egfx_sender = shared.get_event_sender();
                if self.h264_encoder.is_some()
                    && self.egfx_handle.is_some()
                    && self.egfx_sender.is_some()
                {
                    self.egfx_active = true;
                    tracing::trace!("EGFX transport ready, switching to H.264 encoding");
                }
            }

            if self.egfx_active {
                // Surface initialization (separate borrow scope)
                if self.egfx_surface_id.is_none() {
                    if let (Some(handle), Some(sender)) = (&self.egfx_handle, &self.egfx_sender) {
                        if let Some(sid) = shared.init_or_reuse_surface(
                            handle,
                            sender,
                            self.width as u16,
                            self.height as u16,
                        ) {
                            self.egfx_surface_id = Some(sid);
                        }
                    }
                }

                // Encode and send (encoder borrow released before fallback check)
                if let Some(sid) = self.egfx_surface_id {
                    if let Some(handle) = &self.egfx_handle {
                        let readiness = shared.frame_readiness(handle);
                        if !readiness.is_ready() {
                            tracing::trace!(
                                ?readiness,
                                reason = readiness.reason(),
                                "EGFX frame skipped before encode"
                            );
                            self.stats.record_send_unavailable(
                                readiness,
                                shared.frame_flow_snapshot(),
                                self.width,
                                self.height,
                            );
                            return true;
                        }
                    }

                    let encode_start = Instant::now();
                    let codec = self.egfx_codec.unwrap_or(EgfxCodec::Avc420);
                    if force_full_frame {
                        if let Some(enc) = &mut self.h264_encoder {
                            enc.force_full_frame();
                        }
                    }
                    let encode_result = self.h264_encoder.as_mut().map(|enc| {
                        enc.encode_egfx_frame(
                            codec,
                            data,
                            self.stride as usize,
                            &frame_damage_regions,
                        )
                    });
                    let encode_elapsed = encode_start.elapsed();
                    match encode_result {
                        Some(Ok(ref encoded)) if encoded.state() == EncodedFrameState::Sendable => {
                            self.encode_failures = 0;
                            if let (Some(handle), Some(sender)) =
                                (&self.egfx_handle, &self.egfx_sender)
                            {
                                let timestamp = std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .unwrap_or_default()
                                    .as_millis()
                                    as u32;
                                let send_start = Instant::now();
                                sent_via_egfx = shared.send_tracked_encoded_egfx_frame(
                                    handle,
                                    sender,
                                    sid,
                                    encoded,
                                    &frame_damage_regions,
                                    timestamp,
                                    self.width as u16,
                                    self.height as u16,
                                    self.metadata_qp(),
                                );
                                let send_elapsed = send_start.elapsed();
                                if !sent_via_egfx {
                                    if let Some(enc) = &mut self.h264_encoder {
                                        enc.force_idr();
                                    }
                                } else {
                                    let flow = shared.frame_flow_snapshot();
                                    encoded.log_sent_frame(
                                        flow.last_queued_frame_id,
                                        sid,
                                        self.width,
                                        self.height,
                                        &frame_damage_regions,
                                    );
                                    if let Some(enc) = &mut self.h264_encoder {
                                        encoded.commit_after_send(enc);
                                    }
                                    self.damage_detector.update_reference_regions(
                                        data,
                                        self.width,
                                        self.height,
                                        self.stride as usize,
                                        &frame_damage_regions,
                                    );
                                    self.stats.record_sent(SentFrameStats {
                                        width: self.width,
                                        height: self.height,
                                        codec: codec.into(),
                                        surface_id: sid,
                                        damage_regions: &frame_damage_regions,
                                        bytes: encoded.len(),
                                        encode_elapsed,
                                        send_elapsed,
                                        flow,
                                    });
                                }
                            }
                        }
                        Some(Ok(ref encoded)) if encoded.state() == EncodedFrameState::Skipped => {
                            Self::handle_encoder_skip(
                                &mut self.encode_failures,
                                &mut self.stats,
                                self.width,
                                self.height,
                            );
                        }
                        Some(Ok(_)) => {
                            self.encode_failures += 1;
                            tracing::trace!(
                                failures = self.encode_failures,
                                max = MAX_ENCODE_FAILURES,
                                "H.264 encode produced no usable output"
                            );
                            if let Some(enc) = &mut self.h264_encoder {
                                enc.force_idr();
                            }
                        }
                        Some(Err(e)) => {
                            self.encode_failures += 1;
                            tracing::warn!(
                                failures = self.encode_failures,
                                max = MAX_ENCODE_FAILURES,
                                "H.264 encode failed: {:#}",
                                e
                            );
                            if let Some(enc) = &mut self.h264_encoder {
                                enc.force_idr();
                            }
                        }
                        None => {}
                    }

                    if force_full_frame && !sent_via_egfx {
                        shared.request_full_frame();
                    }

                    // In auto mode, recover from repeated VA-API failures by
                    // switching to software. Forced VA-API mode must never
                    // silently substitute a software H.264 encoder.
                    if self.encode_failures >= MAX_ENCODE_FAILURES
                        && self.h264_encoder.as_ref().is_some_and(|e| e.is_vaapi())
                    {
                        if self.h264_backend.allows_runtime_software_fallback() {
                            tracing::warn!(
                                "VA-API encode failed {} consecutive times, switching to software encoder",
                                self.encode_failures
                            );
                            let fallback_result =
                                crate::egfx::FrameEncoder::new_software_only_for_egfx_codec(
                                    self.egfx_codec.unwrap_or(EgfxCodec::Avc420),
                                    self.width,
                                    self.height,
                                    self.bitrate,
                                    self.fps,
                                    self.quality,
                                    self.rate_control,
                                );
                            match fallback_result {
                                Ok(enc) => {
                                    self.h264_encoder = Some(enc);
                                    self.encode_failures = 0;
                                    self.egfx_surface_id = None; // Force surface re-init
                                }
                                Err(e) => {
                                    tracing::error!("Software encoder fallback failed: {:#}", e);
                                    self.h264_encoder = None;
                                    self.egfx_active = false;
                                }
                            }
                        } else {
                            tracing::error!(
                                failures = self.encode_failures,
                                "Forced VA-API encoder failed repeatedly; disabling H.264 instead of switching backends"
                            );
                            self.h264_encoder = None;
                            self.egfx_active = false;
                        }
                    }
                }
            }

            // Non-AVC EGFX is handled by process_clearcodec before the AVC path.
        }

        if sent_via_egfx {
            self.sent_first_frame = true;
            self.pending_damage_regions.clear();
        }

        // Send bitmaps only when EGFX is unavailable or negotiated without AVC.
        // If EGFX is configured but capability negotiation has not completed yet,
        // keep the damage pending so startup does not mix legacy bitmap updates
        // with the graphics pipeline activation sequence.
        let egfx_state = self
            .egfx_shared
            .as_ref()
            .map(|s| (s.is_ready(), s.is_avc_enabled()));
        let egfx_runtime_available =
            self.egfx_active && self.h264_encoder.is_some() && self.egfx_surface_id.is_some();
        let should_send_bitmap = match egfx_state {
            None => true,
            Some((false, _)) => self
                .egfx_shared
                .as_ref()
                .is_some_and(|shared| shared.bitmap_fallback_due(now)),
            Some((true, avc_enabled)) => !avc_enabled || !egfx_runtime_available,
        };

        if !sent_via_egfx && should_send_bitmap {
            self.sent_first_frame = true;
            let update = DisplayUpdate::Bitmap(BitmapUpdate {
                x: 0,
                y: 0,
                width: NonZeroU16::new(self.width as u16).expect("width is non-zero"),
                height: NonZeroU16::new(self.height as u16).expect("height is non-zero"),
                format: self.pixel_format,
                data: Bytes::copy_from_slice(data),
                stride: NonZeroUsize::new(self.stride as usize).expect("stride is non-zero"),
            });
            if tx.blocking_send(update).is_err() {
                tracing::info!("Display update channel closed");
                return false;
            }
            self.damage_detector
                .update_reference(data, self.height, self.stride as usize);
            self.pending_damage_regions.clear();
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::scale::prepare_presentation_frame;
    use crate::display::geometry::{PresentationGeometry, Size};
    use crate::egfx::test_support::{
        ack_frame, drain_gfx_pdus, negotiated_avc444_egfx, negotiated_egfx_with_policy,
        negotiated_no_avc_egfx, negotiated_no_avc_session, process_avc444_capabilities,
        start_gfx_channel, tracked_avc444_session, unnegotiated_egfx_shared,
        Avc444PresentationOracle, ClearCodecFrameOracle, ExpectedAvc444Encoding, TestQueueDepth,
    };
    use crate::egfx::{
        EgfxCodecPolicy, H264RateControl, HyprGfxFactory, DEFAULT_MAX_FRAMES_IN_FLIGHT,
        GFX_READY_GRACE,
    };
    use crate::input::OutputLayoutSnapshot;
    use ironrdp_server::{DisplayUpdate, PixelFormat};
    use ironrdp_server::{GfxServerFactory, ServerEventSender};
    use std::sync::Arc;
    use std::time::{Duration, Instant};
    use tokio::sync::mpsc;

    #[test]
    fn frame_processor_encoder_skip_preserves_retry_without_forcing_idr() {
        let width = 64;
        let height = 64;
        let stride = width * 4;
        let mut processor = FrameProcessor::new(
            None,
            width as u32,
            height as u32,
            PixelFormat::BgrA32,
            stride as u32,
            1_000_000,
            23,
            H264RateControl::Cqp,
            30,
        );
        processor.h264_encoder = Some(
            crate::egfx::FrameEncoder::new_avc444_software_only(
                width as u32,
                height as u32,
                1_000_000,
                30,
                23,
                H264RateControl::Cqp,
            )
            .expect("AVC444 encoder initializes"),
        );
        processor.encode_failures = 2;

        FrameProcessor::handle_encoder_skip(
            &mut processor.encode_failures,
            &mut processor.stats,
            processor.width,
            processor.height,
        );

        assert_eq!(processor.encode_failures, 0);
        assert_eq!(
            processor
                .h264_encoder
                .as_ref()
                .and_then(crate::egfx::FrameEncoder::force_idr_requests_for_test),
            Some(0)
        );
    }

    fn gradient_bgra_frame(width: usize, height: usize, stride: usize) -> Vec<u8> {
        let mut frame = vec![0; stride * height];
        for y in 0..height {
            for x in 0..width {
                let offset = y * stride + x * 4;
                frame[offset] = (x * 11 + y * 3) as u8;
                frame[offset + 1] = (x * 5 + y * 17) as u8;
                frame[offset + 2] = (x * 19 + y * 7) as u8;
                frame[offset + 3] = 255;
            }
        }
        frame
    }

    fn solid_bgra_frame(
        width: usize,
        height: usize,
        stride: usize,
        r: u8,
        g: u8,
        b: u8,
    ) -> Vec<u8> {
        let mut frame = vec![0; stride * height];
        for y in 0..height {
            for x in 0..width {
                write_bgra_pixel(&mut frame, stride, x, y, r, g, b);
            }
        }
        frame
    }

    fn write_bgra_pixel(frame: &mut [u8], stride: usize, x: usize, y: usize, r: u8, g: u8, b: u8) {
        let offset = y * stride + x * 4;
        frame[offset] = b;
        frame[offset + 1] = g;
        frame[offset + 2] = r;
        frame[offset + 3] = 255;
    }

    fn output_layout_snapshot(
        source: (u32, u32),
        presentation: (u32, u32),
    ) -> OutputLayoutSnapshot {
        let source_size = Size::new(source.0, source.1).unwrap();
        let presentation_size = Size::new(presentation.0, presentation.1).unwrap();
        OutputLayoutSnapshot {
            output_name: "DP-1".into(),
            output_w: source.0,
            output_h: source.1,
            layout_extent_w: source.0,
            layout_extent_h: source.1,
            output_offset_x: 0,
            output_offset_y: 0,
            presentation_geometry: PresentationGeometry::new(source_size, presentation_size),
            geometry_generation: 0,
        }
    }

    fn mutate_bgra_tile(
        frame: &mut [u8],
        width: usize,
        height: usize,
        stride: usize,
        index: usize,
    ) {
        let start_x = 8 + (index * 7) % (width - 24);
        let start_y = 8 + (index * 5) % (height - 24);
        for y in start_y..start_y + 16 {
            for x in start_x..start_x + 16 {
                let offset = y * stride + x * 4;
                frame[offset] = frame[offset].wrapping_add((index as u8).wrapping_mul(17));
                frame[offset + 1] ^= 0x5a;
                frame[offset + 2] = frame[offset + 2].wrapping_sub(0x33);
            }
        }
    }

    fn assert_bitmap_update(
        rx: &mut mpsc::Receiver<DisplayUpdate>,
        width: usize,
        height: usize,
        stride: usize,
        format: PixelFormat,
        expected_data: &[u8],
    ) {
        match rx.try_recv() {
            Ok(DisplayUpdate::Bitmap(update)) => {
                assert_eq!(update.x, 0);
                assert_eq!(update.y, 0);
                assert_eq!(update.width.get(), width as u16);
                assert_eq!(update.height.get(), height as u16);
                assert_eq!(update.stride.get(), stride);
                assert_eq!(update.format, format);
                assert_eq!(update.data.as_ref(), expected_data);
            }
            other => panic!("expected bitmap update, got {other:?}"),
        }
    }

    #[test]
    fn egfx_perf_logging_is_opt_in() {
        assert!(!egfx_perf_logging_enabled_with(|_| false));
        assert!(egfx_perf_logging_enabled_with(
            |name| name == "HYPR_RDP_EGFX_PERF"
        ));
    }

    #[test]
    fn frame_stats_keeps_send_unavailable_reason_counters() {
        let mut stats = FrameStats::new();
        let transport_flow = EgfxFrameFlowSnapshot {
            last_queued_frame_id: 8,
            last_acked_frame_id: 5,
            frames_in_flight: 3,
            client_queue_depth: 0,
            frame_ack_suspended: false,
            frame_ack_stream_established: true,
            total_queued_frames: 9,
            total_acked_frames: 6,
        };
        let local_flow = EgfxFrameFlowSnapshot {
            last_queued_frame_id: 11,
            last_acked_frame_id: 8,
            frames_in_flight: 3,
            client_queue_depth: 0,
            frame_ack_suspended: false,
            frame_ack_stream_established: true,
            total_queued_frames: 12,
            total_acked_frames: 9,
        };

        stats.record_send_unavailable(
            EgfxFrameReadiness::TransportBackpressure {
                in_flight: 1,
                client_queue_depth: 0,
            },
            transport_flow,
            64,
            64,
        );
        stats.record_send_unavailable(
            EgfxFrameReadiness::LocalBackpressure {
                in_flight: 2,
                max: 2,
                client_queue_depth: 0,
                ack_suspended: false,
            },
            local_flow,
            64,
            64,
        );

        assert_eq!(stats.skipped_backpressure, 2);
        assert_eq!(stats.skipped_transport_backpressure, 1);
        assert_eq!(stats.skipped_local_backpressure, 1);
        assert_eq!(stats.last_frame_id, 11);
        assert_eq!(stats.last_acked_frame_id, 8);
        assert_eq!(stats.frames_in_flight, 3);
        assert!(stats.frame_ack_stream_established);
        assert_eq!(stats.total_queued_frames, 12);
        assert_eq!(stats.total_acked_frames, 9);
    }

    #[test]
    fn frame_pacer_keeps_30fps_on_quantized_60hz_events() {
        let start = Instant::now();
        let mut pacer = FramePacer::new(30, start);

        assert!(pacer.should_send(start, false, true, 30));
        assert!(!pacer.should_send(start + Duration::from_millis(16), true, true, 30));
        assert!(pacer.should_send(start + Duration::from_millis(32), true, true, 30));
        assert!(!pacer.should_send(start + Duration::from_millis(48), true, true, 30));
        assert!(pacer.should_send(start + Duration::from_millis(64), true, true, 30));
    }

    #[test]
    fn frame_pacer_does_not_burst_after_idle() {
        let start = Instant::now();
        let mut pacer = FramePacer::new(30, start);

        assert!(pacer.should_send(start, false, true, 30));
        assert!(!pacer.should_send(start + Duration::from_secs(1), true, false, 30));
        assert!(pacer.should_send(start + Duration::from_secs(1), true, true, 30));
        assert!(!pacer.should_send(start + Duration::from_secs(1), true, true, 30));
    }

    #[test]
    fn frame_pacer_keeps_30fps_on_quantized_50hz_events() {
        let start = Instant::now();
        let mut pacer = FramePacer::new(30, start);

        let sends = (0..50)
            .filter(|i| pacer.should_send(start + Duration::from_millis(i * 20), *i > 0, true, 30))
            .count();

        assert_eq!(sends, 30);
    }

    #[test]
    fn frame_pacer_uses_ack_lag_throttled_fps_without_bursting() {
        let start = Instant::now();
        let mut pacer = FramePacer::new(30, start);

        assert!(pacer.should_send(start, false, true, 30));
        assert!(!pacer.should_send(start + Duration::from_millis(32), true, true, 7));
        assert!(!pacer.should_send(start + Duration::from_millis(120), true, true, 7));
        assert!(pacer.should_send(start + Duration::from_millis(130), true, true, 7));
        assert!(!pacer.should_send(start + Duration::from_millis(160), true, true, 7));
    }

    #[test]
    fn frame_processor_selects_avc420_when_avc444_dimensions_are_unsupported() {
        let width = 18;
        let height = 16;
        let stride = width * 4;
        let (shared, _event_rx) = negotiated_avc444_egfx(width as u16, height as u16);
        let (display_tx, mut display_rx) = mpsc::channel(4);
        let frame = gradient_bgra_frame(width, height, stride);

        let mut processor = FrameProcessor::new(
            Some(shared),
            width as u32,
            height as u32,
            PixelFormat::BgrA32,
            stride as u32,
            1_000_000,
            23,
            H264RateControl::Vbr,
            30,
        );
        processor.queue_damage(&[(0, 0, width as i32, height as i32)]);

        assert!(processor.process(&frame, &display_tx));
        assert_eq!(processor.egfx_codec, Some(EgfxCodec::Avc420));
        assert!(processor.sent_first_frame);
        assert!(processor.pending_damage_regions.is_empty());
        assert!(display_rx.try_recv().is_err());
    }

    #[test]
    fn frame_processor_selects_avc420_when_policy_forces_avc420() {
        let width = 64;
        let height = 64;
        let stride = width * 4;
        let (shared, _event_rx) =
            negotiated_egfx_with_policy(width as u16, height as u16, EgfxCodecPolicy::Avc420);
        assert!(!shared.is_avc444_enabled());
        let (display_tx, mut display_rx) = mpsc::channel(4);
        let frame = gradient_bgra_frame(width, height, stride);

        let mut processor = FrameProcessor::new(
            Some(shared),
            width as u32,
            height as u32,
            PixelFormat::BgrA32,
            stride as u32,
            1_000_000,
            23,
            H264RateControl::Vbr,
            30,
        );
        processor.queue_damage(&[(0, 0, width as i32, height as i32)]);

        assert!(processor.process(&frame, &display_tx));
        assert_eq!(processor.egfx_codec, Some(EgfxCodec::Avc420));
        assert!(processor.sent_first_frame);
        assert!(processor.pending_damage_regions.is_empty());
        assert!(display_rx.try_recv().is_err());
    }

    #[test]
    fn frame_processor_sends_bitmap_when_egfx_is_not_configured() {
        let width = 8;
        let height = 4;
        let stride = width * 4 + 8;
        let (display_tx, mut display_rx) = mpsc::channel(4);
        let frame = gradient_bgra_frame(width, height, stride);

        let mut processor = FrameProcessor::new(
            None,
            width as u32,
            height as u32,
            PixelFormat::BgrA32,
            stride as u32,
            1_000_000,
            23,
            H264RateControl::Vbr,
            30,
        );
        processor.queue_damage(&[(0, 0, width as i32, height as i32)]);

        assert!(processor.process(&frame, &display_tx));
        assert!(processor.sent_first_frame);
        assert!(processor.pending_damage_regions.is_empty());
        assert_bitmap_update(
            &mut display_rx,
            width,
            height,
            stride,
            PixelFormat::BgrA32,
            &frame,
        );
    }

    #[test]
    fn bitmap_output_downscaling_uses_presentation_dimensions_stride_and_payload() {
        let source_width = 4;
        let source_height = 2;
        let source_stride = source_width * 4;
        let source = gradient_bgra_frame(source_width, source_height, source_stride);
        let prepared = prepare_presentation_frame(
            &source,
            source_width as u32,
            source_height as u32,
            source_stride as u32,
            PixelFormat::BgrA32,
            &[(0, 0, source_width as i32, source_height as i32)],
            &output_layout_snapshot(
                (source_width as u32, source_height as u32),
                (source_width as u32, 4),
            ),
        )
        .expect("scaled frame prepares");

        let (display_tx, mut display_rx) = mpsc::channel(4);
        let mut processor = FrameProcessor::new(
            None,
            prepared.width,
            prepared.height,
            PixelFormat::BgrA32,
            prepared.stride,
            1_000_000,
            23,
            H264RateControl::Vbr,
            30,
        );
        processor.queue_damage(&prepared.damage_regions);

        assert!(processor.process(prepared.data.as_ref(), &display_tx));
        assert!(processor.sent_first_frame);
        assert!(processor.pending_damage_regions.is_empty());
        assert_bitmap_update(
            &mut display_rx,
            prepared.width as usize,
            prepared.height as usize,
            prepared.stride as usize,
            PixelFormat::BgrA32,
            prepared.data.as_ref(),
        );
    }

    #[test]
    fn frame_processor_waits_for_egfx_negotiation_before_bitmap_fallback() {
        let width = 16;
        let height = 16;
        let stride = width * 4;
        let shared = unnegotiated_egfx_shared(width as u16, height as u16, EgfxCodecPolicy::Auto);

        let (display_tx, mut display_rx) = mpsc::channel(4);
        let frame = gradient_bgra_frame(width, height, stride);
        let mut processor = FrameProcessor::new(
            Some(shared),
            width as u32,
            height as u32,
            PixelFormat::BgrA32,
            stride as u32,
            1_000_000,
            23,
            H264RateControl::Vbr,
            30,
        );
        processor.queue_damage(&[(0, 0, width as i32, height as i32)]);

        assert!(processor.process(&frame, &display_tx));
        assert!(!processor.sent_first_frame);
        assert!(processor.has_pending_damage());
        assert!(display_rx.try_recv().is_err());
    }

    #[test]
    fn frame_processor_falls_back_after_egfx_activation_timeout() {
        let width = 16;
        let height = 16;
        let stride = width * 4;
        let shared = unnegotiated_egfx_shared(width as u16, height as u16, EgfxCodecPolicy::Auto);

        let (display_tx, mut display_rx) = mpsc::channel(4);
        let frame = gradient_bgra_frame(width, height, stride);
        let mut processor = FrameProcessor::new(
            Some(shared),
            width as u32,
            height as u32,
            PixelFormat::BgrA32,
            stride as u32,
            1_000_000,
            23,
            H264RateControl::Vbr,
            30,
        );

        let start = Instant::now();
        processor.queue_damage(&[(0, 0, width as i32, height as i32)]);
        assert!(processor.process_at(&frame, &display_tx, start));
        assert!(
            display_rx.try_recv().is_err(),
            "the activation sequence is still worth waiting for"
        );
        assert!(processor.has_pending_damage());
        assert!(!processor.sent_first_frame);

        processor.queue_damage(&[(0, 0, width as i32, height as i32)]);
        assert!(processor.process_at(&frame, &display_tx, start + GFX_READY_GRACE));

        assert!(
            matches!(display_rx.try_recv(), Ok(DisplayUpdate::Bitmap(_))),
            "past the grace the client must get pixels, not silence"
        );
        assert!(processor.sent_first_frame);
        assert!(!processor.has_pending_damage());
    }

    #[test]
    fn rebuilt_frame_processor_keeps_connection_activation_deadline() {
        let width = 16;
        let height = 16;
        let stride = width * 4;
        let shared = unnegotiated_egfx_shared(width as u16, height as u16, EgfxCodecPolicy::Auto);
        let frame = gradient_bgra_frame(width, height, stride);
        let build = |shared: &Arc<crate::egfx::EgfxShared>| {
            FrameProcessor::new(
                Some(Arc::clone(shared)),
                width as u32,
                height as u32,
                PixelFormat::BgrA32,
                stride as u32,
                1_000_000,
                23,
                H264RateControl::Vbr,
                30,
            )
        };

        let start = Instant::now();
        let (first_tx, mut first_rx) = mpsc::channel(4);
        let mut first = build(&shared);
        first.queue_damage(&[(0, 0, width as i32, height as i32)]);
        assert!(first.process_at(&frame, &first_tx, start));
        assert!(first_rx.try_recv().is_err());
        drop(first);

        let (second_tx, mut second_rx) = mpsc::channel(4);
        let mut second = build(&shared);
        second.queue_damage(&[(0, 0, width as i32, height as i32)]);
        assert!(second.process_at(&frame, &second_tx, start + GFX_READY_GRACE));

        assert!(
            matches!(second_rx.try_recv(), Ok(DisplayUpdate::Bitmap(_))),
            "the wait belongs to the connection, not to whichever processor is current"
        );
    }

    #[test]
    fn frame_processor_delays_initial_resize_until_egfx_channel_ready() {
        let width = 16;
        let height = 16;
        let stride = width * 4;
        let shared = unnegotiated_egfx_shared(width as u16, height as u16, EgfxCodecPolicy::Auto);

        let (display_tx, mut display_rx) = mpsc::channel(4);
        let frame = gradient_bgra_frame(width, height, stride);
        let mut processor = FrameProcessor::new(
            Some(shared),
            width as u32,
            height as u32,
            PixelFormat::BgrA32,
            stride as u32,
            1_000_000,
            23,
            H264RateControl::Vbr,
            30,
        );
        processor.set_pending_initial_resize(Some(DesktopSize {
            width: width as u16,
            height: height as u16,
        }));
        processor.queue_damage(&[(0, 0, width as i32, height as i32)]);

        assert!(processor.process(&frame, &display_tx));
        assert!(!processor.sent_first_frame);
        assert!(processor.has_pending_damage());
        assert!(display_rx.try_recv().is_err());
    }

    #[test]
    fn frame_processor_sends_initial_resize_before_first_egfx_frame_when_ready() {
        let width = 16;
        let height = 16;
        let stride = width * 4;
        let (shared, mut event_rx) = negotiated_avc444_egfx(width as u16, height as u16);
        let (display_tx, mut display_rx) = mpsc::channel(4);
        let frame = gradient_bgra_frame(width, height, stride);

        let mut processor = FrameProcessor::new(
            Some(shared),
            width as u32,
            height as u32,
            PixelFormat::BgrA32,
            stride as u32,
            1_000_000,
            23,
            H264RateControl::Vbr,
            30,
        );
        processor.set_pending_initial_resize(Some(DesktopSize {
            width: width as u16,
            height: height as u16,
        }));
        processor.queue_damage(&[(0, 0, width as i32, height as i32)]);

        assert!(!processor.process(&frame, &display_tx));
        assert!(!processor.sent_first_frame);
        assert!(processor.has_pending_damage());
        drain_gfx_pdus(&mut event_rx).assert_empty();
        match display_rx.try_recv().expect("resize update is emitted") {
            DisplayUpdate::Resize(size) => {
                assert_eq!(size.width, width as u16);
                assert_eq!(size.height, height as u16);
            }
            _ => panic!("expected resize update before first EGFX frame"),
        }
    }

    #[test]
    fn frame_processor_sends_clearcodec_when_egfx_client_has_no_avc() {
        let width = 16;
        let height = 16;
        let stride = width * 4;
        let (shared, mut event_rx) = negotiated_no_avc_egfx(width as u16, height as u16);
        let (display_tx, mut display_rx) = mpsc::channel(4);
        let frame = gradient_bgra_frame(width, height, stride);

        let mut processor = FrameProcessor::new(
            Some(shared),
            width as u32,
            height as u32,
            PixelFormat::BgrA32,
            stride as u32,
            1_000_000,
            23,
            H264RateControl::Vbr,
            30,
        );
        processor.queue_damage(&[(0, 0, width as i32, height as i32)]);

        assert!(processor.process(&frame, &display_tx));
        assert!(processor.sent_first_frame);
        assert!(processor.pending_damage_regions.is_empty());
        let trace = drain_gfx_pdus(&mut event_rx);
        trace.assert_initial_surface_setup_precedes_logical_frame(width as u16, height as u16);
        ClearCodecFrameOracle::new().assert_frame(
            &trace,
            processor.egfx_surface_id.unwrap(),
            &frame,
            width,
            height,
            stride,
            &[(0, 0, width as i32, height as i32)],
        );
        assert!(display_rx.try_recv().is_err());
        assert_eq!(processor.stats.sent_frames, 1);
        assert!(matches!(
            processor.stats.last_codec,
            Some(FrameStatsCodec::ClearCodec)
        ));
    }

    fn clearcodec_processor(
        shared: Arc<EgfxShared>,
        width: u32,
        height: u32,
        stride: u32,
    ) -> FrameProcessor {
        FrameProcessor::new(
            Some(shared),
            width,
            height,
            PixelFormat::BgrA32,
            stride,
            1_000_000,
            23,
            H264RateControl::Vbr,
            30,
        )
    }

    #[test]
    fn clearcodec_capture_formats_decode_to_expected_rgb_with_padded_stride() {
        let expected = [
            10, 20, 200, 255, 30, 40, 150, 255, 0, 0, 0, 0, 50, 60, 100, 255, 70, 80, 250, 255, 0,
            0, 0, 0,
        ];
        let bgr = [
            10, 20, 200, 0, 30, 40, 150, 19, 99, 99, 99, 99, 50, 60, 100, 50, 70, 80, 250, 100, 99,
            99, 99, 99,
        ];
        let rgb = [
            200, 20, 10, 0, 150, 40, 30, 19, 99, 99, 99, 99, 100, 60, 50, 50, 250, 80, 70, 100, 99,
            99, 99, 99,
        ];
        for (format, source) in [
            (PixelFormat::BgrA32, bgr),
            (PixelFormat::BgrX32, bgr),
            (PixelFormat::RgbA32, rgb),
            (PixelFormat::RgbX32, rgb),
        ] {
            let mut session = negotiated_no_avc_session(2, 2);
            let mut processor = clearcodec_processor(Arc::clone(&session.shared), 2, 2, 12);
            processor.pixel_format = format;
            let (display_tx, mut display_rx) = mpsc::channel(4);
            processor.queue_damage(&[(0, 0, 2, 2)]);
            assert!(processor.process(&source, &display_tx), "format {format:?}");
            ClearCodecFrameOracle::new().assert_frame(
                &drain_gfx_pdus(&mut session.event_rx),
                processor.egfx_surface_id.unwrap(),
                &expected,
                2,
                2,
                12,
                &[(0, 0, 2, 2)],
            );
            assert!(display_rx.try_recv().is_err());
        }
    }

    #[test]
    fn clearcodec_capture_invalid_input_or_closed_transport_keeps_damage_uncommitted() {
        for case in 0..4 {
            let mut session = negotiated_no_avc_session(16, 16);
            let mut processor = clearcodec_processor(Arc::clone(&session.shared), 16, 16, 64);
            let mut frame = vec![31; 16 * 16 * 4];
            match case {
                0 => frame.truncate(12),
                1 => processor.stride = 63,
                2 => processor.pixel_format = PixelFormat::ARgb32,
                _ => session.event_rx.close(),
            }
            let (display_tx, mut display_rx) = mpsc::channel(4);
            processor.queue_damage(&[(0, 0, 16, 16)]);
            assert!(!processor.process(&frame, &display_tx));
            assert!(!processor.sent_first_frame);
            assert!(!processor.pending_damage_regions.is_empty());
            assert_eq!(processor.stats.sent_frames, 0);
            assert_eq!(session.shared.frame_flow_snapshot().total_queued_frames, 0);
            drain_gfx_pdus(&mut session.event_rx).assert_no_encoded_frame();
            assert!(display_rx.try_recv().is_err());
        }
    }

    #[test]
    fn clearcodec_capture_retains_damage_under_backpressure_and_resumes_after_ack() {
        let mut session = negotiated_no_avc_session(160, 80);
        let mut processor = clearcodec_processor(Arc::clone(&session.shared), 160, 80, 648);
        let (display_tx, mut display_rx) = mpsc::channel(4);
        let mut oracle = ClearCodecFrameOracle::new();
        let mut frame = vec![0; 648 * 80];
        let full = [(0, 0, 160, 80)];
        processor.queue_damage(&full);
        assert!(processor.process(&frame, &display_tx));
        let sid = processor.egfx_surface_id.unwrap();
        let id = oracle.assert_frame(
            &drain_gfx_pdus(&mut session.event_rx),
            sid,
            &frame,
            160,
            80,
            648,
            &full,
        );
        ack_frame(&mut session.bridge, id, TestQueueDepth::AvailableBytes(0));

        let damage = [(1, 2, 4, 5), (130, 20, 5, 5)];
        // Fill the ACK window with distinct frames; the oracle decodes every payload.
        let mut last_id = id;
        for value in 1..=DEFAULT_MAX_FRAMES_IN_FLIGHT {
            frame[2 * 648 + 4] = value as u8;
            frame[20 * 648 + 130 * 4] = value as u8;
            processor.queue_damage(&damage);
            assert!(processor.process(&frame, &display_tx));
            last_id = oracle.assert_frame(
                &drain_gfx_pdus(&mut session.event_rx),
                sid,
                &frame,
                160,
                80,
                648,
                &damage,
            );
        }
        assert_eq!(
            session.shared.frame_flow_snapshot().frames_in_flight,
            DEFAULT_MAX_FRAMES_IN_FLIGHT
        );
        frame[2 * 648 + 4] = 99;
        frame[20 * 648 + 130 * 4] = 99;
        processor.queue_damage(&damage);
        let sent = processor.stats.sent_frames;
        assert!(processor.process(&frame, &display_tx));
        drain_gfx_pdus(&mut session.event_rx).assert_empty();
        assert_eq!(processor.stats.sent_frames, sent);
        assert!(!processor.pending_damage_regions.is_empty());
        ack_frame(
            &mut session.bridge,
            last_id,
            TestQueueDepth::AvailableBytes(0),
        );
        assert!(processor.process(&frame, &display_tx));
        let id = oracle.assert_frame(
            &drain_gfx_pdus(&mut session.event_rx),
            sid,
            &frame,
            160,
            80,
            648,
            &damage,
        );
        ack_frame(&mut session.bridge, id, TestQueueDepth::AvailableBytes(0));
        assert!(processor.pending_damage_regions.is_empty());
        assert_eq!(processor.stats.sent_frames, sent + 1);
        // Unchanged candidates must not keep producing frames after reference commit.
        processor.queue_damage(&damage);
        assert!(processor.process(&frame, &display_tx));
        drain_gfx_pdus(&mut session.event_rx).assert_empty();
        assert!(display_rx.try_recv().is_err());
    }

    #[test]
    fn clearcodec_capture_rebuild_resize_and_full_refresh_preserve_session_sequence() {
        let mut session = negotiated_no_avc_session(16, 16);
        let (display_tx, mut display_rx) = mpsc::channel(4);
        let mut oracle = ClearCodecFrameOracle::new();
        for (index, width) in [16, 16, 24].into_iter().enumerate() {
            if index == 2 {
                session.shared.prepare_for_resize(width, 16);
            }
            let mut processor = clearcodec_processor(
                Arc::clone(&session.shared),
                width.into(),
                16,
                u32::from(width) * 4,
            );
            let frame = vec![index as u8; usize::from(width) * 16 * 4];
            let full = [(0, 0, i32::from(width), 16)];
            processor.queue_damage(&full);
            assert!(processor.process(&frame, &display_tx));
            let sid = processor.egfx_surface_id.unwrap();
            let id = oracle.assert_frame(
                &drain_gfx_pdus(&mut session.event_rx),
                sid,
                &frame,
                width.into(),
                16,
                usize::from(width) * 4,
                &full,
            );
            session.shared.request_full_frame();
            assert!(processor.process(&frame, &display_tx));
            drain_gfx_pdus(&mut session.event_rx).assert_empty();
            assert!(session.shared.full_frame_requested());
            ack_frame(&mut session.bridge, id, TestQueueDepth::AvailableBytes(0));
            assert!(processor.process(&frame, &display_tx));
            let id = oracle.assert_frame(
                &drain_gfx_pdus(&mut session.event_rx),
                sid,
                &frame,
                width.into(),
                16,
                usize::from(width) * 4,
                &full,
            );
            ack_frame(&mut session.bridge, id, TestQueueDepth::AvailableBytes(0));
            assert!(!session.shared.full_frame_requested());
        }
        assert!(display_rx.try_recv().is_err());
    }

    #[test]
    fn bitmap_fallback_preserves_alpha_rgb_order_and_padding() {
        let width = 3;
        let height = 2;
        let stride = width * 4 + 5;
        let mut frame = vec![0xee; stride * height];
        let pixels = [
            [0x10, 0x20, 0x30, 0x40],
            [0x50, 0x60, 0x70, 0x80],
            [0x90, 0xa0, 0xb0, 0xc0],
            [0x01, 0x02, 0x03, 0x04],
            [0x05, 0x06, 0x07, 0x08],
            [0x09, 0x0a, 0x0b, 0x0c],
        ];
        for (index, pixel) in pixels.iter().enumerate() {
            let row = index / width;
            let x = index % width;
            let offset = row * stride + x * 4;
            frame[offset..offset + 4].copy_from_slice(pixel);
        }

        let (display_tx, mut display_rx) = mpsc::channel(4);
        let mut processor = FrameProcessor::new(
            None,
            width as u32,
            height as u32,
            PixelFormat::BgrA32,
            stride as u32,
            1_000_000,
            23,
            H264RateControl::Vbr,
            30,
        );
        processor.queue_damage(&[(0, 0, width as i32, height as i32)]);

        assert!(processor.process(&frame, &display_tx));
        assert_bitmap_update(
            &mut display_rx,
            width,
            height,
            stride,
            PixelFormat::BgrA32,
            &frame,
        );
    }

    #[test]
    fn frame_processor_sends_bitmap_when_negotiated_avc_encoder_is_unavailable() {
        let width = 17;
        let height = 16;
        let stride = width * 4;
        let (shared, _event_rx) = negotiated_avc444_egfx(width as u16, height as u16);
        let (display_tx, mut display_rx) = mpsc::channel(4);
        let frame = gradient_bgra_frame(width, height, stride);

        let mut processor = FrameProcessor::new(
            Some(shared),
            width as u32,
            height as u32,
            PixelFormat::BgrA32,
            stride as u32,
            1_000_000,
            23,
            H264RateControl::Vbr,
            30,
        );
        processor.queue_damage(&[(0, 0, width as i32, height as i32)]);

        assert!(processor.process(&frame, &display_tx));
        assert!(processor.h264_encoder.is_none());
        assert!(!processor.egfx_active);
        assert!(processor.sent_first_frame);
        assert!(processor.pending_damage_regions.is_empty());
        assert_bitmap_update(
            &mut display_rx,
            width,
            height,
            stride,
            PixelFormat::BgrA32,
            &frame,
        );
    }

    #[test]
    fn frame_processor_does_not_emit_bitmap_for_invalid_egfx_encode_input() {
        let width = 64;
        let height = 64;
        let stride = width * 4;
        let (shared, mut event_rx) =
            negotiated_egfx_with_policy(width as u16, height as u16, EgfxCodecPolicy::Auto);
        let (display_tx, mut display_rx) = mpsc::channel(4);
        let short_frame = vec![0x7f; stride * (height - 1)];

        let mut processor = FrameProcessor::new(
            Some(shared),
            width as u32,
            height as u32,
            PixelFormat::BgrA32,
            stride as u32,
            1_000_000,
            23,
            H264RateControl::Vbr,
            30,
        );
        processor.queue_damage(&[(0, 0, width as i32, height as i32)]);

        assert!(processor.process(&short_frame, &display_tx));
        assert_eq!(processor.encode_failures, 1);
        assert!(!processor.sent_first_frame);
        assert!(processor.has_pending_damage());
        assert!(display_rx.try_recv().is_err());
        let setup_pdus = drain_gfx_pdus(&mut event_rx);
        setup_pdus.assert_no_encoded_frame();
    }

    #[test]
    fn frame_processor_auto_policy_switches_repeated_vaapi_failures_to_software() {
        let width = 64;
        let height = 64;
        let stride = width * 4;
        let (shared, mut event_rx) =
            negotiated_egfx_with_policy(width as u16, height as u16, EgfxCodecPolicy::Avc420);
        let (display_tx, mut display_rx) = mpsc::channel(4);
        let frame = gradient_bgra_frame(width, height, stride);
        let mut processor = FrameProcessor::new(
            Some(Arc::clone(&shared)),
            width as u32,
            height as u32,
            PixelFormat::BgrA32,
            stride as u32,
            1_000_000,
            23,
            H264RateControl::Vbr,
            30,
        );
        processor.egfx_ready = true;
        processor.egfx_generation = shared.generation();
        processor.egfx_codec = Some(EgfxCodec::Avc420);
        processor.egfx_handle = shared.get_handle();
        processor.egfx_sender = shared.get_event_sender();
        processor.egfx_surface_id = shared.init_or_reuse_surface(
            processor.egfx_handle.as_ref().expect("EGFX handle"),
            processor.egfx_sender.as_ref().expect("EGFX sender"),
            width as u16,
            height as u16,
        );
        processor.egfx_active = true;
        processor.h264_encoder = Some(crate::egfx::FrameEncoder::failing_vaapi_for_test());
        processor.queue_damage(&[(0, 0, width as i32, height as i32)]);
        let setup_pdus = drain_gfx_pdus(&mut event_rx);
        let surface_id = setup_pdus.first_created_surface_id();
        assert!(setup_pdus.contains_map_surface_to_output(surface_id));
        setup_pdus.assert_no_encoded_frame();

        for expected_failures in 1..MAX_ENCODE_FAILURES {
            assert!(processor.process(&frame, &display_tx));
            assert_eq!(processor.encode_failures, expected_failures);
            assert!(!processor.sent_first_frame);
            assert!(processor.has_pending_damage());
            assert!(display_rx.try_recv().is_err());
            drain_gfx_pdus(&mut event_rx).assert_empty();
        }

        assert!(processor.process(&frame, &display_tx));

        assert_eq!(processor.encode_failures, 0);
        assert_eq!(
            processor
                .h264_encoder
                .as_ref()
                .map(crate::egfx::FrameEncoder::backend_name),
            Some("openh264")
        );
        assert!(processor.sent_first_frame);
        assert!(!processor.has_pending_damage());
        assert!(processor.egfx_surface_id.is_none());
        assert_bitmap_update(
            &mut display_rx,
            width,
            height,
            stride,
            PixelFormat::BgrA32,
            &frame,
        );
    }

    #[test]
    fn frame_processor_forced_vaapi_never_switches_to_software() {
        let width = 64;
        let height = 64;
        let stride = width * 4;
        let (shared, mut event_rx) =
            negotiated_egfx_with_policy(width as u16, height as u16, EgfxCodecPolicy::Avc420);
        let (display_tx, mut display_rx) = mpsc::channel(4);
        let frame = gradient_bgra_frame(width, height, stride);
        let mut processor = FrameProcessor::new_with_h264_backend(
            Some(Arc::clone(&shared)),
            width as u32,
            height as u32,
            PixelFormat::BgrA32,
            stride as u32,
            1_000_000,
            23,
            H264RateControl::Vbr,
            30,
            H264BackendPolicy::Vaapi,
        );
        processor.egfx_ready = true;
        processor.egfx_generation = shared.generation();
        processor.egfx_codec = Some(EgfxCodec::Avc420);
        processor.egfx_handle = shared.get_handle();
        processor.egfx_sender = shared.get_event_sender();
        processor.egfx_surface_id = shared.init_or_reuse_surface(
            processor.egfx_handle.as_ref().expect("EGFX handle"),
            processor.egfx_sender.as_ref().expect("EGFX sender"),
            width as u16,
            height as u16,
        );
        processor.egfx_active = true;
        processor.h264_encoder = Some(crate::egfx::FrameEncoder::failing_vaapi_for_test());
        processor.queue_damage(&[(0, 0, width as i32, height as i32)]);
        let setup_pdus = drain_gfx_pdus(&mut event_rx);
        setup_pdus.assert_no_encoded_frame();

        for _ in 0..MAX_ENCODE_FAILURES {
            assert!(processor.process(&frame, &display_tx));
        }

        assert!(processor.h264_encoder.is_none());
        assert!(!processor.egfx_active);
        assert_eq!(processor.encode_failures, MAX_ENCODE_FAILURES);
        assert!(processor.sent_first_frame);
        assert!(!processor.has_pending_damage());
        assert_bitmap_update(
            &mut display_rx,
            width,
            height,
            stride,
            PixelFormat::BgrA32,
            &frame,
        );
    }

    #[test]
    fn frame_processor_emits_avc444_events_for_initial_and_followup_damage() {
        let width = 64;
        let height = 64;
        let stride = width * 4;
        let (shared, mut event_rx) = negotiated_avc444_egfx(width as u16, height as u16);
        let (display_tx, mut display_rx) = mpsc::channel(4);
        let first = gradient_bgra_frame(width, height, stride);
        let mut second = first.clone();
        second[(16 * stride) + (16 * 4)] ^= 0x7f;
        second[(17 * stride) + (17 * 4) + 1] ^= 0x3f;

        let mut processor = FrameProcessor::new(
            Some(shared),
            width as u32,
            height as u32,
            PixelFormat::BgrA32,
            stride as u32,
            1_000_000,
            23,
            H264RateControl::Cqp,
            30,
        );
        processor.queue_damage(&[(0, 0, width as i32, height as i32)]);

        assert!(processor.process(&first, &display_tx));
        assert_eq!(processor.egfx_codec, Some(EgfxCodec::Avc444));
        assert!(processor.sent_first_frame);
        assert!(processor.egfx_surface_id.is_some());
        assert!(!processor.has_pending_damage());
        assert!(display_rx.try_recv().is_err());

        let initial_pdus = drain_gfx_pdus(&mut event_rx);
        initial_pdus.assert_initial_surface_setup_precedes_logical_frame(64, 64);
        initial_pdus.assert_sendable_avc444_frame(ExpectedAvc444Encoding::LumaAndChroma);

        processor.queue_damage(&[(16, 16, 2, 2)]);
        assert!(processor.process(&second, &display_tx));
        assert!(processor.sent_first_frame);
        assert!(!processor.has_pending_damage());
        assert!(display_rx.try_recv().is_err());

        let followup_pdus = drain_gfx_pdus(&mut event_rx);
        followup_pdus.assert_sendable_avc444_frame(ExpectedAvc444Encoding::LumaAndChroma);
    }

    #[test]
    fn frame_processor_sends_full_frame_after_repeated_capabilities_refresh_request() {
        let width = 64;
        let height = 64;
        let stride = width * 4;
        let mut session = tracked_avc444_session(width as u16, height as u16, 3);
        let shared = Arc::clone(&session.shared);
        let (display_tx, mut display_rx) = mpsc::channel(4);
        let frame = gradient_bgra_frame(width, height, stride);

        let mut processor = FrameProcessor::new(
            Some(Arc::clone(&shared)),
            width as u32,
            height as u32,
            PixelFormat::BgrA32,
            stride as u32,
            1_000_000,
            23,
            H264RateControl::Cqp,
            30,
        );
        processor.queue_damage(&[(0, 0, width as i32, height as i32)]);

        assert!(processor.process(&frame, &display_tx));
        assert!(processor.sent_first_frame);
        assert!(!processor.has_pending_damage());
        assert!(display_rx.try_recv().is_err());
        let initial_pdus = drain_gfx_pdus(&mut session.event_rx);
        initial_pdus.assert_sendable_avc444_frame(ExpectedAvc444Encoding::LumaAndChroma);
        let initial_frame_id = initial_pdus.frame_id();
        ack_frame(
            &mut session.bridge,
            initial_frame_id,
            TestQueueDepth::AvailableBytes(1),
        );

        let force_idr_requests = processor
            .h264_encoder
            .as_ref()
            .and_then(crate::egfx::FrameEncoder::force_idr_requests_for_test);
        shared.request_full_frame();
        assert!(shared.full_frame_requested());
        assert!(processor.process(&frame, &display_tx));
        assert!(!shared.full_frame_requested());
        assert!(!processor.has_pending_damage());
        assert!(display_rx.try_recv().is_err());

        let refresh_pdus = drain_gfx_pdus(&mut session.event_rx);
        refresh_pdus.assert_sendable_avc444_frame(ExpectedAvc444Encoding::LumaAndChroma);
        let (last_luma_regions, last_chroma_regions) = processor
            .h264_encoder
            .as_ref()
            .and_then(crate::egfx::FrameEncoder::avc444_last_reference_regions_for_test)
            .expect("AVC444 region state exists");
        let full_frame = [(0, 0, width as i32, height as i32)];
        assert_eq!(last_luma_regions, full_frame);
        assert_eq!(last_chroma_regions, full_frame);
        assert_eq!(
            processor
                .h264_encoder
                .as_ref()
                .and_then(crate::egfx::FrameEncoder::force_idr_requests_for_test),
            force_idr_requests
        );
    }

    #[test]
    fn frame_processor_keeps_repeated_capabilities_refresh_pending_until_ack_window_drains() {
        let width = 64;
        let height = 64;
        let stride = width * 4;
        let mut session = tracked_avc444_session(width as u16, height as u16, 3);
        let shared = Arc::clone(&session.shared);
        let (display_tx, mut display_rx) = mpsc::channel(4);
        let first = gradient_bgra_frame(width, height, stride);
        let mut second = first.clone();
        let mut third = first.clone();
        for y in 8..16 {
            for x in 8..16 {
                write_bgra_pixel(&mut second, stride, x, y, 192, 192, 192);
                write_bgra_pixel(&mut third, stride, x, y, 64, 192, 64);
            }
        }

        let mut processor = FrameProcessor::new(
            Some(Arc::clone(&shared)),
            width as u32,
            height as u32,
            PixelFormat::BgrA32,
            stride as u32,
            1_000_000,
            23,
            H264RateControl::Cqp,
            30,
        );
        processor.queue_damage(&[(0, 0, width as i32, height as i32)]);

        assert!(processor.process(&first, &display_tx));
        let first_pdus = drain_gfx_pdus(&mut session.event_rx);
        let first_frame_id = first_pdus.frame_id();
        ack_frame(
            &mut session.bridge,
            first_frame_id,
            TestQueueDepth::AvailableBytes(1),
        );
        assert_eq!(shared.frame_flow_snapshot().frames_in_flight, 0);

        processor.queue_damage(&[(8, 8, 8, 8)]);
        assert!(processor.process(&second, &display_tx));
        let second_pdus = drain_gfx_pdus(&mut session.event_rx);
        let second_frame_id = second_pdus.frame_id();
        assert_eq!(shared.frame_flow_snapshot().frames_in_flight, 1);
        assert!(shared.can_send_frame(&session.handle));

        shared.request_full_frame();
        processor.queue_damage(&[(8, 8, 8, 8)]);
        assert!(processor.process(&third, &display_tx));
        assert!(shared.full_frame_requested());
        assert!(processor.has_pending_damage());
        assert_eq!(processor.stats.skipped_local_backpressure, 1);
        drain_gfx_pdus(&mut session.event_rx).assert_empty();

        ack_frame(
            &mut session.bridge,
            second_frame_id,
            TestQueueDepth::AvailableBytes(1),
        );
        assert_eq!(shared.frame_flow_snapshot().frames_in_flight, 0);
        assert!(processor.process(&third, &display_tx));
        assert!(!shared.full_frame_requested());
        assert!(!processor.has_pending_damage());
        assert!(display_rx.try_recv().is_err());

        let refresh_pdus = drain_gfx_pdus(&mut session.event_rx);
        refresh_pdus.assert_sendable_avc444_frame(ExpectedAvc444Encoding::LumaAndChroma);
        let full_frame = [(0, 0, width as i32, height as i32)];
        let (last_luma_regions, last_chroma_regions) = processor
            .h264_encoder
            .as_ref()
            .and_then(crate::egfx::FrameEncoder::avc444_last_reference_regions_for_test)
            .expect("AVC444 region state exists");
        assert_eq!(last_luma_regions, full_frame);
        assert_eq!(last_chroma_regions, full_frame);
    }

    #[test]
    fn frame_processor_recovers_avc444_with_full_luma_and_chroma_after_send_failure() {
        let width = 64;
        let height = 64;
        let stride = width * 4;
        let (shared, event_rx) = negotiated_avc444_egfx(width as u16, height as u16);
        let (display_tx, mut display_rx) = mpsc::channel(4);
        let first = gradient_bgra_frame(width, height, stride);
        let mut second = first.clone();
        second[4] = 0x40;

        let mut processor = FrameProcessor::new(
            Some(shared),
            width as u32,
            height as u32,
            PixelFormat::BgrA32,
            stride as u32,
            1_000_000,
            23,
            H264RateControl::Cqp,
            30,
        );
        processor.queue_damage(&[(0, 0, width as i32, height as i32)]);

        assert!(processor.process(&first, &display_tx));
        assert_eq!(processor.egfx_codec, Some(EgfxCodec::Avc444));
        assert!(processor.sent_first_frame);
        let committed_before = processor
            .h264_encoder
            .as_ref()
            .and_then(crate::egfx::FrameEncoder::avc444_luma_reference_y_for_test)
            .expect("AVC444 reference committed after successful send")
            .to_vec();

        drop(event_rx);
        processor.queue_damage(&[(0, 0, 4, 2)]);
        assert!(processor.process(&second, &display_tx));

        let committed_after = processor
            .h264_encoder
            .as_ref()
            .and_then(crate::egfx::FrameEncoder::avc444_luma_reference_y_for_test)
            .expect("AVC444 reference remains available")
            .to_vec();
        assert_eq!(committed_after, committed_before);
        assert!(processor.has_pending_damage());
        assert!(display_rx.try_recv().is_err());

        let (recovery_tx, mut recovery_rx) = mpsc::unbounded_channel();
        processor.egfx_sender = Some(recovery_tx);
        assert!(processor.process(&second, &display_tx));

        let committed_recovered = processor
            .h264_encoder
            .as_ref()
            .and_then(crate::egfx::FrameEncoder::avc444_luma_reference_y_for_test)
            .expect("AVC444 reference committed after recovery send")
            .to_vec();
        assert_ne!(committed_recovered, committed_before);
        let (last_luma_regions, last_chroma_regions) = processor
            .h264_encoder
            .as_ref()
            .and_then(crate::egfx::FrameEncoder::avc444_last_reference_regions_for_test)
            .expect("AVC444 region state exists");
        let full_frame = [(0, 0, width as i32, height as i32)];
        assert_eq!(last_luma_regions, full_frame);
        assert_eq!(last_chroma_regions, full_frame);
        assert!(!processor.has_pending_damage());
        assert!(display_rx.try_recv().is_err());
        assert!(recovery_rx.try_recv().is_ok());
    }

    #[test]
    fn frame_processor_backpressure_preserves_avc444_reference_and_pending_damage() {
        let width = 64;
        let height = 64;
        let stride = width * 4;
        let mut session = tracked_avc444_session(width as u16, height as u16, 1);
        let shared = Arc::clone(&session.shared);

        let (display_tx, mut display_rx) = mpsc::channel(4);
        let first = gradient_bgra_frame(width, height, stride);
        let mut second = first.clone();
        second[16 * stride + 16 * 4] ^= 0x7f;
        second[17 * stride + 17 * 4 + 1] ^= 0x3f;
        let mut third = second.clone();
        third[24 * stride + 24 * 4] ^= 0x5f;

        let mut processor = FrameProcessor::new(
            Some(Arc::clone(&shared)),
            width as u32,
            height as u32,
            PixelFormat::BgrA32,
            stride as u32,
            1_000_000,
            23,
            H264RateControl::Cqp,
            30,
        );
        processor.queue_damage(&[(0, 0, width as i32, height as i32)]);

        assert!(processor.process(&first, &display_tx));
        assert!(!processor.has_pending_damage());
        assert!(display_rx.try_recv().is_err());
        let initial_pdus = drain_gfx_pdus(&mut session.event_rx);
        let frame_id = initial_pdus.frame_id();

        ack_frame(
            &mut session.bridge,
            frame_id,
            TestQueueDepth::AvailableBytes(1),
        );
        assert!(shared.can_send_frame(&session.handle));

        processor.queue_damage(&[(16, 16, 2, 2)]);
        assert!(processor.process(&second, &display_tx));
        assert!(!processor.has_pending_damage());
        assert!(!shared.can_send_frame(&session.handle));
        let second_pdus = drain_gfx_pdus(&mut session.event_rx);
        let second_frame_id = second_pdus.frame_id();
        let committed_before = processor
            .h264_encoder
            .as_ref()
            .and_then(crate::egfx::FrameEncoder::avc444_luma_reference_y_for_test)
            .expect("AVC444 reference committed after second send")
            .to_vec();

        processor.queue_damage(&[(24, 24, 1, 1)]);
        assert!(processor.process(&third, &display_tx));

        assert_eq!(processor.stats.skipped_backpressure, 1);
        assert!(processor.has_pending_damage());
        assert!(!shared.can_send_frame(&session.handle));
        drain_gfx_pdus(&mut session.event_rx).assert_empty();
        let committed_after_backpressure = processor
            .h264_encoder
            .as_ref()
            .and_then(crate::egfx::FrameEncoder::avc444_luma_reference_y_for_test)
            .expect("AVC444 reference remains available after backpressure")
            .to_vec();
        assert_eq!(committed_after_backpressure, committed_before);

        ack_frame(
            &mut session.bridge,
            second_frame_id,
            TestQueueDepth::AvailableBytes(1),
        );
        assert!(shared.can_send_frame(&session.handle));
        assert!(processor.process(&third, &display_tx));
        assert!(!processor.has_pending_damage());
        let recovered_pdus = drain_gfx_pdus(&mut session.event_rx);
        recovered_pdus.assert_sendable_avc444_frame(ExpectedAvc444Encoding::Luma);
        let committed_after_recovery = processor
            .h264_encoder
            .as_ref()
            .and_then(crate::egfx::FrameEncoder::avc444_luma_reference_y_for_test)
            .expect("AVC444 reference committed after backpressure recovery")
            .to_vec();
        assert_ne!(committed_after_recovery, committed_before);
        assert!(display_rx.try_recv().is_err());
    }

    #[test]
    fn frame_processor_sends_repeated_avc444_frames_after_acks() {
        let width = 64;
        let height = 64;
        let stride = width * 4;
        let mut session = tracked_avc444_session(width as u16, height as u16, 1);
        let shared = Arc::clone(&session.shared);

        let (display_tx, mut display_rx) = mpsc::channel(4);
        let mut frame = gradient_bgra_frame(width, height, stride);
        let mut processor = FrameProcessor::new(
            Some(Arc::clone(&shared)),
            width as u32,
            height as u32,
            PixelFormat::BgrA32,
            stride as u32,
            1_000_000,
            23,
            H264RateControl::Cqp,
            30,
        );

        for index in 0..6 {
            mutate_bgra_tile(&mut frame, width, height, stride, index + 1);
            processor.queue_damage(&[(0, 0, width as i32, height as i32)]);

            assert!(
                processor.process(&frame, &display_tx),
                "frame {index} processed"
            );
            assert_eq!(processor.egfx_codec, Some(EgfxCodec::Avc444));
            assert!(processor.sent_first_frame);
            assert!(!processor.has_pending_damage());
            assert!(display_rx.try_recv().is_err());

            let pdus = drain_gfx_pdus(&mut session.event_rx);
            if index == 0 {
                pdus.assert_initial_surface_setup_precedes_logical_frame(64, 64);
            }
            pdus.assert_sendable_avc444_frame(ExpectedAvc444Encoding::LumaAndChroma);
            let frame_id = pdus.frame_id();

            assert!(!shared.can_send_frame(&session.handle));
            ack_frame(
                &mut session.bridge,
                frame_id,
                TestQueueDepth::AvailableBytes(1),
            );
            assert!(shared.can_send_frame(&session.handle));
        }

        assert_eq!(processor.stats.sent_frames, 6);
        assert_eq!(processor.stats.skipped_backpressure, 0);
    }

    #[test]
    fn frame_processor_repeated_avc444_frames_decode_with_single_h264_decoder() {
        let width = 64;
        let height = 64;
        let stride = width * 4;
        let mut oracle = Avc444PresentationOracle::new(width, height);
        let mut session = tracked_avc444_session(width as u16, height as u16, 1);
        let shared = Arc::clone(&session.shared);
        let (display_tx, mut display_rx) = mpsc::channel(4);
        let mut frame = gradient_bgra_frame(width, height, stride);
        let mut processor = FrameProcessor::new(
            Some(Arc::clone(&shared)),
            width as u32,
            height as u32,
            PixelFormat::BgrA32,
            stride as u32,
            1_000_000,
            23,
            H264RateControl::Cqp,
            30,
        );

        let mut decoded_pictures = 0usize;
        for index in 0..6 {
            mutate_bgra_tile(&mut frame, width, height, stride, index + 1);
            processor.queue_damage(&[(0, 0, width as i32, height as i32)]);

            assert!(
                processor.process(&frame, &display_tx),
                "frame {index} processed"
            );
            assert_eq!(processor.egfx_codec, Some(EgfxCodec::Avc444));
            assert!(processor.sent_first_frame);
            assert!(!processor.has_pending_damage());
            assert!(display_rx.try_recv().is_err());

            let pdus = drain_gfx_pdus(&mut session.event_rx);
            if index == 0 {
                pdus.assert_initial_surface_setup_precedes_logical_frame(
                    width as u16,
                    height as u16,
                );
            }
            decoded_pictures += oracle.assert_trace_decodes_pictures(&pdus);
            let frame_id = pdus.frame_id();

            assert!(!shared.can_send_frame(&session.handle));
            ack_frame(
                &mut session.bridge,
                frame_id,
                TestQueueDepth::AvailableBytes(1),
            );
            assert!(shared.can_send_frame(&session.handle));
        }

        assert!(decoded_pictures >= 6);
        assert_eq!(processor.stats.sent_frames, 6);
        assert_eq!(processor.stats.skipped_backpressure, 0);
        assert_eq!(processor.stats.skipped_encoder, 0);
    }

    #[test]
    fn frame_processor_repeated_avc444_frames_reconstruct_visible_progress() {
        let width = 64;
        let height = 64;
        let stride = width * 4;
        let mut oracle = Avc444PresentationOracle::new(width, height);
        let mut session = tracked_avc444_session(width as u16, height as u16, 1);
        let shared = Arc::clone(&session.shared);
        let (display_tx, mut display_rx) = mpsc::channel(4);
        let mut frame = gradient_bgra_frame(width, height, stride);
        let mut processor = FrameProcessor::new(
            Some(Arc::clone(&shared)),
            width as u32,
            height as u32,
            PixelFormat::BgrA32,
            stride as u32,
            1_000_000,
            23,
            H264RateControl::Cqp,
            30,
        );

        for index in 0..6 {
            mutate_bgra_tile(&mut frame, width, height, stride, index + 1);
            processor.queue_damage(&[(0, 0, width as i32, height as i32)]);

            assert!(
                processor.process(&frame, &display_tx),
                "frame {index} processed"
            );
            assert_eq!(processor.egfx_codec, Some(EgfxCodec::Avc444));
            assert!(processor.sent_first_frame);
            assert!(!processor.has_pending_damage());
            assert!(display_rx.try_recv().is_err());

            let pdus = drain_gfx_pdus(&mut session.event_rx);
            if index == 0 {
                pdus.assert_initial_surface_setup_precedes_logical_frame(
                    width as u16,
                    height as u16,
                );
            }
            pdus.assert_sendable_avc444_frame(ExpectedAvc444Encoding::LumaAndChroma);
            oracle.assert_trace_reconstructs_visible_progress(&pdus, index);

            let frame_id = pdus.frame_id();
            assert!(!shared.can_send_frame(&session.handle));
            ack_frame(
                &mut session.bridge,
                frame_id,
                TestQueueDepth::AvailableBytes(1),
            );
            assert!(shared.can_send_frame(&session.handle));
        }

        oracle.assert_distinct_reconstructed_frames(6);
        assert_eq!(processor.stats.sent_frames, 6);
        assert_eq!(processor.stats.skipped_backpressure, 0);
        assert_eq!(processor.stats.skipped_encoder, 0);
    }

    #[test]
    fn frame_processor_repeated_avc444_frames_keep_bounded_yuv444_error() {
        let width = 64;
        let height = 64;
        let stride = width * 4;
        let mut oracle = Avc444PresentationOracle::new(width, height);
        let mut session = tracked_avc444_session(width as u16, height as u16, 1);
        let shared = Arc::clone(&session.shared);
        let (display_tx, mut display_rx) = mpsc::channel(4);
        let mut frame = gradient_bgra_frame(width, height, stride);
        let mut processor = FrameProcessor::new(
            Some(Arc::clone(&shared)),
            width as u32,
            height as u32,
            PixelFormat::BgrA32,
            stride as u32,
            10_000_000,
            18,
            H264RateControl::Cqp,
            30,
        );

        for index in 0..6 {
            mutate_bgra_tile(&mut frame, width, height, stride, index + 1);
            processor.queue_damage(&[(0, 0, width as i32, height as i32)]);

            assert!(
                processor.process(&frame, &display_tx),
                "frame {index} processed"
            );
            assert_eq!(processor.egfx_codec, Some(EgfxCodec::Avc444));
            assert!(processor.sent_first_frame);
            assert!(!processor.has_pending_damage());
            assert!(display_rx.try_recv().is_err());

            let pdus = drain_gfx_pdus(&mut session.event_rx);
            if index == 0 {
                pdus.assert_initial_surface_setup_precedes_logical_frame(
                    width as u16,
                    height as u16,
                );
            }
            pdus.assert_sendable_avc444_frame(ExpectedAvc444Encoding::LumaAndChroma);
            oracle.assert_trace_matches_bgra_with_bounded_yuv444_error(
                &pdus, index, &frame, stride, 16.0, 0.10,
            );

            let frame_id = pdus.frame_id();
            assert!(!shared.can_send_frame(&session.handle));
            ack_frame(
                &mut session.bridge,
                frame_id,
                TestQueueDepth::AvailableBytes(1),
            );
            assert!(shared.can_send_frame(&session.handle));
        }

        oracle.assert_distinct_reconstructed_frames(6);
        assert_eq!(processor.stats.sent_frames, 6);
        assert_eq!(processor.stats.skipped_backpressure, 0);
        assert_eq!(processor.stats.skipped_encoder, 0);
    }

    #[test]
    fn frame_processor_preserves_separated_avc444_damage_regions_without_outer_union() {
        let width = 192;
        let height = 64;
        let stride = width * 4;
        let mut oracle = Avc444PresentationOracle::new(width, height);
        let mut session = tracked_avc444_session(width as u16, height as u16, 1);
        let shared = Arc::clone(&session.shared);
        let (display_tx, mut display_rx) = mpsc::channel(4);
        let first = gradient_bgra_frame(width, height, stride);
        let mut second = first.clone();
        second[(10 * stride) + (10 * 4)] ^= 0x7f;
        second[(10 * stride) + (150 * 4)] ^= 0x7f;

        let mut processor = FrameProcessor::new(
            Some(Arc::clone(&shared)),
            width as u32,
            height as u32,
            PixelFormat::BgrA32,
            stride as u32,
            10_000_000,
            18,
            H264RateControl::Cqp,
            30,
        );

        processor.queue_damage(&[(0, 0, width as i32, height as i32)]);
        assert!(processor.process(&first, &display_tx));
        assert!(display_rx.try_recv().is_err());
        let initial_pdus = drain_gfx_pdus(&mut session.event_rx);
        initial_pdus
            .assert_initial_surface_setup_precedes_logical_frame(width as u16, height as u16);
        initial_pdus.assert_sendable_avc444_frame(ExpectedAvc444Encoding::LumaAndChroma);
        oracle.assert_trace_matches_bgra_with_bounded_yuv444_error(
            &initial_pdus,
            0,
            &first,
            stride,
            16.0,
            0.10,
        );
        ack_frame(
            &mut session.bridge,
            initial_pdus.frame_id(),
            TestQueueDepth::AvailableBytes(1),
        );

        processor.queue_damage(&[(10, 10, 1, 1), (150, 10, 1, 1)]);
        assert!(processor.process(&second, &display_tx));
        assert!(!processor.has_pending_damage());
        assert!(display_rx.try_recv().is_err());
        let followup_pdus = drain_gfx_pdus(&mut session.event_rx);
        followup_pdus.assert_sendable_avc444_frame(ExpectedAvc444Encoding::Luma);
        oracle.assert_trace_matches_bgra_with_bounded_yuv444_error(
            &followup_pdus,
            1,
            &second,
            stride,
            16.0,
            0.10,
        );

        let (last_luma_regions, _last_chroma_regions) = processor
            .h264_encoder
            .as_ref()
            .and_then(crate::egfx::FrameEncoder::avc444_last_reference_regions_for_test)
            .expect("AVC444 region state exists");
        assert_eq!(last_luma_regions, [(8, 10, 4, 2), (148, 10, 4, 2)]);
    }

    #[test]
    fn frame_processor_avc444_luma_only_frame_reconstructs_visible_progress() {
        let width = 64;
        let height = 64;
        let stride = width * 4;
        let mut oracle = Avc444PresentationOracle::new(width, height);
        let mut session = tracked_avc444_session(width as u16, height as u16, 1);
        let shared = Arc::clone(&session.shared);
        let (display_tx, mut display_rx) = mpsc::channel(4);
        let mut frame = solid_bgra_frame(width, height, stride, 128, 128, 128);
        let mut processor = FrameProcessor::new(
            Some(Arc::clone(&shared)),
            width as u32,
            height as u32,
            PixelFormat::BgrA32,
            stride as u32,
            1_000_000,
            23,
            H264RateControl::Cqp,
            30,
        );

        processor.queue_damage(&[(0, 0, width as i32, height as i32)]);
        assert!(processor.process(&frame, &display_tx));
        assert!(display_rx.try_recv().is_err());
        let initial_pdus = drain_gfx_pdus(&mut session.event_rx);
        initial_pdus
            .assert_initial_surface_setup_precedes_logical_frame(width as u16, height as u16);
        initial_pdus.assert_sendable_avc444_frame(ExpectedAvc444Encoding::LumaAndChroma);
        oracle.assert_trace_reconstructs_visible_progress(&initial_pdus, 0);
        ack_frame(
            &mut session.bridge,
            initial_pdus.frame_id(),
            TestQueueDepth::AvailableBytes(1),
        );

        for y in 0..2 {
            for x in 0..4 {
                write_bgra_pixel(&mut frame, stride, x, y, 192, 192, 192);
            }
        }

        processor.queue_damage(&[(0, 0, 4, 2)]);
        assert!(processor.process(&frame, &display_tx));
        assert!(display_rx.try_recv().is_err());
        let luma_pdus = drain_gfx_pdus(&mut session.event_rx);
        luma_pdus.assert_sendable_avc444_frame(ExpectedAvc444Encoding::Luma);
        oracle.assert_trace_reconstructs_visible_progress_with_min_delta(&luma_pdus, 1, 1);

        assert_eq!(processor.stats.sent_frames, 2);
        assert_eq!(processor.stats.skipped_backpressure, 0);
        assert_eq!(processor.stats.skipped_encoder, 0);
    }

    #[test]
    fn frame_processor_avc444_chroma_only_frame_reconstructs_visible_progress() {
        let width = 64;
        let height = 64;
        let stride = width * 4;
        let mut oracle = Avc444PresentationOracle::new(width, height);
        let mut session = tracked_avc444_session(width as u16, height as u16, 1);
        let shared = Arc::clone(&session.shared);
        let (display_tx, mut display_rx) = mpsc::channel(4);
        let color_a = (0u8, 0u8, 187u8);
        let color_b = (0u8, 17u8, 17u8);
        let mut frame = solid_bgra_frame(width, height, stride, 128, 128, 128);
        write_bgra_pixel(&mut frame, stride, 0, 0, color_a.0, color_a.1, color_a.2);
        write_bgra_pixel(&mut frame, stride, 1, 0, color_b.0, color_b.1, color_b.2);
        let mut processor = FrameProcessor::new(
            Some(Arc::clone(&shared)),
            width as u32,
            height as u32,
            PixelFormat::BgrA32,
            stride as u32,
            1_000_000,
            23,
            H264RateControl::Cqp,
            30,
        );

        processor.queue_damage(&[(0, 0, width as i32, height as i32)]);
        assert!(processor.process(&frame, &display_tx));
        assert!(display_rx.try_recv().is_err());
        let initial_pdus = drain_gfx_pdus(&mut session.event_rx);
        initial_pdus
            .assert_initial_surface_setup_precedes_logical_frame(width as u16, height as u16);
        initial_pdus.assert_sendable_avc444_frame(ExpectedAvc444Encoding::LumaAndChroma);
        oracle.assert_trace_reconstructs_visible_progress(&initial_pdus, 0);
        ack_frame(
            &mut session.bridge,
            initial_pdus.frame_id(),
            TestQueueDepth::AvailableBytes(1),
        );

        write_bgra_pixel(&mut frame, stride, 0, 0, color_b.0, color_b.1, color_b.2);
        write_bgra_pixel(&mut frame, stride, 1, 0, color_a.0, color_a.1, color_a.2);

        processor.queue_damage(&[(0, 0, 4, 2)]);
        assert!(processor.process(&frame, &display_tx));
        assert!(display_rx.try_recv().is_err());
        let chroma_pdus = drain_gfx_pdus(&mut session.event_rx);
        chroma_pdus.assert_sendable_avc444_frame(ExpectedAvc444Encoding::Chroma);
        oracle.assert_trace_reconstructs_visible_progress_with_min_delta(&chroma_pdus, 1, 1);

        assert_eq!(processor.stats.sent_frames, 2);
        assert_eq!(processor.stats.skipped_backpressure, 0);
        assert_eq!(processor.stats.skipped_encoder, 0);
    }

    #[test]
    fn frame_processor_full_scan_sends_changed_avc444_frames_and_skips_unchanged() {
        let width = 64;
        let height = 64;
        let stride = width * 4;
        let mut session = tracked_avc444_session(width as u16, height as u16, 1);
        let shared = Arc::clone(&session.shared);
        let (display_tx, mut display_rx) = mpsc::channel(4);
        let mut frame = gradient_bgra_frame(width, height, stride);
        let mut processor = FrameProcessor::new(
            Some(Arc::clone(&shared)),
            width as u32,
            height as u32,
            PixelFormat::BgrA32,
            stride as u32,
            1_000_000,
            23,
            H264RateControl::Cqp,
            30,
        );

        for index in 0..4 {
            mutate_bgra_tile(&mut frame, width, height, stride, index + 1);
            processor.queue_damage(&[(0, 0, width as i32, height as i32)]);

            assert!(
                processor.process(&frame, &display_tx),
                "changed synthetic frame {index} should be processed"
            );
            assert!(processor.sent_first_frame);
            assert!(!processor.has_pending_damage());
            assert!(display_rx.try_recv().is_err());

            let pdus = drain_gfx_pdus(&mut session.event_rx);
            if index == 0 {
                pdus.assert_initial_surface_setup_precedes_logical_frame(
                    width as u16,
                    height as u16,
                );
            }
            let frame_summary =
                pdus.assert_sendable_avc444_frame(ExpectedAvc444Encoding::LumaAndChroma);
            assert!(!shared.can_send_frame(&session.handle));
            ack_frame(
                &mut session.bridge,
                frame_summary.frame_id,
                TestQueueDepth::AvailableBytes(1),
            );
            assert!(shared.can_send_frame(&session.handle));
        }

        processor.queue_damage(&[(0, 0, width as i32, height as i32)]);
        assert!(processor.process(&frame, &display_tx));
        assert_eq!(processor.stats.skipped_no_damage, 1);
        drain_gfx_pdus(&mut session.event_rx).assert_empty();

        assert_eq!(processor.stats.sent_frames, 4);
        assert_eq!(processor.stats.skipped_backpressure, 0);
        assert_eq!(processor.stats.skipped_encoder, 0);
    }

    #[test]
    fn frame_processor_preserves_dirty_avc444_frame_across_backpressure_and_resize() {
        let width = 64;
        let height = 64;
        let stride = width * 4;
        let mut session = tracked_avc444_session(width as u16, height as u16, 1);
        let shared = Arc::clone(&session.shared);
        let (display_tx, mut display_rx) = mpsc::channel(4);
        let mut frame = gradient_bgra_frame(width, height, stride);
        let mut processor = FrameProcessor::new(
            Some(Arc::clone(&shared)),
            width as u32,
            height as u32,
            PixelFormat::BgrA32,
            stride as u32,
            1_000_000,
            23,
            H264RateControl::Cqp,
            30,
        );

        mutate_bgra_tile(&mut frame, width, height, stride, 1);
        processor.queue_damage(&[(0, 0, width as i32, height as i32)]);
        assert!(processor.process(&frame, &display_tx));
        let first_pdus = drain_gfx_pdus(&mut session.event_rx);
        first_pdus.assert_initial_surface_setup_precedes_logical_frame(width as u16, height as u16);
        let first_frame_summary =
            first_pdus.assert_sendable_avc444_frame(ExpectedAvc444Encoding::LumaAndChroma);
        let first_surface_id = first_frame_summary.surface_id;
        let first_frame_id = first_pdus.frame_id();
        assert!(!processor.has_pending_damage());
        assert!(!shared.can_send_frame(&session.handle));

        ack_frame(
            &mut session.bridge,
            first_frame_id,
            TestQueueDepth::AvailableBytes(1),
        );
        assert!(shared.can_send_frame(&session.handle));

        mutate_bgra_tile(&mut frame, width, height, stride, 2);
        processor.queue_damage(&[(8, 8, 16, 16)]);
        assert!(processor.process(&frame, &display_tx));
        let second_pdus = drain_gfx_pdus(&mut session.event_rx);
        second_pdus.assert_sendable_avc444_frame(ExpectedAvc444Encoding::LumaAndChroma);
        let second_frame_id = second_pdus.frame_id();
        assert!(!shared.can_send_frame(&session.handle));

        mutate_bgra_tile(&mut frame, width, height, stride, 3);
        processor.queue_damage(&[(12, 12, 16, 16)]);
        assert!(processor.process(&frame, &display_tx));
        assert!(processor.has_pending_damage());
        assert_eq!(processor.stats.skipped_backpressure, 1);
        drain_gfx_pdus(&mut session.event_rx).assert_empty();

        ack_frame(
            &mut session.bridge,
            second_frame_id,
            TestQueueDepth::AvailableBytes(1),
        );
        assert!(shared.can_send_frame(&session.handle));
        assert!(processor.process(&frame, &display_tx));
        assert!(!processor.has_pending_damage());
        let recovered_pdus = drain_gfx_pdus(&mut session.event_rx);
        recovered_pdus.assert_sendable_avc444_frame(ExpectedAvc444Encoding::LumaAndChroma);
        let recovered_frame_id = recovered_pdus.frame_id();

        ack_frame(
            &mut session.bridge,
            recovered_frame_id,
            TestQueueDepth::AvailableBytes(1),
        );
        shared.prepare_for_resize(width as u16, height as u16);
        let resize_pdus = drain_gfx_pdus(&mut session.event_rx);
        assert!(resize_pdus.contains_delete_surface(first_surface_id));
        resize_pdus.assert_no_encoded_frame();

        mutate_bgra_tile(&mut frame, width, height, stride, 4);
        processor.queue_damage(&[(0, 0, width as i32, height as i32)]);
        assert!(processor.process(&frame, &display_tx));
        assert!(!processor.has_pending_damage());
        let resized_pdus = drain_gfx_pdus(&mut session.event_rx);
        let resized_surface_id = resized_pdus.first_created_surface_id();
        assert_ne!(resized_surface_id, first_surface_id);
        assert!(resized_pdus.contains_map_surface_to_output(resized_surface_id));
        let resized_frame_summary =
            resized_pdus.assert_sendable_avc444_frame(ExpectedAvc444Encoding::LumaAndChroma);
        assert_eq!(resized_frame_summary.surface_id, resized_surface_id);
        assert_eq!(processor.stats.sent_frames, 4);
        assert!(display_rx.try_recv().is_err());
    }

    #[test]
    fn frame_processor_restarts_avc444_surface_after_new_client_capabilities() {
        let width = 64;
        let height = 64;
        let stride = width * 4;
        let shared = Arc::new(crate::egfx::EgfxShared::with_codec_policy(
            DEFAULT_MAX_FRAMES_IN_FLIGHT,
            EgfxCodecPolicy::Avc444,
        ));
        shared.set_surface_size(width as u16, height as u16);

        let (first_event_tx, mut first_event_rx) = mpsc::unbounded_channel();
        let mut factory = HyprGfxFactory::new(Arc::clone(&shared));
        ServerEventSender::set_sender(&mut factory, first_event_tx);
        let (mut first_bridge, _) =
            GfxServerFactory::build_server_with_handle(&factory).expect("EGFX server builds");
        start_gfx_channel(&mut first_bridge);
        process_avc444_capabilities(&mut first_bridge);

        let (display_tx, mut display_rx) = mpsc::channel(4);
        let mut first = gradient_bgra_frame(width, height, stride);
        let mut processor = FrameProcessor::new(
            Some(Arc::clone(&shared)),
            width as u32,
            height as u32,
            PixelFormat::BgrA32,
            stride as u32,
            1_000_000,
            23,
            H264RateControl::Cqp,
            30,
        );
        processor.queue_damage(&[(0, 0, width as i32, height as i32)]);
        assert!(processor.process(&first, &display_tx));
        let first_generation = processor.egfx_generation;
        assert_eq!(first_generation, shared.generation());
        let first_pdus = drain_gfx_pdus(&mut first_event_rx);
        first_pdus.assert_initial_surface_setup_precedes_logical_frame(width as u16, height as u16);
        first_pdus.assert_sendable_avc444_frame(ExpectedAvc444Encoding::LumaAndChroma);
        assert!(display_rx.try_recv().is_err());

        let (second_event_tx, mut second_event_rx) = mpsc::unbounded_channel();
        ServerEventSender::set_sender(&mut factory, second_event_tx);
        let (mut second_bridge, _) =
            GfxServerFactory::build_server_with_handle(&factory).expect("EGFX server rebuilds");

        mutate_bgra_tile(&mut first, width, height, stride, 2);
        processor.queue_damage(&[(8, 8, 16, 16)]);
        assert!(processor.process(&first, &display_tx));
        assert!(!processor.sent_first_frame);
        assert!(processor.has_pending_damage());
        assert!(display_rx.try_recv().is_err());
        drain_gfx_pdus(&mut second_event_rx).assert_empty();

        start_gfx_channel(&mut second_bridge);
        process_avc444_capabilities(&mut second_bridge);
        assert!(shared.generation() > first_generation);
        assert!(processor.process(&first, &display_tx));

        assert_eq!(processor.egfx_generation, shared.generation());
        assert!(processor.sent_first_frame);
        assert!(!processor.has_pending_damage());
        assert!(display_rx.try_recv().is_err());
        let second_pdus = drain_gfx_pdus(&mut second_event_rx);
        second_pdus
            .assert_initial_surface_setup_precedes_logical_frame(width as u16, height as u16);
        second_pdus.assert_sendable_avc444_frame(ExpectedAvc444Encoding::LumaAndChroma);
        drain_gfx_pdus(&mut first_event_rx).assert_empty();
    }

    #[test]
    fn frame_processor_recreates_avc444_state_after_egfx_generation_bump() {
        let width = 64;
        let height = 64;
        let stride = width * 4;
        let (shared, mut event_rx) = negotiated_avc444_egfx(width as u16, height as u16);
        let (display_tx, mut display_rx) = mpsc::channel(4);
        let first = gradient_bgra_frame(width, height, stride);
        let mut second = first.clone();
        second[8] = 0x7f;

        let mut processor = FrameProcessor::new(
            Some(Arc::clone(&shared)),
            width as u32,
            height as u32,
            PixelFormat::BgrA32,
            stride as u32,
            1_000_000,
            23,
            H264RateControl::Cqp,
            30,
        );
        processor.queue_damage(&[(0, 0, width as i32, height as i32)]);

        assert!(processor.process(&first, &display_tx));
        assert_eq!(processor.egfx_codec, Some(EgfxCodec::Avc444));
        assert!(processor.sent_first_frame);
        assert!(processor.egfx_surface_id.is_some());
        let initial_pdus = drain_gfx_pdus(&mut event_rx);
        let initial_frame_summary =
            initial_pdus.assert_sendable_avc444_frame(ExpectedAvc444Encoding::LumaAndChroma);
        let old_surface_id = initial_frame_summary.surface_id;
        let first_generation = processor.egfx_generation;

        shared.prepare_for_resize(width as u16, height as u16);
        let resize_pdus = drain_gfx_pdus(&mut event_rx);
        assert!(resize_pdus.contains_delete_surface(old_surface_id));
        resize_pdus.assert_no_encoded_frame();
        processor.queue_damage(&[(0, 0, 4, 2)]);
        assert!(processor.process(&second, &display_tx));

        assert!(processor.egfx_generation > first_generation);
        assert_eq!(processor.egfx_generation, shared.generation());
        assert_eq!(processor.egfx_codec, Some(EgfxCodec::Avc444));
        assert!(processor.sent_first_frame);
        assert!(processor.egfx_surface_id.is_some());
        let resized_pdus = drain_gfx_pdus(&mut event_rx);
        let new_surface_id = resized_pdus.first_created_surface_id();
        assert_ne!(new_surface_id, old_surface_id);
        assert!(resized_pdus.contains_map_surface_to_output(new_surface_id));
        let resized_frame_summary =
            resized_pdus.assert_sendable_avc444_frame(ExpectedAvc444Encoding::LumaAndChroma);
        assert_eq!(resized_frame_summary.surface_id, new_surface_id);
        assert!(!processor.has_pending_damage());
        let (last_luma_regions, last_chroma_regions) = processor
            .h264_encoder
            .as_ref()
            .and_then(crate::egfx::FrameEncoder::avc444_last_reference_regions_for_test)
            .expect("AVC444 region state exists after re-generation");
        let full_frame = [(0, 0, width as i32, height as i32)];
        assert_eq!(last_luma_regions, full_frame);
        assert_eq!(last_chroma_regions, full_frame);
        assert!(display_rx.try_recv().is_err());
    }
}
