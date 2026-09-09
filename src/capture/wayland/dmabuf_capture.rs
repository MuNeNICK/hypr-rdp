use std::time::Instant;
use std::{fmt, sync::Arc};

use anyhow::{bail, Context, Result};
use ironrdp_server::{DesktopSize, DisplayUpdate};
use wayland_client::protocol::wl_buffer;
use wayland_client::{Connection, QueueHandle};
use wayland_protocols::ext::image_copy_capture::v1::client::ext_image_copy_capture_session_v1;
use wayland_protocols::wp::linux_dmabuf::zv1::client::{
    zwp_linux_buffer_params_v1, zwp_linux_dmabuf_v1,
};

use super::state::AppState;
use super::{poll_dispatch, POLL_TIMEOUT_MS};
use crate::capture::dmabuf::Allocation;
use crate::capture::frame::FramePacer;
use crate::capture::scale::dmabuf_zero_copy_allowed;
use crate::egfx::{
    EgfxFrameSession, EgfxShared, EncodedEgfxFrame, EncodedFrameState, H264BackendPolicy,
    H264RateControl,
};
use crate::input::{OutputLayoutSnapshot, SharedOutputLayout};

const MAX_ENCODE_FAILURES: u32 = 5;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DmaBufEncodeFailureAction {
    Retry,
    FallBackToShm,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DmaBufCaptureDecision {
    Continue,
    FallBackToShm,
    RestartCaptureSession,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DmaBufFrameWaitDecision {
    ContinueWaiting,
    FrameCompleted,
    Shutdown,
    Exit(DmaBufCaptureDecision),
}

#[derive(Debug)]
pub(super) struct DmaBufCaptureSessionGeometryChanged;

impl fmt::Display for DmaBufCaptureSessionGeometryChanged {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("EXT DMA-BUF capture source geometry changed")
    }
}

impl std::error::Error for DmaBufCaptureSessionGeometryChanged {}

pub(super) fn is_capture_session_geometry_changed(err: &anyhow::Error) -> bool {
    err.downcast_ref::<DmaBufCaptureSessionGeometryChanged>()
        .is_some()
}

#[derive(Debug)]
struct DmaBufEncodeFailureTracker {
    failures: u32,
    max_failures: u32,
}

impl DmaBufEncodeFailureTracker {
    fn new(max_failures: u32) -> Self {
        Self {
            failures: 0,
            max_failures: max_failures.max(1),
        }
    }

    fn failures(&self) -> u32 {
        self.failures
    }

    fn reset_window(&mut self) {
        self.failures = 0;
    }

    fn record_no_progress(&mut self) -> DmaBufEncodeFailureAction {
        self.failures = self.failures.saturating_add(1);
        if self.failures >= self.max_failures {
            DmaBufEncodeFailureAction::FallBackToShm
        } else {
            DmaBufEncodeFailureAction::Retry
        }
    }
}

fn dmabuf_capture_decision(
    snapshot: Option<&OutputLayoutSnapshot>,
    loop_size: (u32, u32),
) -> DmaBufCaptureDecision {
    let Some(snapshot) = snapshot else {
        return DmaBufCaptureDecision::FallBackToShm;
    };

    let source = snapshot.presentation_geometry.source();
    if (source.width, source.height) != loop_size {
        return DmaBufCaptureDecision::RestartCaptureSession;
    }

    if dmabuf_zero_copy_allowed(snapshot) {
        DmaBufCaptureDecision::Continue
    } else {
        DmaBufCaptureDecision::FallBackToShm
    }
}

fn dmabuf_capture_decision_result(decision: DmaBufCaptureDecision) -> Result<()> {
    match decision {
        DmaBufCaptureDecision::Continue => Ok(()),
        DmaBufCaptureDecision::FallBackToShm => {
            bail!("presentation geometry changed while using DMA-BUF; falling back to SHM")
        }
        DmaBufCaptureDecision::RestartCaptureSession => {
            Err(DmaBufCaptureSessionGeometryChanged.into())
        }
    }
}

fn dmabuf_egfx_activation_timed_out(shared: &EgfxShared, ready: bool, now: Instant) -> bool {
    !ready && shared.bitmap_fallback_due(now)
}

fn dmabuf_frame_wait_decision(
    frame_ready: bool,
    frame_failed: bool,
    should_stop: bool,
    snapshot: Option<&OutputLayoutSnapshot>,
    loop_size: (u32, u32),
) -> DmaBufFrameWaitDecision {
    if should_stop {
        return DmaBufFrameWaitDecision::Shutdown;
    }

    match dmabuf_capture_decision(snapshot, loop_size) {
        DmaBufCaptureDecision::Continue if frame_ready || frame_failed => {
            DmaBufFrameWaitDecision::FrameCompleted
        }
        DmaBufCaptureDecision::Continue => DmaBufFrameWaitDecision::ContinueWaiting,
        decision => DmaBufFrameWaitDecision::Exit(decision),
    }
}

/// Context for DMA-BUF capture (created during setup, passed to capture loop).
#[cfg(feature = "vaapi")]
pub(super) struct DmaBufCaptureContext {
    // Fields drop top-to-bottom; protocol/VA objects must release before GBM owners.
    wl_buffers: Vec<wl_buffer::WlBuffer>,
    vpp: crate::egfx::VppConverter,
    nv12_info: crate::egfx::VppDmaBufInfo,
    drm_device_path: std::path::PathBuf,
    #[allow(dead_code)]
    dmabuf_infos: Vec<crate::capture::dmabuf::DmaBufInfo>,
    #[allow(dead_code)]
    gbm_bos: Vec<crate::capture::dmabuf::GbmBo>,
    #[allow(dead_code)]
    gbm_device: crate::capture::dmabuf::GbmDevice,
}

/// Try to set up DMA-BUF capture. Returns None if compositor doesn't support DMA-BUF,
/// or Some(Err) if setup fails.
#[cfg(feature = "vaapi")]
pub(super) fn try_setup_dmabuf(
    state: &AppState,
    qh: &QueueHandle<AppState>,
    width: u32,
    height: u32,
) -> Option<Result<DmaBufCaptureContext>> {
    use crate::capture::dmabuf::{DRM_FORMAT_ARGB8888, DRM_FORMAT_XRGB8888};

    let dev = state.dmabuf_device?;
    if state.dmabuf_formats.is_empty() {
        return None;
    }
    let linux_dmabuf = state.linux_dmabuf.as_ref()?;

    // Find a suitable format (prefer XRGB8888)
    let (chosen_format, chosen_modifiers) = {
        let mut best: Option<&(u32, Vec<u64>)> = None;
        for entry in &state.dmabuf_formats {
            if entry.0 == DRM_FORMAT_XRGB8888 {
                best = Some(entry);
                break;
            }
            if entry.0 == DRM_FORMAT_ARGB8888 && best.is_none() {
                best = Some(entry);
            }
        }
        match best {
            Some(entry) => (entry.0, &entry.1),
            None => {
                tracing::trace!("No XRGB8888/ARGB8888 format in DMA-BUF formats");
                return None;
            }
        }
    };

    tracing::trace!(
        format = format!("0x{:08x}", chosen_format),
        num_modifiers = chosen_modifiers.len(),
        "Attempting DMA-BUF capture setup"
    );

    Some(setup_dmabuf_inner(
        dev,
        linux_dmabuf,
        qh,
        width,
        height,
        chosen_format,
        chosen_modifiers,
    ))
}

#[cfg(feature = "vaapi")]
fn setup_dmabuf_inner(
    dev: libc::dev_t,
    linux_dmabuf: &zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
    qh: &QueueHandle<AppState>,
    width: u32,
    height: u32,
    format: u32,
    modifiers: &[u64],
) -> Result<DmaBufCaptureContext> {
    use crate::capture::dmabuf::{drm_device_from_devt, open_drm_device, GbmBo, GbmDevice};

    // Find DRM device path from dev_t
    let drm_device_path =
        drm_device_from_devt(dev).context("failed to find DRM device from dev_t")?;
    tracing::trace!(device = %drm_device_path.display(), "DMA-BUF: found DRM device");

    // Open DRM device and create GBM device
    let drm_fd = open_drm_device(&drm_device_path)?;
    let gbm_device = GbmDevice::new(drm_fd)?;

    // Allocate 2 GBM buffer objects (double-buffered capture)
    let mut gbm_bos = Vec::with_capacity(2);
    let mut wl_buffers = Vec::with_capacity(2);
    let mut dmabuf_infos = Vec::with_capacity(2);

    let plan = crate::capture::dmabuf::plan_allocation(modifiers, |modifier| {
        gbm_device.format_modifier_plane_count(format, modifier)
    });
    if plan == Allocation::Refuse {
        anyhow::bail!(
            "none of the {} modifiers the compositor advertised for format {:#010x} describes a \
             single plane; the capture path can only describe one",
            modifiers.len(),
            format
        );
    }

    for i in 0..2 {
        let mut bo = match &plan {
            Allocation::WithModifiers {
                modifiers,
                allow_implicit_fallback,
            } => {
                let explicit =
                    GbmBo::create_with_modifiers(&gbm_device, width, height, format, modifiers);
                if *allow_implicit_fallback {
                    explicit.or_else(|_| GbmBo::create(&gbm_device, width, height, format))
                } else {
                    explicit
                }
            }
            Allocation::ImplicitModifier => GbmBo::create(&gbm_device, width, height, format),
            Allocation::Refuse => unreachable!("refused allocation was handled above"),
        }
        .with_context(|| format!("failed to allocate GBM buffer {}", i))?;

        anyhow::ensure!(
            bo.plane_count() == 1,
            "GBM returned a {}-plane buffer for modifier {:#018x}; the capture path can only \
             describe one",
            bo.plane_count(),
            bo.modifier()
        );

        let info = bo
            .dmabuf_info(format, width, height)
            .with_context(|| format!("failed to get DMA-BUF info for buffer {}", i))?;

        // Create wl_buffer via linux-dmabuf
        let params = linux_dmabuf.create_params(qh, ());
        let fd = bo
            .fd()
            .with_context(|| format!("failed to get fd for buffer {}", i))?;
        let stride = bo.stride();
        let offset = bo.offset(0);
        let modifier = bo.modifier();
        let modifier_hi = (modifier >> 32) as u32;
        let modifier_lo = (modifier & 0xFFFFFFFF) as u32;

        // Add the plane to params.
        // SAFETY: `context.gbm_bos` owns the GBM BOs for the lifetime of these buffers.
        params.add(
            unsafe { std::os::unix::io::BorrowedFd::borrow_raw(fd) },
            0,
            offset,
            stride,
            modifier_hi,
            modifier_lo,
        );

        let wl_buf = params.create_immed(
            width as i32,
            height as i32,
            format,
            zwp_linux_buffer_params_v1::Flags::empty(),
            qh,
            (),
        );

        tracing::trace!(
            idx = i,
            fd,
            stride,
            offset,
            modifier = format!("0x{:016x}", modifier),
            "DMA-BUF: allocated capture buffer"
        );

        dmabuf_infos.push(info);
        wl_buffers.push(wl_buf);
        gbm_bos.push(bo);
    }

    // Create VPP converter
    let mut vpp = crate::egfx::VppConverter::new(&drm_device_path, width, height)?;

    // Import the two XRGB DMA-BUFs as VPP input surfaces
    for (i, info) in dmabuf_infos.iter().enumerate() {
        vpp.import_input_surface(info.fd, width, height, info.stride, info.modifier, format)
            .with_context(|| format!("failed to import VPP input surface {}", i))?;
    }

    // Export the NV12 output surface as a DMA-BUF
    let nv12_info = vpp.export_nv12_output()?;
    tracing::trace!(
        nv12_fd = nv12_info.fd,
        nv12_stride = nv12_info.stride,
        "DMA-BUF: VPP NV12 output exported"
    );

    Ok(DmaBufCaptureContext {
        wl_buffers,
        vpp,
        nv12_info,
        drm_device_path,
        dmabuf_infos,
        gbm_bos,
        gbm_device,
    })
}

/// DMA-BUF capture loop for ext-image-copy-capture.
#[cfg(feature = "vaapi")]
#[allow(clippy::too_many_arguments)]
pub(super) fn capture_loop_ext_dmabuf(
    conn: &Connection,
    event_queue: &mut wayland_client::EventQueue<AppState>,
    state: &mut AppState,
    qh: &QueueHandle<AppState>,
    session: &ext_image_copy_capture_session_v1::ExtImageCopyCaptureSessionV1,
    width: u32,
    height: u32,
    dmabuf_ctx: &DmaBufCaptureContext,
    egfx_shared: Option<Arc<EgfxShared>>,
    bitrate: u32,
    quality: u8,
    rate_control: H264RateControl,
    fps: u32,
    h264_backend: H264BackendPolicy,
    pending_initial_resize: Option<DesktopSize>,
    output_layout: Arc<SharedOutputLayout>,
) -> Result<()> {
    tracing::info!(
        width,
        height,
        device = %dmabuf_ctx.drm_device_path.display(),
        mode = "ext-dmabuf",
        fps,
        "Starting capture loop (zero-copy DMA-BUF)"
    );

    let mut frame_pacer = FramePacer::new(fps, Instant::now());
    let mut cap_idx: usize = 0;
    let mut sent_first_frame = false;

    // EGFX transport and surface state live behind the EGFX session helper.
    let mut egfx_session = EgfxFrameSession::new();
    let mut h264_encoder: Option<crate::egfx::FrameEncoder> = None;
    let mut encode_failures = DmaBufEncodeFailureTracker::new(MAX_ENCODE_FAILURES);
    let metadata_qp = match rate_control {
        H264RateControl::Vbr => 0,
        H264RateControl::Cqp => quality.min(51),
    };

    // Start initial capture
    let mut frame = session.create_frame(qh, ());
    state.frame_ready = false;
    state.frame_failed = false;
    state.damage_regions.clear();
    frame.attach_buffer(&dmabuf_ctx.wl_buffers[cap_idx]);
    frame.damage_buffer(0, 0, width as i32, height as i32);
    frame.capture();
    conn.flush().context("Wayland flush failed")?;

    loop {
        if state.should_stop() {
            break;
        }

        // Wait for current frame to complete (poll-based for responsive shutdown)
        while !state.frame_ready && !state.frame_failed {
            poll_dispatch(event_queue, state, POLL_TIMEOUT_MS)?;
            match dmabuf_frame_wait_decision(
                state.frame_ready,
                state.frame_failed,
                state.should_stop(),
                output_layout.snapshot().as_ref(),
                (width, height),
            ) {
                DmaBufFrameWaitDecision::ContinueWaiting => {}
                DmaBufFrameWaitDecision::FrameCompleted | DmaBufFrameWaitDecision::Shutdown => {
                    break;
                }
                DmaBufFrameWaitDecision::Exit(decision) => {
                    frame.destroy();
                    return dmabuf_capture_decision_result(decision);
                }
            }
        }
        frame.destroy();

        // Shutdown interrupted the wait — exit cleanly
        if !state.frame_ready && !state.frame_failed {
            break;
        }

        dmabuf_capture_decision_result(dmabuf_capture_decision(
            output_layout.snapshot().as_ref(),
            (width, height),
        ))?;

        let completed_failed = state.frame_failed;
        let completed_idx = cap_idx;
        let has_damage = !state.damage_regions.is_empty();

        // Start next capture immediately
        cap_idx = 1 - cap_idx;
        state.frame_ready = false;
        state.frame_failed = false;
        state.damage_regions.clear();
        frame = session.create_frame(qh, ());
        frame.attach_buffer(&dmabuf_ctx.wl_buffers[cap_idx]);
        frame.damage_buffer(0, 0, width as i32, height as i32);
        frame.capture();
        conn.flush().context("Wayland flush failed")?;

        if completed_failed {
            continue;
        }

        // Rate limit
        let pacing_fps = egfx_shared
            .as_ref()
            .map_or(fps.max(1), |shared| shared.preferred_frame_rate(fps));
        if frame_pacer.should_send(Instant::now(), sent_first_frame, has_damage, pacing_fps) {
            // Process via DMA-BUF zero-copy pipeline
            // Update EGFX state
            if let Some(shared) = &egfx_shared {
                if let Some(size) = pending_initial_resize {
                    if shared.is_ready() {
                        tracing::info!(
                            width = size.width,
                            height = size.height,
                            "Sending initial resize after graphics channel is ready"
                        );
                        let _ = state.tx.blocking_send(DisplayUpdate::Resize(size));
                        break;
                    }
                }

                let session_refresh = egfx_session.refresh(shared);
                if session_refresh.became_unready {
                    h264_encoder = None;
                    encode_failures.reset_window();
                }

                if session_refresh.ready
                    && (session_refresh.generation_changed || h264_encoder.is_none())
                {
                    encode_failures.reset_window();
                    match crate::egfx::FrameEncoder::new(
                        h264_backend,
                        width,
                        height,
                        bitrate,
                        fps,
                        quality,
                        rate_control,
                    ) {
                        Ok(enc) => {
                            tracing::info!(
                                width,
                                height,
                                backend = enc.backend_name(),
                                gen = shared.generation(),
                                "H.264 encoder initialized (DMA-BUF path)"
                            );
                            h264_encoder = Some(enc);
                        }
                        Err(e) => {
                            // DMA-BUF path requires a working encoder; bail to SHM fallback
                            frame.destroy();
                            bail!("H.264 encoder init failed in DMA-BUF mode, falling back to SHM: {:#}", e);
                        }
                    }
                }

                if dmabuf_egfx_activation_timed_out(shared, session_refresh.ready, Instant::now()) {
                    frame.destroy();
                    bail!("EGFX activation timed out in DMA-BUF mode; falling back to SHM");
                }

                if session_refresh.ready && h264_encoder.is_some() {
                    if !egfx_session.ensure_surface(shared, width as u16, height as u16) {
                        continue;
                    }

                    let readiness = egfx_session.frame_readiness(shared);
                    if !readiness.is_ready() {
                        tracing::trace!(
                            ?readiness,
                            reason = readiness.reason(),
                            "EGFX frame skipped before DMA-BUF encode"
                        );
                        continue;
                    }

                    // Zero-copy encode pipeline:
                    // 1. VPP: XRGB DMA-BUF -> NV12 (GPU)
                    // 2. Encoder: NV12 DMA-BUF -> H.264 (GPU)
                    let vpp_result = dmabuf_ctx.vpp.convert(completed_idx);
                    let encode_result = match vpp_result {
                        Ok(()) => {
                            let nv12 = &dmabuf_ctx.nv12_info;
                            h264_encoder.as_mut().map(|enc| {
                                enc.encode_dmabuf(
                                    nv12.fd,
                                    nv12.width,
                                    nv12.height,
                                    nv12.stride,
                                    nv12.offset,
                                    nv12.modifier,
                                    nv12.uv_stride,
                                    nv12.uv_offset,
                                )
                            })
                        }
                        Err(e) => Some(Err(e)),
                    };

                    match encode_result {
                        Some(Ok(h264_data)) => {
                            let encoded = EncodedEgfxFrame::avc420(h264_data);
                            match encoded.state() {
                                EncodedFrameState::Sendable => {
                                    encode_failures.reset_window();
                                    let timestamp = std::time::SystemTime::now()
                                        .duration_since(std::time::UNIX_EPOCH)
                                        .unwrap_or_default()
                                        .as_millis()
                                        as u32;
                                    let sent = egfx_session.send_encoded_frame(
                                        shared,
                                        &encoded,
                                        &[(0, 0, width as i32, height as i32)],
                                        timestamp,
                                        width as u16,
                                        height as u16,
                                        metadata_qp,
                                    );
                                    if sent {
                                        sent_first_frame = true;
                                    } else if let Some(enc) = &mut h264_encoder {
                                        enc.force_idr();
                                    }
                                }
                                EncodedFrameState::Skipped | EncodedFrameState::Invalid => {
                                    let action = encode_failures.record_no_progress();
                                    tracing::trace!(
                                        failures = encode_failures.failures(),
                                        max = MAX_ENCODE_FAILURES,
                                        bytes = encoded.len(),
                                        "DMA-BUF encode produced no sendable H.264 output"
                                    );
                                    if let Some(enc) = &mut h264_encoder {
                                        enc.force_idr();
                                    }
                                    if action == DmaBufEncodeFailureAction::FallBackToShm {
                                        frame.destroy();
                                        bail!(
                                            "VA-API encode produced no sendable H.264 output \
                                                 {} consecutive times in DMA-BUF mode, falling \
                                                 back to SHM",
                                            encode_failures.failures()
                                        );
                                    }
                                }
                            }
                        }
                        Some(Err(e)) => {
                            let action = encode_failures.record_no_progress();
                            tracing::warn!(
                                failures = encode_failures.failures(),
                                max = MAX_ENCODE_FAILURES,
                                "DMA-BUF encode pipeline failed: {:#}",
                                e
                            );
                            if let Some(enc) = &mut h264_encoder {
                                enc.force_idr();
                            }
                            if action == DmaBufEncodeFailureAction::FallBackToShm {
                                // Destroy the in-flight frame before dropping DMA-BUF resources
                                frame.destroy();
                                bail!(
                                    "VA-API encode failed {} consecutive times in DMA-BUF mode, \
                                         falling back to SHM",
                                    encode_failures.failures()
                                );
                            }
                        }
                        None => {}
                    }
                }
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::display::geometry::{PresentationGeometry, Size};

    fn snapshot(source: (u32, u32), presentation: (u32, u32)) -> OutputLayoutSnapshot {
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

    #[test]
    fn dmabuf_capture_decision_classifies_geometry_changes() {
        assert_eq!(
            dmabuf_capture_decision(Some(&snapshot((1920, 1080), (1920, 1080))), (1920, 1080)),
            DmaBufCaptureDecision::Continue
        );
        assert_eq!(
            dmabuf_capture_decision(Some(&snapshot((1920, 1080), (1280, 720))), (1920, 1080)),
            DmaBufCaptureDecision::FallBackToShm
        );
        assert_eq!(
            dmabuf_capture_decision(Some(&snapshot((1600, 900), (1600, 900))), (1920, 1080)),
            DmaBufCaptureDecision::RestartCaptureSession
        );
        assert_eq!(
            dmabuf_capture_decision(Some(&snapshot((1600, 900), (1280, 720))), (1920, 1080)),
            DmaBufCaptureDecision::RestartCaptureSession
        );
        assert_eq!(
            dmabuf_capture_decision(None, (1920, 1080)),
            DmaBufCaptureDecision::FallBackToShm
        );
    }

    #[test]
    fn dmabuf_live_identity_resize_restarts_ext_capture_session() {
        assert_eq!(
            dmabuf_capture_decision(Some(&snapshot((1920, 1080), (1920, 1080))), (1920, 1080)),
            DmaBufCaptureDecision::Continue
        );
        assert_eq!(
            dmabuf_capture_decision(Some(&snapshot((1600, 900), (1600, 900))), (1920, 1080)),
            DmaBufCaptureDecision::RestartCaptureSession
        );
    }

    #[test]
    fn dmabuf_same_source_scaled_geometry_requests_shm_fallback() {
        assert_eq!(
            dmabuf_capture_decision(Some(&snapshot((1920, 1080), (1280, 720))), (1920, 1080)),
            DmaBufCaptureDecision::FallBackToShm
        );
    }

    #[test]
    fn dmabuf_guard_exits_during_wait_when_layout_changes() {
        assert_eq!(
            dmabuf_frame_wait_decision(
                false,
                false,
                false,
                Some(&snapshot((1920, 1080), (1920, 1080))),
                (1920, 1080)
            ),
            DmaBufFrameWaitDecision::ContinueWaiting
        );
        assert_eq!(
            dmabuf_frame_wait_decision(
                false,
                false,
                false,
                Some(&snapshot((1920, 1080), (1280, 720))),
                (1920, 1080)
            ),
            DmaBufFrameWaitDecision::Exit(DmaBufCaptureDecision::FallBackToShm)
        );
        assert_eq!(
            dmabuf_frame_wait_decision(
                false,
                false,
                false,
                Some(&snapshot((1600, 900), (1600, 900))),
                (1920, 1080)
            ),
            DmaBufFrameWaitDecision::Exit(DmaBufCaptureDecision::RestartCaptureSession)
        );
    }

    #[test]
    fn dmabuf_wait_decision_checks_layout_before_frame_completion() {
        assert_eq!(
            dmabuf_frame_wait_decision(
                true,
                false,
                false,
                Some(&snapshot((1920, 1080), (1920, 1080))),
                (1920, 1080)
            ),
            DmaBufFrameWaitDecision::FrameCompleted
        );
        assert_eq!(
            dmabuf_frame_wait_decision(
                true,
                false,
                false,
                Some(&snapshot((1600, 900), (1600, 900))),
                (1920, 1080)
            ),
            DmaBufFrameWaitDecision::Exit(DmaBufCaptureDecision::RestartCaptureSession)
        );
        assert_eq!(
            dmabuf_frame_wait_decision(
                false,
                true,
                false,
                Some(&snapshot((1920, 1080), (1280, 720))),
                (1920, 1080)
            ),
            DmaBufFrameWaitDecision::Exit(DmaBufCaptureDecision::FallBackToShm)
        );
    }

    #[test]
    fn dmabuf_wait_decision_respects_shutdown() {
        assert_eq!(
            dmabuf_frame_wait_decision(
                false,
                false,
                true,
                Some(&snapshot((1600, 900), (1600, 900))),
                (1920, 1080)
            ),
            DmaBufFrameWaitDecision::Shutdown
        );
    }

    #[test]
    fn dmabuf_source_resize_error_is_restart_marker() {
        let err = dmabuf_capture_decision_result(DmaBufCaptureDecision::RestartCaptureSession)
            .expect_err("source resize should request capture restart");

        assert!(is_capture_session_geometry_changed(&err));
    }

    #[test]
    fn dmabuf_presentation_resize_error_is_shm_fallback() {
        let err = dmabuf_capture_decision_result(DmaBufCaptureDecision::FallBackToShm)
            .expect_err("presentation resize should request SHM fallback");

        assert!(!is_capture_session_geometry_changed(&err));
    }

    #[test]
    fn dmabuf_current_geometry_decision_is_ok() {
        dmabuf_capture_decision_result(DmaBufCaptureDecision::Continue)
            .expect("current geometry continues");
    }

    #[test]
    fn dmabuf_egfx_activation_timeout_selects_shm_fallback() {
        let shared = EgfxShared::with_codec_policy(
            crate::egfx::DEFAULT_MAX_FRAMES_IN_FLIGHT,
            crate::egfx::EgfxCodecPolicy::Auto,
        );
        let start = Instant::now();

        assert!(!dmabuf_egfx_activation_timed_out(&shared, false, start));
        assert!(!dmabuf_egfx_activation_timed_out(
            &shared,
            true,
            start + crate::egfx::GFX_READY_GRACE
        ));
        assert!(dmabuf_egfx_activation_timed_out(
            &shared,
            false,
            start + crate::egfx::GFX_READY_GRACE
        ));
    }

    #[test]
    fn dmabuf_non_avc_clearcodec_session_selects_shm_without_waiting() {
        let session = crate::egfx::test_support::negotiated_no_avc_session(16, 16);
        let mut transport = crate::egfx::EgfxFrameSession::new();
        let refresh = transport.refresh(&session.shared);
        assert!(!refresh.ready);
        assert!(dmabuf_egfx_activation_timed_out(
            &session.shared,
            refresh.ready,
            Instant::now()
        ));
    }

    #[test]
    fn dmabuf_encode_failure_tracker_retries_until_fallback_threshold() {
        let mut tracker = DmaBufEncodeFailureTracker::new(3);

        assert_eq!(
            tracker.record_no_progress(),
            DmaBufEncodeFailureAction::Retry
        );
        assert_eq!(tracker.failures(), 1);
        assert_eq!(
            tracker.record_no_progress(),
            DmaBufEncodeFailureAction::Retry
        );
        assert_eq!(tracker.failures(), 2);
        assert_eq!(
            tracker.record_no_progress(),
            DmaBufEncodeFailureAction::FallBackToShm
        );
        assert_eq!(tracker.failures(), 3);
    }

    #[test]
    fn dmabuf_encode_failure_tracker_reset_requires_fresh_failure_window() {
        let mut tracker = DmaBufEncodeFailureTracker::new(2);

        assert_eq!(
            tracker.record_no_progress(),
            DmaBufEncodeFailureAction::Retry
        );
        tracker.reset_window();

        assert_eq!(tracker.failures(), 0);
        assert_eq!(
            tracker.record_no_progress(),
            DmaBufEncodeFailureAction::Retry
        );
    }

    #[test]
    fn dmabuf_encode_tracker_falls_back_after_unsendable_h264_outputs() {
        let mut tracker = DmaBufEncodeFailureTracker::new(2);

        assert_eq!(
            tracker.record_no_progress(),
            DmaBufEncodeFailureAction::Retry
        );
        assert_eq!(
            tracker.record_no_progress(),
            DmaBufEncodeFailureAction::FallBackToShm
        );

        tracker.reset_window();
        assert_eq!(tracker.failures(), 0);
    }
}
