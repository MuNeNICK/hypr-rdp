mod damage;
#[cfg(feature = "vaapi")]
pub mod dmabuf;
mod frame;
mod scale;
mod wayland;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use ironrdp_displaycontrol::pdu::{DisplayControlMonitorLayout, MonitorOrientation};
use ironrdp_server::{
    DesktopSize, DisplayUpdate, RdpServerDisplay, RdpServerDisplayUpdates, ServerError,
    ServerErrorExt as _, ServerResult,
};
use tokio::sync::{mpsc, Mutex};

use crate::egfx::{EgfxShared, H264BackendPolicy, H264RateControl};
use crate::input::{PreparedOutputLayout, SharedOutputLayout};

pub(crate) use wayland::HeadlessOutputGuard;

const H264_SOFTWARE_MAX_LONG_DIMENSION: u32 = 3840;
const H264_SOFTWARE_MAX_SHORT_DIMENSION: u32 = 2160;
const DISPLAYCONTROL_MAX_PRESENTATION_AREA: u64 = 3840 * 2400;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureMode {
    /// ext-image-copy-capture-v1
    Ext,
    /// wlr-screencopy-v1
    Wlr,
}

/// Inner state for display capture. Held behind Arc<Mutex<>> so that
/// server::run() can call shutdown() independently of RdpServer's drop.
struct HyprDisplayInner {
    width: u16,
    height: u16,
    resolution: (u32, u32),
    capture_mode: CaptureMode,
    output_name: String,
    egfx_shared: Option<Arc<EgfxShared>>,
    output_layout: Arc<SharedOutputLayout>,
    update_tx: mpsc::Sender<DisplayUpdate>,
    update_rx: Option<mpsc::Receiver<DisplayUpdate>>,
    bitrate: u32,
    quality: u8,
    rate_control: H264RateControl,
    h264_backend: H264BackendPolicy,
    fps: u32,
    output: Option<String>,
    headless_scale: f64,
    resolution_fixed: bool,
    stop_flag: Arc<AtomicBool>,
    capture_handle: Option<std::thread::JoinHandle<()>>,
    headless_guard: Option<HeadlessOutputGuard>,
    pending_initial_resize: Option<DesktopSize>,
    resize_gate: Arc<Mutex<()>>,
    closed: bool,
    output_size_unconfirmed: bool,
}

impl HyprDisplayInner {
    /// Explicit shutdown: stop capture thread → join → remove headless output.
    fn shutdown(&mut self) {
        self.stop_flag.store(true, Ordering::Release);
        if let Some(handle) = self.capture_handle.take() {
            let _ = handle.join();
        }
        // Thread exited, Wayland connection closed. Safe to remove output.
        drop(self.headless_guard.take());
    }
}

impl Drop for HyprDisplayInner {
    fn drop(&mut self) {
        // Safety net: if shutdown() was not called (e.g. early error in setup()),
        // ensure capture thread is joined before headless_guard drops.
        self.shutdown();
    }
}

/// Shared handle to HyprDisplayInner for explicit shutdown from server::run().
#[derive(Clone)]
pub struct HyprDisplayHandle {
    inner: Arc<Mutex<HyprDisplayInner>>,
}

impl HyprDisplayHandle {
    pub async fn shutdown(&self) {
        let gate = {
            let mut inner = self.inner.lock().await;
            inner.closed = true;
            inner.stop_flag.store(true, Ordering::Release);
            Arc::clone(&inner.resize_gate)
        };
        let permit = gate.lock_owned().await;
        let lease = Arc::clone(&self.inner);
        let _ = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            lease.blocking_lock().shutdown();
        })
        .await;
    }
}

fn resize_headless_output(output_name: &str, width: u32, height: u32, scale: f64) -> Result<()> {
    let mode = format!("{}x{}@60", width, height);
    let rule = wayland::headless_monitor_rule(output_name, &mode, scale);
    crate::hyprland::keyword_monitor(&rule).context("failed to resize headless output")?;
    wayland::wait_for_output_size(output_name, width, height, Duration::from_secs(5))
        .context("headless output did not reach requested size after resize")?;
    Ok(())
}

fn clamp_to_h264_software_limits(width: u32, height: u32) -> (u32, u32) {
    let width = width & !1;
    let height = height & !1;
    if width == 0 || height == 0 {
        return (width, height);
    }

    let long = width.max(height);
    let short = width.min(height);
    if long <= H264_SOFTWARE_MAX_LONG_DIMENSION && short <= H264_SOFTWARE_MAX_SHORT_DIMENSION {
        return (width, height);
    }

    let scale_by_long = H264_SOFTWARE_MAX_LONG_DIMENSION as f64 / long as f64;
    let scale_by_short = H264_SOFTWARE_MAX_SHORT_DIMENSION as f64 / short as f64;
    let scale = scale_by_long.min(scale_by_short).min(1.0);

    let scaled_width = ((width as f64 * scale).floor() as u32).max(2) & !1;
    let scaled_height = ((height as f64 * scale).floor() as u32).max(2) & !1;
    (scaled_width, scaled_height)
}

/// Fit automatic presentation requests to the source without changing their
/// aspect ratio. Repeated normalization must be idempotent.
fn normalize_presentation_size(requested_size: (u32, u32), source_size: (u32, u32)) -> (u32, u32) {
    let (source_w, source_h) = source_size;
    let (width, height) = (requested_size.0 & !1, requested_size.1 & !1);
    if width == 0 || height == 0 || source_w == 0 || source_h == 0 {
        return (0, 0);
    }

    // H.264 dimensions are even and at least two pixels.
    let (fit_w, fit_h) = (source_w.max(2) & !1, source_h.max(2) & !1);
    let (fitted_width, fitted_height) = if width <= fit_w || height <= fit_h {
        (u64::from(width), u64::from(height))
    } else {
        let (requested_w, requested_h) = (u64::from(width), u64::from(height));
        let (fit_w, fit_h) = (u64::from(fit_w), u64::from(fit_h));
        if requested_w * fit_h <= requested_h * fit_w {
            (fit_w, requested_h * fit_w / requested_w)
        } else {
            // AVC444 requires a width divisible by four where possible.
            let padded = requested_w * fit_h / requested_h;
            let padded = if padded >= 4 {
                padded & !3
            } else {
                padded & !1
            };
            (padded, fit_h)
        }
    };

    // Encoder limits last: the fit only shrinks, so clamping after it never
    // undoes the fit, while clamping first can land a pixel under the source
    // and lose the identity geometry.
    clamp_to_h264_software_limits(fitted_width as u32, fitted_height as u32)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ResizeTarget {
    ManagedHeadlessOutput,
    PhysicalPresentation,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ResizeDecision {
    target: ResizeTarget,
    width: u32,
    height: u32,
}

// Recover externally mutated output after canceled/failed preparation, even
// when policy would otherwise skip an unchanged presentation.
fn reconcile_resize_decision(
    inner: &HyprDisplayInner,
    decision: Option<ResizeDecision>,
) -> Option<ResizeDecision> {
    decision.or_else(|| {
        (inner.output.is_none() && inner.output_size_unconfirmed).then_some(ResizeDecision {
            target: ResizeTarget::ManagedHeadlessOutput,
            width: inner.resolution.0,
            height: inner.resolution.1,
        })
    })
}

fn startup_presentation_size(
    physical_output: bool,
    resolution_fixed: bool,
    configured_resolution: (u32, u32),
    source_size: (u32, u32),
) -> (u32, u32) {
    let requested = if physical_output && !resolution_fixed {
        source_size
    } else {
        configured_resolution
    };
    if !physical_output {
        return requested;
    }
    if resolution_fixed {
        // A pinned resolution is the operator's call, upscale included; only
        // the encoder limits still apply.
        return clamp_to_h264_software_limits(requested.0, requested.1);
    }
    normalize_presentation_size(requested, source_size)
}

fn initial_size_resize_decision(
    physical_output: bool,
    resolution_fixed: bool,
    current_resolution: (u32, u32),
    requested_size: (u32, u32),
    source_size: Option<(u32, u32)>,
) -> Option<ResizeDecision> {
    if resolution_fixed {
        return None;
    }

    let (width, height) = if physical_output {
        normalize_presentation_size(requested_size, source_size?)
    } else {
        requested_size
    };
    if width == 0 || height == 0 || (width, height) == current_resolution {
        return None;
    }

    Some(ResizeDecision {
        target: if physical_output {
            ResizeTarget::PhysicalPresentation
        } else {
            ResizeTarget::ManagedHeadlessOutput
        },
        width,
        height,
    })
}

fn display_control_resize_decision(
    layout: &DisplayControlMonitorLayout,
    physical_output: bool,
    resolution_fixed: bool,
    current_resolution: (u32, u32),
    source_size: Option<(u32, u32)>,
) -> Option<ResizeDecision> {
    if resolution_fixed {
        return None;
    }

    let (requested_w, requested_h) = if physical_output {
        physical_display_control_size(layout)?
    } else {
        headless_display_control_size(layout)?
    };
    let (width, height) = if physical_output {
        normalize_presentation_size((requested_w, requested_h), source_size?)
    } else {
        clamp_to_h264_software_limits(requested_w, requested_h)
    };
    if width == 0 || height == 0 || (width, height) == current_resolution {
        return None;
    }

    Some(ResizeDecision {
        target: if physical_output {
            ResizeTarget::PhysicalPresentation
        } else {
            ResizeTarget::ManagedHeadlessOutput
        },
        width,
        height,
    })
}

fn headless_display_control_size(layout: &DisplayControlMonitorLayout) -> Option<(u32, u32)> {
    let monitor = layout
        .monitors()
        .iter()
        .find(|m| m.is_primary())
        .or_else(|| layout.monitors().first())?;
    Some(monitor.dimensions())
}

fn physical_display_control_size(layout: &DisplayControlMonitorLayout) -> Option<(u32, u32)> {
    let [monitor] = layout.monitors() else {
        return None;
    };
    if !monitor.is_primary() || monitor.position() != Some((0, 0)) {
        return None;
    }
    if matches!(
        monitor.orientation(),
        Some(
            MonitorOrientation::Portrait
                | MonitorOrientation::LandscapeFlipped
                | MonitorOrientation::PortraitFlipped
        )
    ) {
        return None;
    }

    let (width, height) = monitor.dimensions();
    if u64::from(width).saturating_mul(u64::from(height)) > DISPLAYCONTROL_MAX_PRESENTATION_AREA {
        return None;
    }

    Some((width, height))
}

fn apply_presentation_state_with(
    inner: &mut HyprDisplayInner,
    width: u32,
    height: u32,
    refresh_layout: impl FnOnce(&SharedOutputLayout, &str, (u32, u32)) -> Result<()>,
) -> Option<DesktopSize> {
    if let Err(e) = refresh_layout(&inner.output_layout, &inner.output_name, (width, height)) {
        tracing::warn!(
            "Failed to refresh input layout after presentation resize: {}",
            e
        );
        return None;
    }

    inner.output_size_unconfirmed = false;
    inner.resolution = (width, height);
    inner.width = width as u16;
    inner.height = height as u16;
    let desktop_size = DesktopSize {
        width: inner.width,
        height: inner.height,
    };

    if let Some(shared) = &inner.egfx_shared {
        shared.set_surface_size(inner.width, inner.height);
        shared.prepare_for_resize(inner.width, inner.height);
    }

    Some(desktop_size)
}

fn apply_resize_decision_with(
    inner: &mut HyprDisplayInner,
    decision: ResizeDecision,
    mut resize_headless: impl FnMut(&str, u32, u32, f64) -> Result<()>,
    mut refresh_layout: impl FnMut(&SharedOutputLayout, &str, (u32, u32)) -> Result<()>,
) -> Option<DesktopSize> {
    match decision.target {
        ResizeTarget::ManagedHeadlessOutput => {
            inner.output_size_unconfirmed = true;
            if let Err(e) = resize_headless(
                &inner.output_name,
                decision.width,
                decision.height,
                inner.headless_scale,
            ) {
                tracing::warn!("Failed to resize headless output: {}", e);
                None
            } else {
                apply_presentation_state_with(
                    inner,
                    decision.width,
                    decision.height,
                    |layout, name, presentation| refresh_layout(layout, name, presentation),
                )
            }
        }
        ResizeTarget::PhysicalPresentation => apply_presentation_state_with(
            inner,
            decision.width,
            decision.height,
            |layout, name, presentation| refresh_layout(layout, name, presentation),
        ),
    }
}

/// RdpServerDisplay implementation that delegates to HyprDisplayInner.
pub struct HyprDisplay {
    inner: Arc<Mutex<HyprDisplayInner>>,
}

impl HyprDisplay {
    #[allow(clippy::too_many_arguments)]
    pub async fn new(
        resolution: (u32, u32),
        headless_scale: f64,
        capture_mode: CaptureMode,
        egfx_shared: Arc<EgfxShared>,
        output_layout: Arc<SharedOutputLayout>,
        bitrate: u32,
        quality: u8,
        rate_control: H264RateControl,
        fps: u32,
        h264_backend: H264BackendPolicy,
        resolution_fixed: bool,
        output: Option<String>,
    ) -> Result<(Self, HyprDisplayHandle, (u16, u16))> {
        let (tx, rx) = mpsc::channel(128);
        let requested_resolution = resolution;
        let configured_resolution = clamp_to_h264_software_limits(resolution.0, resolution.1);
        if configured_resolution != requested_resolution {
            tracing::warn!(
                requested_w = requested_resolution.0,
                requested_h = requested_resolution.1,
                applied_w = configured_resolution.0,
                applied_h = configured_resolution.1,
                "Configured resolution exceeds H.264 software encoder policy limit; clamping"
            );
        }

        // Create or verify output up front, but defer Wayland capture until a
        // client subscribes to display updates. This keeps idle memory bounded.
        let (output_name, headless_guard) = if let Some(ref name) = output {
            (name.clone(), None)
        } else {
            let stale = wayland::list_stale_headless_outputs().unwrap_or_default();
            if let Some(existing) = stale.into_iter().next() {
                tracing::info!(name = %existing, "Reusing headless output from previous session");
                let mode = format!("{}x{}@60", configured_resolution.0, configured_resolution.1);
                let rule = wayland::headless_monitor_rule(&existing, &mode, headless_scale);
                crate::hyprland::keyword_monitor(&rule)
                    .context("failed to resize reused headless output")?;
                wayland::wait_for_output_size(
                    &existing,
                    configured_resolution.0,
                    configured_resolution.1,
                    Duration::from_secs(5),
                )?;
                (
                    existing.clone(),
                    Some(wayland::HeadlessOutputGuard::adopt(existing)),
                )
            } else {
                let (name, guard) = wayland::create_headless_output(
                    configured_resolution.0,
                    configured_resolution.1,
                    headless_scale,
                )?;
                wayland::wait_for_output_size(
                    &name,
                    configured_resolution.0,
                    configured_resolution.1,
                    Duration::from_secs(5),
                )?;
                (name, Some(guard))
            }
        };

        let capture_info = wayland::output_info(&output_name)
            .context("failed to get initial output dimensions")?;
        let requested_presentation_resolution = if output.is_some() && !resolution_fixed {
            (capture_info.width, capture_info.height)
        } else {
            configured_resolution
        };
        let presentation_resolution = startup_presentation_size(
            output.is_some(),
            resolution_fixed,
            configured_resolution,
            (capture_info.width, capture_info.height),
        );
        if output.is_some() && presentation_resolution != requested_presentation_resolution {
            tracing::info!(
                requested_w = requested_presentation_resolution.0,
                requested_h = requested_presentation_resolution.1,
                source_w = capture_info.width,
                source_h = capture_info.height,
                applied_w = presentation_resolution.0,
                applied_h = presentation_resolution.1,
                "Physical output presentation fitted to encoder limits and captured source"
            );
        }
        output_layout
            .update_from_output_with_presentation(&output_name, presentation_resolution)
            .context("failed to initialize input layout for output")?;

        let stop_flag = Arc::new(AtomicBool::new(false));

        let protocol_name = match capture_mode {
            CaptureMode::Ext => "ext-image-copy-capture-v1",
            CaptureMode::Wlr => "wlr-screencopy-v1",
        };
        tracing::info!(
            width = capture_info.width,
            height = capture_info.height,
            presentation_w = presentation_resolution.0,
            presentation_h = presentation_resolution.1,
            "Display prepared via {}; capture will start on client connection",
            protocol_name
        );

        let inner = Arc::new(Mutex::new(HyprDisplayInner {
            width: presentation_resolution.0 as u16,
            height: presentation_resolution.1 as u16,
            resolution: presentation_resolution,
            capture_mode,
            output_name: capture_info.output_name,
            egfx_shared: Some(egfx_shared),
            output_layout,
            update_tx: tx,
            update_rx: Some(rx),
            bitrate,
            quality,
            rate_control,
            h264_backend,
            fps,
            output,
            headless_scale,
            resolution_fixed,
            stop_flag,
            capture_handle: None,
            headless_guard,
            pending_initial_resize: None,
            resize_gate: Arc::new(Mutex::new(())),
            closed: false,
            output_size_unconfirmed: false,
        }));

        let dims = (
            presentation_resolution.0 as u16,
            presentation_resolution.1 as u16,
        );
        let handle = HyprDisplayHandle {
            inner: Arc::clone(&inner),
        };
        Ok((Self { inner }, handle, dims))
    }

    async fn request_initial_size_with(
        &mut self,
        client_size: DesktopSize,
        resize_headless: impl FnOnce(String, u32, u32, f64) -> Result<()> + Send + 'static,
        prepare_layout: impl FnOnce(&str, (u32, u32)) -> Result<PreparedOutputLayout> + Send + 'static,
    ) -> DesktopSize {
        // Acquire before even a no-op decision: canceled work may have changed
        // the compositor without publishing a new presentation.
        let gate = Arc::clone(&self.inner.lock().await.resize_gate);
        let permit = gate.lock_owned().await;
        let work = {
            let mut inner = self.inner.lock().await;
            let source = inner
                .output_layout
                .snapshot()
                .map(|s| (s.output_w, s.output_h));
            let requested =
                clamp_to_h264_software_limits(client_size.width.into(), client_size.height.into());
            if requested
                != (
                    u32::from(client_size.width) & !1,
                    u32::from(client_size.height) & !1,
                )
            {
                tracing::warn!(
                    requested_w = client_size.width,
                    requested_h = client_size.height,
                    applied_w = requested.0,
                    applied_h = requested.1,
                    "Client requested size exceeds H.264 software encoder policy limit; clamping"
                );
            }
            let decision = initial_size_resize_decision(
                inner.output.is_some(),
                inner.resolution_fixed,
                inner.resolution,
                requested,
                source,
            );
            let decision = reconcile_resize_decision(&inner, decision);
            if inner.closed || decision.is_none() {
                tracing::debug!(
                    client_w = client_size.width,
                    client_h = client_size.height,
                    applied_w = inner.width,
                    applied_h = inner.height,
                    resolution_fixed = inner.resolution_fixed,
                    closed = inner.closed,
                    "Client requested initial size; keeping the current presentation"
                );
                return DesktopSize {
                    width: inner.width,
                    height: inner.height,
                };
            }
            let decision = decision.unwrap();
            tracing::info!(client_w = client_size.width, client_h = client_size.height,
                applied_w = decision.width, applied_h = decision.height,
                server_w = inner.width, server_h = inner.height, target = ?decision.target,
                "Client requested initial size; applying presentation resize");
            if decision.target == ResizeTarget::ManagedHeadlessOutput {
                inner.output_size_unconfirmed = true;
            }
            (decision, inner.output_name.clone(), inner.headless_scale)
        };
        let lease = Arc::clone(&self.inner);
        let (decision, name, scale) = work;
        let result = tokio::task::spawn_blocking(move || {
            // The lease and permit survive cancellation of the awaiting request.
            if lease.blocking_lock().closed {
                anyhow::bail!("display is closed");
            }
            if decision.target == ResizeTarget::ManagedHeadlessOutput {
                resize_headless(name.clone(), decision.width, decision.height, scale)?;
            }
            let prepared = prepare_layout(&name, (decision.width, decision.height))?;
            Ok::<_, anyhow::Error>((prepared, permit, lease))
        })
        .await;
        let mut inner = self.inner.lock().await;
        match result {
            Ok(Ok((prepared, _permit, _lease))) if !inner.closed => {
                let layout = Arc::clone(&inner.output_layout);
                let size = apply_presentation_state_with(
                    &mut inner,
                    decision.width,
                    decision.height,
                    |_, _, _| {
                        layout.apply_prepared(prepared);
                        Ok(())
                    },
                );
                inner.pending_initial_resize = size;
            }
            Ok(Ok(_)) => {}
            Ok(Err(error)) => tracing::warn!(%error, "Initial resize did not commit"),
            Err(error) => tracing::warn!(%error, "Initial resize worker failed"),
        }
        DesktopSize {
            width: inner.width,
            height: inner.height,
        }
    }

    fn request_layout_with(
        &mut self,
        layout: DisplayControlMonitorLayout,
        mut resize_headless: impl FnMut(&str, u32, u32, f64) -> Result<()>,
        mut refresh_layout: impl FnMut(&SharedOutputLayout, &str, (u32, u32)) -> Result<()>,
    ) {
        let monitor = match layout.monitors().iter().find(|m| m.is_primary()) {
            Some(m) => m,
            None => match layout.monitors().first() {
                Some(m) => m,
                None => return,
            },
        };

        let (requested_w, requested_h) = monitor.dimensions();
        let desktop_scale = monitor.desktop_scale_factor();
        let device_scale = monitor.device_scale_factor();
        let physical = monitor.physical_dimensions();

        tracing::info!(
            w = requested_w,
            h = requested_h,
            ?desktop_scale,
            ?device_scale,
            ?physical,
            monitors = layout.monitors().len(),
            "Client requested DisplayControl layout"
        );

        let gate = Arc::clone(&self.inner.blocking_lock().resize_gate);
        let _permit = gate.blocking_lock();
        let mut inner = self.inner.blocking_lock();
        if inner.closed {
            return;
        }
        let source_size = inner
            .output_layout
            .snapshot()
            .map(|snapshot| (snapshot.output_w, snapshot.output_h));
        let decision = display_control_resize_decision(
            &layout,
            inner.output.is_some(),
            inner.resolution_fixed,
            inner.resolution,
            source_size,
        );
        let Some(decision) = reconcile_resize_decision(&inner, decision) else {
            tracing::trace!(
                resolution_fixed = inner.resolution_fixed,
                physical_output = inner.output.is_some(),
                "Ignoring DisplayControl layout for current output policy"
            );
            return;
        };

        if decision.width != (requested_w & !1) || decision.height != (requested_h & !1) {
            match decision.target {
                ResizeTarget::PhysicalPresentation => {
                    tracing::info!(
                        requested_w,
                        requested_h,
                        applied_w = decision.width,
                        applied_h = decision.height,
                        source_w = source_size.map(|(w, _)| w).unwrap_or_default(),
                        source_h = source_size.map(|(_, h)| h).unwrap_or_default(),
                        "DisplayControl presentation fitted to encoder limits and captured source"
                    );
                }
                ResizeTarget::ManagedHeadlessOutput => {
                    tracing::warn!(
                        requested_w,
                        requested_h,
                        applied_w = decision.width,
                        applied_h = decision.height,
                        "DisplayControl size exceeds H.264 software encoder policy limit; clamping"
                    );
                }
            }
        }

        match decision.target {
            ResizeTarget::ManagedHeadlessOutput => {
                tracing::info!(
                    w = decision.width,
                    h = decision.height,
                    "Client requested resize via DisplayControl"
                );

                if let Some(desktop_size) = apply_resize_decision_with(
                    &mut inner,
                    decision,
                    &mut resize_headless,
                    &mut refresh_layout,
                ) {
                    let _ = inner
                        .update_tx
                        .try_send(DisplayUpdate::Resize(desktop_size));
                }
            }
            ResizeTarget::PhysicalPresentation => {
                tracing::info!(
                    w = decision.width,
                    h = decision.height,
                    "Client requested physical-output presentation resize via DisplayControl"
                );
                if let Some(desktop_size) = apply_resize_decision_with(
                    &mut inner,
                    decision,
                    &mut resize_headless,
                    &mut refresh_layout,
                ) {
                    let _ = inner
                        .update_tx
                        .try_send(DisplayUpdate::Resize(desktop_size));
                }
            }
        }
    }
}

#[async_trait]
impl RdpServerDisplay for HyprDisplay {
    async fn size(&mut self) -> DesktopSize {
        let inner = self.inner.lock().await;
        DesktopSize {
            width: inner.width,
            height: inner.height,
        }
    }

    async fn request_initial_size(&mut self, client_size: DesktopSize) -> DesktopSize {
        self.request_initial_size_with(
            client_size,
            |name, width, height, scale| resize_headless_output(&name, width, height, scale),
            SharedOutputLayout::prepare_from_output_with_presentation,
        )
        .await
    }

    fn request_layout(&mut self, layout: DisplayControlMonitorLayout) {
        self.request_layout_with(
            layout,
            resize_headless_output,
            SharedOutputLayout::update_from_output_with_presentation,
        );
    }

    async fn updates(&mut self) -> ServerResult<Box<dyn RdpServerDisplayUpdates>> {
        // A client may skip initial-size negotiation altogether. Capture must
        // still wait for a canceled previous session's external mutation.
        let gate = Arc::clone(&self.inner.lock().await.resize_gate);
        let _permit = gate.lock_owned().await;
        // Extract stop_flag and handle before joining, to avoid holding
        // the Mutex during a blocking join() call.
        let (stop_flag, handle) = {
            let mut inner = self.inner.lock().await;
            if inner.closed {
                return Err(ServerError::reason(
                    "display closed",
                    "capture restart rejected",
                ));
            }
            drop(inner.update_rx.take());
            (Arc::clone(&inner.stop_flag), inner.capture_handle.take())
        };
        stop_flag.store(true, Ordering::Release);
        if let Some(handle) = handle {
            let _ = tokio::task::spawn_blocking(move || handle.join()).await;
        }

        let mut inner = self.inner.lock().await;
        if inner.closed {
            return Err(ServerError::reason(
                "display closed",
                "capture restart rejected",
            ));
        }

        let (tx, rx) = mpsc::channel(128);
        inner.update_tx = tx.clone();

        let pending_initial_resize = inner.pending_initial_resize.take();
        let capture_dead = Arc::new(tokio::sync::Notify::new());

        inner.stop_flag = Arc::new(AtomicBool::new(false));
        let (capture_info, capture_handle) = wayland::start_capture(
            tx,
            Arc::clone(&capture_dead),
            inner.capture_mode,
            inner.egfx_shared.clone(),
            Arc::clone(&inner.output_layout),
            inner.bitrate,
            inner.quality,
            inner.rate_control,
            inner.fps,
            inner.h264_backend,
            inner.output_name.clone(),
            pending_initial_resize,
            Arc::clone(&inner.stop_flag),
        )
        .await
        .map_err(capture_start_error)?;
        inner.capture_handle = Some(capture_handle);
        inner.output_name = capture_info.output_name;
        if let Some(snapshot) = inner.output_layout.snapshot() {
            let presentation = snapshot.presentation_geometry.presentation();
            inner.width = presentation.width as u16;
            inner.height = presentation.height as u16;
            inner.resolution = (presentation.width, presentation.height);
        } else {
            inner.width = capture_info.width as u16;
            inner.height = capture_info.height as u16;
            inner.resolution = (capture_info.width, capture_info.height);
        }

        Ok(Box::new(HyprDisplayUpdates { rx, capture_dead }))
    }
}

fn capture_start_error(error: anyhow::Error) -> ServerError {
    ServerError::reason("failed to start capture", format!("{error:#}"))
}

struct HyprDisplayUpdates {
    rx: mpsc::Receiver<DisplayUpdate>,
    /// Signaled when the capture thread exits without a stop request. The
    /// display half keeps a live sender for resize delivery, so the update
    /// channel cannot close by itself; without this signal a dead capture
    /// freezes the session instead of disconnecting it.
    capture_dead: Arc<tokio::sync::Notify>,
}

#[async_trait]
impl RdpServerDisplayUpdates for HyprDisplayUpdates {
    async fn next_update(&mut self) -> ServerResult<Option<DisplayUpdate>> {
        tokio::select! {
            biased;
            update = self.rx.recv() => Ok(update),
            _ = self.capture_dead.notified() => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_start_error_carries_the_whole_anyhow_chain() {
        let error =
            anyhow::anyhow!("no dmabuf feedback").context("binding zwlr_screencopy_manager_v1");

        let converted = capture_start_error(error);
        let rendered = format!("{converted:#}");

        assert!(
            rendered.contains("failed to start capture"),
            "context lost: {rendered}"
        );
        assert!(
            rendered.contains("binding zwlr_screencopy_manager_v1"),
            "outer context dropped: {rendered}"
        );
        assert!(
            rendered.contains("no dmabuf feedback"),
            "root cause dropped: {rendered}"
        );
    }

    #[tokio::test]
    async fn next_update_delivers_buffered_updates_before_the_death_signal() {
        let (tx, rx) = mpsc::channel(4);
        let capture_dead = Arc::new(tokio::sync::Notify::new());
        let mut updates = HyprDisplayUpdates {
            rx,
            capture_dead: Arc::clone(&capture_dead),
        };

        tx.send(DisplayUpdate::Resize(DesktopSize {
            width: 800,
            height: 600,
        }))
        .await
        .unwrap();
        capture_dead.notify_one();

        assert!(matches!(
            updates.next_update().await.unwrap(),
            Some(DisplayUpdate::Resize(_))
        ));
        // With the queue drained, the stored death permit disconnects.
        assert!(updates.next_update().await.unwrap().is_none());
        drop(tx);
    }

    #[tokio::test]
    async fn next_update_disconnects_when_capture_dies_with_a_live_sender() {
        // The display half keeps a sender clone for resizes, so the channel
        // alone can never close; the death signal must end the session.
        let (tx, rx) = mpsc::channel::<DisplayUpdate>(4);
        let capture_dead = Arc::new(tokio::sync::Notify::new());
        let mut updates = HyprDisplayUpdates {
            rx,
            capture_dead: Arc::clone(&capture_dead),
        };

        capture_dead.notify_one();

        assert!(updates.next_update().await.unwrap().is_none());
        drop(tx);
    }

    #[test]
    fn h264_software_limit_keeps_supported_landscape_size() {
        assert_eq!(clamp_to_h264_software_limits(1920, 1200), (1920, 1200));
        assert_eq!(clamp_to_h264_software_limits(3840, 2160), (3840, 2160));
    }

    #[test]
    fn h264_software_limit_scales_ultrawide_client_size() {
        assert_eq!(clamp_to_h264_software_limits(5120, 1440), (3840, 1080));
    }

    #[test]
    fn h264_software_limit_scales_portrait_size() {
        assert_eq!(clamp_to_h264_software_limits(1440, 5120), (1080, 3840));
    }

    #[test]
    fn h264_software_limit_rounds_to_even_dimensions() {
        assert_eq!(clamp_to_h264_software_limits(5121, 1441), (3840, 1080));
    }
}

#[cfg(test)]
mod output_downscaling {
    use super::*;
    use crate::egfx::{EgfxCodecPolicy, DEFAULT_MAX_FRAMES_IN_FLIGHT};
    use ironrdp_displaycontrol::pdu::{
        DeviceScaleFactor, DisplayControlMonitorLayout, MonitorLayoutEntry, MonitorOrientation,
    };

    fn single_primary(width: u32, height: u32) -> DisplayControlMonitorLayout {
        DisplayControlMonitorLayout::new(&[MonitorLayoutEntry::new_primary(width, height).unwrap()])
            .unwrap()
    }

    fn physical_display_for_callback_test(
        resolution: (u32, u32),
    ) -> (
        HyprDisplay,
        mpsc::Receiver<DisplayUpdate>,
        Arc<EgfxShared>,
        Arc<SharedOutputLayout>,
    ) {
        physical_display_for_callback_test_with_source(resolution, resolution)
    }

    fn physical_display_for_callback_test_with_source(
        source: (u32, u32),
        resolution: (u32, u32),
    ) -> (
        HyprDisplay,
        mpsc::Receiver<DisplayUpdate>,
        Arc<EgfxShared>,
        Arc<SharedOutputLayout>,
    ) {
        let (tx, rx) = mpsc::channel(4);
        let shared = Arc::new(EgfxShared::with_codec_policy(
            DEFAULT_MAX_FRAMES_IN_FLIGHT,
            EgfxCodecPolicy::Auto,
        ));
        shared.set_surface_size(resolution.0 as u16, resolution.1 as u16);
        let output_layout = Arc::new(SharedOutputLayout::new());
        output_layout
            .update_snapshot_for_test(
                "DP-1", source.0, source.1, source.0, source.1, 0, 0, resolution,
            )
            .expect("initial physical layout");
        let inner = HyprDisplayInner {
            width: resolution.0 as u16,
            height: resolution.1 as u16,
            resolution,
            capture_mode: CaptureMode::Ext,
            output_name: "DP-1".into(),
            egfx_shared: Some(Arc::clone(&shared)),
            output_layout: Arc::clone(&output_layout),
            update_tx: tx,
            update_rx: None,
            bitrate: 1_000_000,
            quality: 23,
            rate_control: H264RateControl::Vbr,
            h264_backend: H264BackendPolicy::Auto,
            fps: 30,
            output: Some("DP-1".into()),
            headless_scale: 1.0,
            resolution_fixed: false,
            stop_flag: Arc::new(AtomicBool::new(false)),
            capture_handle: None,
            headless_guard: None,
            pending_initial_resize: None,
            resize_gate: Arc::new(Mutex::new(())),
            closed: false,
            output_size_unconfirmed: false,
        };

        (
            HyprDisplay {
                inner: Arc::new(Mutex::new(inner)),
            },
            rx,
            shared,
            output_layout,
        )
    }

    fn refresh_physical_layout_for_test(
        layout: &SharedOutputLayout,
        output_name: &str,
        presentation: (u32, u32),
    ) -> Result<()> {
        let snapshot = layout.snapshot().expect("existing physical layout");
        layout.update_snapshot_for_test(
            output_name,
            snapshot.output_w,
            snapshot.output_h,
            snapshot.layout_extent_w,
            snapshot.layout_extent_h,
            snapshot.output_offset_x,
            snapshot.output_offset_y,
            presentation,
        )
    }

    #[test]
    fn presentation_fits_a_hidpi_request_to_the_source_instead_of_upscaling() {
        assert_eq!(
            normalize_presentation_size((2560, 1440), (1920, 1080)),
            (1920, 1080)
        );
    }

    #[test]
    fn presentation_fit_lands_on_the_source_exactly() {
        for request in [(1984, 1116), (2944, 1656), (2560, 1440)] {
            assert_eq!(
                normalize_presentation_size(request, (1920, 1080)),
                (1920, 1080),
                "request {request:?}"
            );
        }
        assert_eq!(
            normalize_presentation_size((2656, 1494), (2560, 1440)),
            (2560, 1440)
        );
    }

    #[test]
    fn physical_output_startup_keeps_pinned_resolution_above_the_source() {
        assert_eq!(
            startup_presentation_size(true, true, (2560, 1440), (1920, 1080)),
            (2560, 1440)
        );
    }

    #[test]
    fn presentation_fit_keeps_the_requested_aspect_ratio() {
        assert_eq!(
            normalize_presentation_size((3000, 1200), (1920, 1080)),
            (2700, 1080)
        );
    }

    #[test]
    fn presentation_fit_lands_on_the_source_when_the_height_limits_it() {
        assert_eq!(
            normalize_presentation_size((3840, 1200), (1920, 1080)),
            (3456, 1080)
        );
    }

    #[test]
    fn presentation_fit_runs_before_the_encoder_limits() {
        // Clamping first would land two pixels under the source and lose the
        // identity geometry the zero-copy capture path needs.
        assert_eq!(
            normalize_presentation_size((3842, 2162), (3840, 2160)),
            (3840, 2160)
        );
    }

    #[test]
    fn presentation_fit_keeps_even_dimensions_for_the_encoder() {
        assert_eq!(
            normalize_presentation_size((2000, 1125), (1920, 1080)),
            (1920, 1080)
        );
    }

    #[test]
    fn presentation_fit_never_falls_below_the_encoder_minimum() {
        assert_eq!(normalize_presentation_size((1920, 1080), (1, 1)), (2, 2));
        assert_eq!(normalize_presentation_size((640, 480), (2, 1)), (2, 2));
    }

    #[test]
    fn an_odd_request_is_evened_before_it_is_fitted() {
        // DisplayControl layouts carry odd sizes; fitting the raw request
        // instead of the evened one lands two rows off the source.
        assert_eq!(
            normalize_presentation_size((1924, 1085), (1920, 1080)),
            (1920, 1080)
        );
    }

    #[test]
    fn an_unknown_source_size_is_rejected_instead_of_fitted() {
        assert_eq!(normalize_presentation_size((1920, 1080), (0, 0)), (0, 0));
        assert_eq!(normalize_presentation_size((1920, 1080), (1920, 0)), (0, 0));
        assert_eq!(
            initial_size_resize_decision(true, false, (1920, 1080), (1600, 900), Some((0, 0))),
            None
        );
    }

    #[test]
    fn presentation_at_or_below_the_source_is_left_alone() {
        assert_eq!(
            normalize_presentation_size((1920, 1080), (1920, 1080)),
            (1920, 1080)
        );
        assert_eq!(
            normalize_presentation_size((960, 540), (1920, 1080)),
            (960, 540)
        );
        assert_eq!(
            normalize_presentation_size((1920, 1200), (3840, 1080)),
            (1920, 1200)
        );
    }

    #[test]
    fn presentation_fit_converges_when_a_client_repeats_its_request() {
        let source = (1920, 1080);
        for request in [(2560, 1440), (4000, 1200), (3000, 3000)] {
            let fitted = normalize_presentation_size(request, source);
            assert_eq!(normalize_presentation_size(fitted, source), fitted);
        }
    }

    #[test]
    fn physical_output_displaycontrol_fits_a_hidpi_request_to_the_source() {
        let decision = display_control_resize_decision(
            &single_primary(2560, 1440),
            true,
            false,
            (1280, 720),
            Some((1920, 1080)),
        )
        .unwrap();

        assert_eq!(decision.target, ResizeTarget::PhysicalPresentation);
        assert_eq!((decision.width, decision.height), (1920, 1080));
    }

    #[test]
    fn physical_output_startup_keeps_explicit_resolution_for_letterboxing() {
        assert_eq!(
            startup_presentation_size(true, true, (1920, 1200), (3840, 1080)),
            (1920, 1200)
        );
        assert_eq!(
            startup_presentation_size(true, true, (1600, 900), (3840, 2160)),
            (1600, 900)
        );
    }

    #[test]
    fn physical_output_startup_uses_source_size_when_resolution_is_omitted() {
        assert_eq!(
            startup_presentation_size(true, false, (1920, 1080), (3840, 2160)),
            (3840, 2160)
        );
    }

    #[test]
    fn headless_startup_keeps_configured_session_resolution() {
        assert_eq!(
            startup_presentation_size(false, false, (1920, 1080), (3840, 2160)),
            (1920, 1080)
        );
    }

    #[test]
    fn physical_output_initial_size_updates_presentation_only() {
        let decision = initial_size_resize_decision(
            true,
            false,
            (3840, 2160),
            (1600, 900),
            Some((3840, 2160)),
        )
        .unwrap();

        assert_eq!(decision.target, ResizeTarget::PhysicalPresentation);
        assert_eq!((decision.width, decision.height), (1600, 900));
    }

    #[test]
    fn physical_output_initial_size_keeps_client_size_for_letterboxing() {
        let decision = initial_size_resize_decision(
            true,
            false,
            (3840, 1080),
            (1920, 1200),
            Some((3840, 1080)),
        )
        .unwrap();

        assert_eq!(decision.target, ResizeTarget::PhysicalPresentation);
        assert_eq!((decision.width, decision.height), (1920, 1200));
    }

    #[test]
    fn physical_output_size_negotiation_reaches_fixed_point_immediately() {
        // Regression: reshaping the requested size to the source aspect made
        // the client re-request every applied size, walking a shrinking
        // staircase (728x408 → 724x408 → 724x406 → … → 704x396) with a full
        // capture and encoder restart on every step.
        let source = Some((3840, 2160));
        let decision =
            initial_size_resize_decision(true, false, (3840, 2160), (728, 408), source).unwrap();
        assert_eq!((decision.width, decision.height), (728, 408));

        // The client echoes the applied size back; the negotiation must stop.
        let echo = initial_size_resize_decision(
            true,
            false,
            (decision.width, decision.height),
            (decision.width, decision.height),
            source,
        );
        assert_eq!(echo, None);
    }

    #[tokio::test]
    async fn physical_output_initial_size_callback_updates_presentation_state() {
        let (mut display, _rx, shared, _layout) =
            physical_display_for_callback_test_with_source((3840, 1080), (3840, 1080));

        let size = display
            .request_initial_size_with(
                DesktopSize {
                    width: 1920,
                    height: 1200,
                },
                |_name, _width, _height, _scale| {
                    panic!("physical output must not resize headless output")
                },
                |name, size| {
                    SharedOutputLayout::prepare_snapshot_for_test(
                        name, 3840, 1080, 3840, 1080, 0, 0, size,
                    )
                },
            )
            .await;

        assert_eq!(
            size,
            DesktopSize {
                width: 1920,
                height: 1200
            }
        );
        assert_eq!(shared.get_surface_size(), (1920, 1200));

        let inner = display.inner.lock().await;
        assert_eq!(inner.resolution, (1920, 1200));
        assert_eq!(
            inner.pending_initial_resize,
            Some(DesktopSize {
                width: 1920,
                height: 1200
            })
        );
    }

    #[tokio::test]
    async fn physical_output_initial_size_callback_layout_failure_preserves_state() {
        let (mut display, _rx, shared, _layout) = physical_display_for_callback_test((3840, 2160));

        let size = display
            .request_initial_size_with(
                DesktopSize {
                    width: 1600,
                    height: 900,
                },
                |_name, _width, _height, _scale| {
                    panic!("physical output must not resize headless output")
                },
                |_name, _presentation| anyhow::bail!("layout refresh failed"),
            )
            .await;

        assert_eq!(
            size,
            DesktopSize {
                width: 3840,
                height: 2160
            }
        );
        assert_eq!(shared.get_surface_size(), (3840, 2160));

        let inner = display.inner.lock().await;
        assert_eq!(inner.resolution, (3840, 2160));
        assert_eq!(inner.pending_initial_resize, None);
    }

    #[test]
    fn physical_output_initial_size_uses_desktop_size_policy_not_displaycontrol_layout_policy() {
        let decision =
            initial_size_resize_decision(true, false, (3840, 2160), (100, 100), Some((3840, 2160)))
                .unwrap();

        assert_eq!(decision.target, ResizeTarget::PhysicalPresentation);
        assert_eq!((decision.width, decision.height), (100, 100));
    }

    #[test]
    fn fixed_zero_and_unchanged_initial_size_requests_are_noops() {
        assert_eq!(
            initial_size_resize_decision(true, true, (1920, 1080), (1600, 900), Some((1920, 1080))),
            None
        );
        assert_eq!(
            initial_size_resize_decision(true, false, (1920, 1080), (0, 900), Some((1920, 1080))),
            None
        );
        assert_eq!(
            initial_size_resize_decision(
                true,
                false,
                (1920, 1200),
                (1920, 1200),
                Some((3840, 1080))
            ),
            None
        );
    }

    #[test]
    fn physical_output_displaycontrol_accepts_single_primary_at_origin() {
        let decision = display_control_resize_decision(
            &single_primary(1280, 720),
            true,
            false,
            (1920, 1080),
            Some((1920, 1080)),
        )
        .unwrap();

        assert_eq!(decision.target, ResizeTarget::PhysicalPresentation);
        assert_eq!((decision.width, decision.height), (1280, 720));
    }

    #[test]
    fn physical_output_displaycontrol_keeps_monitor_size_for_letterboxing() {
        let decision = display_control_resize_decision(
            &single_primary(1920, 1200),
            true,
            false,
            (3840, 1080),
            Some((3840, 1080)),
        )
        .unwrap();

        assert_eq!(decision.target, ResizeTarget::PhysicalPresentation);
        assert_eq!((decision.width, decision.height), (1920, 1200));
    }

    #[test]
    fn physical_output_displaycontrol_callback_emits_presentation_resize() {
        let (mut display, mut rx, shared, _layout) =
            physical_display_for_callback_test((1920, 1080));

        display.request_layout_with(
            single_primary(1280, 720),
            |_name, _width, _height, _scale| {
                panic!("physical output must not resize headless output")
            },
            refresh_physical_layout_for_test,
        );

        match rx.try_recv().expect("resize update") {
            DisplayUpdate::Resize(size) => {
                assert_eq!(
                    size,
                    DesktopSize {
                        width: 1280,
                        height: 720
                    }
                );
            }
            other => panic!("expected resize update, got {other:?}"),
        }
        assert_eq!(shared.get_surface_size(), (1280, 720));

        let inner = display.inner.blocking_lock();
        assert_eq!(inner.resolution, (1280, 720));
    }

    #[test]
    fn physical_output_displaycontrol_callback_layout_failure_emits_no_resize() {
        let (mut display, mut rx, shared, _layout) =
            physical_display_for_callback_test((1920, 1080));

        display.request_layout_with(
            single_primary(1280, 720),
            |_name, _width, _height, _scale| {
                panic!("physical output must not resize headless output")
            },
            |_layout, _name, _presentation| anyhow::bail!("layout refresh failed"),
        );

        assert!(rx.try_recv().is_err());
        assert_eq!(shared.get_surface_size(), (1920, 1080));

        let inner = display.inner.blocking_lock();
        assert_eq!(inner.resolution, (1920, 1080));
    }

    #[test]
    fn physical_output_displaycontrol_ignores_physical_size_and_scale_fields() {
        let monitor = MonitorLayoutEntry::new_primary(1280, 720)
            .unwrap()
            .with_physical_dimensions(1000, 500)
            .unwrap()
            .with_desktop_scale_factor(150)
            .unwrap()
            .with_device_scale_factor(DeviceScaleFactor::Scale140Percent);
        let layout = DisplayControlMonitorLayout::new(&[monitor]).unwrap();

        let decision =
            display_control_resize_decision(&layout, true, false, (1920, 1080), Some((1920, 1080)))
                .unwrap();

        assert_eq!(decision.target, ResizeTarget::PhysicalPresentation);
        assert_eq!((decision.width, decision.height), (1280, 720));
    }

    #[test]
    fn physical_output_displaycontrol_rejects_multi_monitor_layouts() {
        let monitors = [
            MonitorLayoutEntry::new_primary(1280, 720).unwrap(),
            MonitorLayoutEntry::new_secondary(1024, 768).unwrap(),
        ];
        let layout = DisplayControlMonitorLayout::new(&monitors).unwrap();

        assert_eq!(
            display_control_resize_decision(&layout, true, false, (1920, 1080), Some((1920, 1080))),
            None
        );
    }

    #[test]
    fn physical_output_displaycontrol_rejects_valid_rotated_orientation() {
        let monitor = MonitorLayoutEntry::new_primary(1280, 720)
            .unwrap()
            .with_orientation(MonitorOrientation::Portrait);
        let layout = DisplayControlMonitorLayout::new(&[monitor]).unwrap();

        assert_eq!(
            display_control_resize_decision(&layout, true, false, (1920, 1080), Some((1920, 1080))),
            None
        );
    }

    #[test]
    fn physical_output_displaycontrol_rejects_layouts_over_advertised_area_cap() {
        assert_eq!(
            display_control_resize_decision(
                &single_primary(8192, 2000),
                true,
                false,
                (1920, 1080),
                Some((1920, 1080))
            ),
            None
        );
    }

    #[test]
    fn physical_output_displaycontrol_normalizes_odd_height_for_h264() {
        let decision = display_control_resize_decision(
            &single_primary(1280, 721),
            true,
            false,
            (1920, 1080),
            Some((1920, 1080)),
        )
        .unwrap();

        assert_eq!(decision.target, ResizeTarget::PhysicalPresentation);
        assert_eq!((decision.width, decision.height), (1280, 720));
    }

    #[test]
    fn physical_output_displaycontrol_fixed_and_unchanged_requests_are_noops() {
        assert_eq!(
            display_control_resize_decision(
                &single_primary(1280, 720),
                true,
                true,
                (1920, 1080),
                Some((1920, 1080))
            ),
            None
        );
        assert_eq!(
            display_control_resize_decision(
                &single_primary(1920, 1080),
                true,
                false,
                (1920, 1080),
                Some((1920, 1080))
            ),
            None
        );
    }
}

#[cfg(test)]
mod managed_headless_resize {
    use super::*;
    use ironrdp_displaycontrol::pdu::{DisplayControlMonitorLayout, MonitorLayoutEntry};

    fn single_primary(width: u32, height: u32) -> DisplayControlMonitorLayout {
        DisplayControlMonitorLayout::new(&[MonitorLayoutEntry::new_primary(width, height).unwrap()])
            .unwrap()
    }

    fn headless_inner_for_resize_test_with_tx(
        resolution: (u32, u32),
        tx: mpsc::Sender<DisplayUpdate>,
    ) -> HyprDisplayInner {
        let output_layout = Arc::new(SharedOutputLayout::new());
        output_layout
            .update_snapshot_for_test(
                "HEADLESS-1",
                resolution.0,
                resolution.1,
                resolution.0,
                resolution.1,
                0,
                0,
                resolution,
            )
            .expect("initial headless layout");
        HyprDisplayInner {
            width: resolution.0 as u16,
            height: resolution.1 as u16,
            resolution,
            capture_mode: CaptureMode::Ext,
            output_name: "HEADLESS-1".into(),
            egfx_shared: None,
            output_layout,
            update_tx: tx,
            update_rx: None,
            bitrate: 1_000_000,
            quality: 23,
            rate_control: H264RateControl::Vbr,
            h264_backend: H264BackendPolicy::Auto,
            fps: 30,
            output: None,
            headless_scale: 1.0,
            resolution_fixed: false,
            stop_flag: Arc::new(AtomicBool::new(false)),
            capture_handle: None,
            headless_guard: None,
            pending_initial_resize: None,
            resize_gate: Arc::new(Mutex::new(())),
            closed: false,
            output_size_unconfirmed: false,
        }
    }

    fn headless_inner_for_resize_test(resolution: (u32, u32)) -> HyprDisplayInner {
        let (tx, _rx) = mpsc::channel(4);
        headless_inner_for_resize_test_with_tx(resolution, tx)
    }

    fn headless_display_for_callback_test(
        resolution: (u32, u32),
    ) -> (HyprDisplay, mpsc::Receiver<DisplayUpdate>) {
        headless_display_for_callback_test_with_scale(resolution, 1.0)
    }

    fn headless_display_for_callback_test_with_scale(
        resolution: (u32, u32),
        headless_scale: f64,
    ) -> (HyprDisplay, mpsc::Receiver<DisplayUpdate>) {
        let (tx, rx) = mpsc::channel(4);
        let mut inner = headless_inner_for_resize_test_with_tx(resolution, tx);
        inner.headless_scale = headless_scale;
        (
            HyprDisplay {
                inner: Arc::new(Mutex::new(inner)),
            },
            rx,
        )
    }

    fn prepare_headless_layout_for_test(
        name: &str,
        size: (u32, u32),
    ) -> Result<PreparedOutputLayout> {
        SharedOutputLayout::prepare_snapshot_for_test(
            name, size.0, size.1, size.0, size.1, 0, 0, size,
        )
    }

    fn refresh_headless_layout_for_test(
        layout: &SharedOutputLayout,
        output_name: &str,
        presentation: (u32, u32),
    ) -> Result<()> {
        layout.update_snapshot_for_test(
            output_name,
            presentation.0,
            presentation.1,
            presentation.0,
            presentation.1,
            0,
            0,
            presentation,
        )
    }

    #[test]
    fn managed_headless_initial_size_still_targets_headless_output_resize() {
        let decision =
            initial_size_resize_decision(false, false, (1920, 1080), (1600, 900), None).unwrap();

        assert_eq!(decision.target, ResizeTarget::ManagedHeadlessOutput);
        assert_eq!((decision.width, decision.height), (1600, 900));
    }

    #[tokio::test]
    async fn managed_headless_initial_size_callback_resizes_headless_and_updates_pending_resize() {
        let (mut display, _rx) = headless_display_for_callback_test_with_scale((1920, 1080), 1.5);
        let called = Arc::new(std::sync::Mutex::new(None));
        let record = Arc::clone(&called);

        let size = display
            .request_initial_size_with(
                DesktopSize {
                    width: 1600,
                    height: 900,
                },
                move |name, width, height, scale| {
                    *record.lock().unwrap() = Some((name, width, height, scale));
                    Ok(())
                },
                prepare_headless_layout_for_test,
            )
            .await;

        assert_eq!(
            *called.lock().unwrap(),
            Some(("HEADLESS-1".into(), 1600, 900, 1.5))
        );
        assert_eq!(
            size,
            DesktopSize {
                width: 1600,
                height: 900
            }
        );

        let inner = display.inner.lock().await;
        assert_eq!(inner.resolution, (1600, 900));
        assert_eq!(
            inner.pending_initial_resize,
            Some(DesktopSize {
                width: 1600,
                height: 900
            })
        );
    }

    #[tokio::test]
    async fn managed_headless_initial_resize_runs_without_holding_the_display_lock() {
        // Regression: request_initial_size held self.inner across the blocking
        // Hyprland resize (up to 5s), which stalled HyprDisplayHandle::shutdown()
        // — it takes this inner lock directly. The resize must now run with the
        // lock released. This injected callback runs on the same spawn_blocking
        // boundary as the production IPC operation.
        let (mut display, _rx) = headless_display_for_callback_test((1920, 1080));
        let probe = Arc::clone(&display.inner);

        let size = display
            .request_initial_size_with(
                DesktopSize {
                    width: 1600,
                    height: 900,
                },
                move |_name, _width, _height, _scale| {
                    assert!(probe.try_lock().is_ok());
                    Ok(())
                },
                prepare_headless_layout_for_test,
            )
            .await;

        assert_eq!(
            size,
            DesktopSize {
                width: 1600,
                height: 900
            }
        );
    }

    #[tokio::test]
    async fn managed_headless_initial_resize_failure_preserves_state() {
        // Phase 2 failure path on the initial-size route: a failing headless
        // resize must keep the current presentation and never commit Phase 3
        // (no layout refresh, no pending_initial_resize).
        let (mut display, _rx) = headless_display_for_callback_test((1920, 1080));

        let size = display
            .request_initial_size_with(
                DesktopSize {
                    width: 1600,
                    height: 900,
                },
                |_name, _width, _height, _scale| anyhow::bail!("resize failed"),
                |_output_name, _presentation| {
                    panic!("layout refresh must not run after a failed headless resize")
                },
            )
            .await;

        assert_eq!(
            size,
            DesktopSize {
                width: 1920,
                height: 1080
            }
        );
        let inner = display.inner.lock().await;
        assert_eq!(inner.resolution, (1920, 1080));
        assert_eq!(inner.pending_initial_resize, None);
    }

    #[tokio::test]
    async fn initial_resize_clean_noop_preserves_pending_state_without_ipc() {
        for (fixed, requested) in [
            (false, (1920, 1080)),
            (false, (0, 900)),
            (true, (1600, 900)),
        ] {
            let (mut display, _rx) = headless_display_for_callback_test((1920, 1080));
            let original = DesktopSize {
                width: 1920,
                height: 1080,
            };
            {
                let mut inner = display.inner.lock().await;
                inner.resolution_fixed = fixed;
                inner.pending_initial_resize = Some(original);
            }
            let actual = display
                .request_initial_size_with(
                    DesktopSize {
                        width: requested.0,
                        height: requested.1,
                    },
                    |_, _, _, _| panic!("a clean no-op must not resize the output"),
                    |_, _| panic!("a clean no-op must not query the layout"),
                )
                .await;
            assert_eq!(actual, original);
            let inner = display.inner.lock().await;
            assert_eq!(inner.pending_initial_resize, Some(original));
            assert!(!inner.output_size_unconfirmed);
            assert_eq!(
                inner.output_layout.snapshot().unwrap().geometry_generation,
                0
            );
        }
    }

    // The blocking callback has its own deadline so a scheduler regression
    // fails assertions instead of hanging the test process indefinitely.
    fn wait_for_release(rx: std::sync::mpsc::Receiver<()>) {
        rx.recv_timeout(Duration::from_secs(3))
            .expect("test must release worker");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn initial_resize_ipc_keeps_runtime_and_display_lock_available() {
        for physical in [false, true] {
            for block_preparation in [false, true] {
                if physical && !block_preparation {
                    continue;
                }
                let (mut display, _rx) = headless_display_for_callback_test((1920, 1080));
                if physical {
                    display.inner.lock().await.output = Some("HEADLESS-1".into());
                }
                let inner = Arc::clone(&display.inner);
                let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
                let (release_tx, release_rx) = std::sync::mpsc::channel();
                let barrier = Arc::new(std::sync::Mutex::new(Some((entered_tx, release_rx))));
                let resize_barrier = Arc::clone(&barrier);
                let task = tokio::spawn(async move {
                    display
                        .request_initial_size_with(
                            DesktopSize {
                                width: 1600,
                                height: 900,
                            },
                            move |_, _, _, _| {
                                assert!(!physical);
                                if !block_preparation {
                                    let (tx, rx) = resize_barrier.lock().unwrap().take().unwrap();
                                    tx.send(()).unwrap();
                                    wait_for_release(rx);
                                }
                                Ok(())
                            },
                            move |name, size| {
                                if block_preparation {
                                    let (tx, rx) = barrier.lock().unwrap().take().unwrap();
                                    tx.send(()).unwrap();
                                    wait_for_release(rx);
                                }
                                prepare_headless_layout_for_test(name, size)
                            },
                        )
                        .await
                });
                tokio::time::timeout(Duration::from_secs(1), entered_rx)
                    .await
                    .unwrap()
                    .unwrap();
                tokio::time::timeout(Duration::from_millis(500), async {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    let state = inner.lock().await;
                    assert_eq!(state.resolution, (1920, 1080));
                    assert_eq!(state.pending_initial_resize, None);
                })
                .await
                .expect("runtime and display lock must progress during IPC");
                release_tx.send(()).unwrap();
                assert_eq!(
                    task.await.unwrap(),
                    DesktopSize {
                        width: 1600,
                        height: 900
                    }
                );
            }
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn initial_resize_cancelled_job_cannot_overtake_next_request() {
        use std::future::Future;
        for via_display_control in [false, true] {
            for target in [(1920, 1080), (1280, 720)] {
                let (mut display, _rx) = headless_display_for_callback_test((1920, 1080));
                let inner = Arc::clone(&display.inner);
                let shared = Arc::new(EgfxShared::with_codec_policy(
                    3,
                    crate::egfx::EgfxCodecPolicy::Auto,
                ));
                shared.set_surface_size(1920, 1080);
                inner.lock().await.egfx_shared = Some(Arc::clone(&shared));
                let generation = shared.generation();
                let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
                let (release_tx, release_rx) = std::sync::mpsc::channel();
                let external = Arc::new(std::sync::Mutex::new((1920, 1080)));
                let old_external = Arc::clone(&external);
                let old = tokio::spawn(async move {
                    display
                        .request_initial_size_with(
                            DesktopSize {
                                width: 1600,
                                height: 900,
                            },
                            move |_, w, h, _| {
                                entered_tx.send(()).unwrap();
                                wait_for_release(release_rx);
                                *old_external.lock().unwrap() = (w, h);
                                Ok(())
                            },
                            prepare_headless_layout_for_test,
                        )
                        .await
                });
                entered_rx.await.unwrap();
                old.abort();
                assert!(old.await.unwrap_err().is_cancelled());
                let gate = Arc::clone(&inner.lock().await.resize_gate);
                assert!(gate.try_lock().is_err(), "detached job must retain permit");
                assert_eq!(inner.lock().await.pending_initial_resize, None);
                assert_eq!(shared.generation(), generation);
                assert_eq!(shared.get_surface_size(), (1920, 1080));
                let mut replacement = HyprDisplay {
                    inner: Arc::clone(&inner),
                };
                let new_external = Arc::clone(&external);
                let called = Arc::new(AtomicBool::new(false));
                let new_called = Arc::clone(&called);
                let (started_tx, started_rx) = tokio::sync::oneshot::channel();
                let replacement = tokio::spawn(async move {
                    if via_display_control {
                        tokio::task::spawn_blocking(move || {
                            started_tx.send(()).unwrap();
                            replacement.request_layout_with(
                                single_primary(target.0, target.1),
                                move |_, w, h, _| {
                                    new_called.store(true, Ordering::Release);
                                    *new_external.lock().unwrap() = (w, h);
                                    Ok(())
                                },
                                refresh_headless_layout_for_test,
                            )
                        })
                        .await
                        .unwrap();
                    } else {
                        let future = replacement.request_initial_size_with(
                            DesktopSize {
                                width: target.0 as u16,
                                height: target.1 as u16,
                            },
                            move |_, w, h, _| {
                                new_called.store(true, Ordering::Release);
                                *new_external.lock().unwrap() = (w, h);
                                Ok(())
                            },
                            prepare_headless_layout_for_test,
                        );
                        tokio::pin!(future);
                        std::future::poll_fn(|cx| {
                            assert!(future.as_mut().poll(cx).is_pending());
                            std::task::Poll::Ready(())
                        })
                        .await;
                        started_tx.send(()).unwrap();
                        future.await;
                    }
                });
                started_rx.await.unwrap();
                assert!(gate.try_lock().is_err());
                assert!(!called.load(Ordering::Acquire));
                release_tx.send(()).unwrap();
                tokio::time::timeout(Duration::from_secs(1), replacement)
                    .await
                    .unwrap()
                    .unwrap();
                assert!(called.load(Ordering::Acquire));
                assert_eq!(*external.lock().unwrap(), target);
                let state = inner.lock().await;
                assert_eq!(state.resolution, target);
                assert!(!state.output_size_unconfirmed);
                assert_eq!(state.output_layout.snapshot().unwrap().output_w, target.0);
                assert_eq!(
                    shared.get_surface_size(),
                    (target.0 as u16, target.1 as u16)
                );
            }
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn initial_resize_shutdown_waits_without_locking_out_runtime() {
        let (mut display, _rx) = headless_display_for_callback_test((1920, 1080));
        let inner = Arc::clone(&display.inner);
        let handle = HyprDisplayHandle {
            inner: Arc::clone(&inner),
        };
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let task = tokio::spawn(async move {
            display
                .request_initial_size_with(
                    DesktopSize {
                        width: 1600,
                        height: 900,
                    },
                    move |_, _, _, _| {
                        entered_tx.send(()).unwrap();
                        wait_for_release(release_rx);
                        Ok(())
                    },
                    prepare_headless_layout_for_test,
                )
                .await
        });
        entered_rx.await.unwrap();
        task.abort();
        let _ = task.await;
        let shutdown = tokio::spawn(async move { handle.shutdown().await });
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if inner.lock().await.closed {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(!shutdown.is_finished());
        assert!(inner.lock().await.stop_flag.load(Ordering::Acquire));
        release_tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(1), shutdown)
            .await
            .unwrap()
            .unwrap();
        let mut display = HyprDisplay {
            inner: Arc::clone(&inner),
        };
        display
            .request_initial_size_with(
                DesktopSize {
                    width: 1600,
                    height: 900,
                },
                |_, _, _, _| panic!("closed display must not issue IPC"),
                |_, _| panic!("closed display must not prepare layout"),
            )
            .await;
        tokio::task::spawn_blocking(move || {
            display.request_layout_with(
                single_primary(1600, 900),
                |_, _, _, _| panic!("closed DisplayControl must not issue IPC"),
                |_, _, _| panic!("closed DisplayControl must not refresh layout"),
            )
        })
        .await
        .unwrap();
        assert_eq!(inner.lock().await.pending_initial_resize, None);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn initial_resize_cancelled_job_retains_output_owner_until_completion() {
        let (mut display, _rx) = headless_display_for_callback_test((1920, 1080));
        let owner = Arc::downgrade(&display.inner);
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let task = tokio::spawn(async move {
            display
                .request_initial_size_with(
                    DesktopSize {
                        width: 1600,
                        height: 900,
                    },
                    move |_, _, _, _| {
                        entered_tx.send(()).unwrap();
                        wait_for_release(release_rx);
                        Ok(())
                    },
                    prepare_headless_layout_for_test,
                )
                .await
        });
        entered_rx.await.unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(
            owner.upgrade().is_some(),
            "detached IPC must retain output cleanup ownership"
        );
        release_tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while owner.upgrade().is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("completed job must release its final display lease");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn capture_updates_waits_for_cancelled_resize_and_rejects_closed_display() {
        let (mut display, _rx) = headless_display_for_callback_test((1920, 1080));
        let inner = Arc::clone(&display.inner);
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let task = tokio::spawn(async move {
            display
                .request_initial_size_with(
                    DesktopSize {
                        width: 1600,
                        height: 900,
                    },
                    move |_, _, _, _| {
                        entered_tx.send(()).unwrap();
                        wait_for_release(release_rx);
                        Ok(())
                    },
                    prepare_headless_layout_for_test,
                )
                .await
        });
        entered_rx.await.unwrap();
        task.abort();
        let _ = task.await;
        let mut next = HyprDisplay {
            inner: Arc::clone(&inner),
        };
        {
            let mut updates = next.updates();
            std::future::poll_fn(|cx| {
                assert!(
                    updates.as_mut().poll(cx).is_pending(),
                    "updates must wait before touching Wayland"
                );
                std::task::Poll::Ready(())
            })
            .await;
            assert!(!inner.lock().await.stop_flag.load(Ordering::Acquire));
            // Drop the waiting callback before opening its gate: no compositor
            // exists in this deterministic boundary test.
        }
        inner.lock().await.closed = true;
        release_tx.send(()).unwrap();
        let result = tokio::time::timeout(Duration::from_secs(1), next.updates())
            .await
            .unwrap();
        assert!(result.is_err());
        assert!(result.err().unwrap().to_string().contains("display closed"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn capture_updates_rechecks_closed_after_capture_join() {
        let (mut display, _rx) = headless_display_for_callback_test((1920, 1080));
        let inner = Arc::clone(&display.inner);
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        inner.lock().await.capture_handle =
            Some(std::thread::spawn(move || wait_for_release(release_rx)));
        let mut updates = display.updates();
        std::future::poll_fn(|cx| {
            assert!(updates.as_mut().poll(cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        assert!(inner.lock().await.capture_handle.is_none());
        let shutdown = tokio::spawn({
            let inner = Arc::clone(&inner);
            async move {
                HyprDisplayHandle { inner }.shutdown().await;
            }
        });
        tokio::time::timeout(Duration::from_secs(1), async {
            while !inner.lock().await.closed {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        release_tx.send(()).unwrap();
        let result = tokio::time::timeout(Duration::from_secs(1), updates)
            .await
            .unwrap();
        assert!(result.err().unwrap().to_string().contains("display closed"));
        tokio::time::timeout(Duration::from_secs(1), shutdown)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn initial_resize_preparation_panic_preserves_state() {
        for panic in [false, true] {
            let (mut display, _rx) = headless_display_for_callback_test((1920, 1080));
            let generation = display
                .inner
                .lock()
                .await
                .output_layout
                .snapshot()
                .unwrap()
                .geometry_generation;
            let size = display
                .request_initial_size_with(
                    DesktopSize {
                        width: 1600,
                        height: 900,
                    },
                    |_, _, _, _| Ok(()),
                    move |_, _| {
                        if panic {
                            panic!("injected preparation panic");
                        }
                        anyhow::bail!("injected preparation failure")
                    },
                )
                .await;
            assert_eq!(
                size,
                DesktopSize {
                    width: 1920,
                    height: 1080
                }
            );
            let state = display.inner.lock().await;
            assert_eq!(state.pending_initial_resize, None);
            assert!(state.output_size_unconfirmed);
            assert_eq!(
                state.output_layout.snapshot().unwrap().geometry_generation,
                generation
            );
        }
    }

    #[test]
    fn managed_headless_displaycontrol_still_targets_headless_output_resize() {
        let decision = display_control_resize_decision(
            &single_primary(1600, 900),
            false,
            false,
            (1920, 1080),
            None,
        )
        .unwrap();

        assert_eq!(decision.target, ResizeTarget::ManagedHeadlessOutput);
        assert_eq!((decision.width, decision.height), (1600, 900));
    }

    #[test]
    fn managed_headless_displaycontrol_callback_resizes_headless_and_emits_resize() {
        let (mut display, mut rx) =
            headless_display_for_callback_test_with_scale((1920, 1080), 2.0);
        let mut called = None;

        display.request_layout_with(
            single_primary(1600, 900),
            |name, width, height, scale| {
                called = Some((name.to_string(), width, height, scale));
                Ok(())
            },
            refresh_headless_layout_for_test,
        );

        assert_eq!(called, Some(("HEADLESS-1".into(), 1600, 900, 2.0)));
        match rx.try_recv().expect("resize update") {
            DisplayUpdate::Resize(size) => {
                assert_eq!(
                    size,
                    DesktopSize {
                        width: 1600,
                        height: 900
                    }
                );
            }
            other => panic!("expected resize update, got {other:?}"),
        }

        let inner = display.inner.blocking_lock();
        assert_eq!(inner.resolution, (1600, 900));
    }

    #[test]
    fn managed_headless_displaycontrol_callback_failure_preserves_state_and_emits_no_resize() {
        let (mut display, mut rx) = headless_display_for_callback_test((1920, 1080));

        display.request_layout_with(
            single_primary(1600, 900),
            |_name, _width, _height, _scale| anyhow::bail!("resize failed"),
            |_layout, _output_name, _presentation| {
                panic!("layout refresh must not run after headless resize failure")
            },
        );

        assert!(rx.try_recv().is_err());
        let inner = display.inner.blocking_lock();
        assert_eq!(inner.resolution, (1920, 1080));
        assert_eq!((inner.width, inner.height), (1920, 1080));
    }

    #[test]
    fn managed_headless_displaycontrol_callback_layout_failure_preserves_rdp_state() {
        let (mut display, mut rx) = headless_display_for_callback_test((1920, 1080));
        let mut called = None;

        display.request_layout_with(
            single_primary(1600, 900),
            |name, width, height, _scale| {
                called = Some((name.to_string(), width, height));
                Ok(())
            },
            |_layout, _output_name, _presentation| anyhow::bail!("layout refresh failed"),
        );

        assert_eq!(called, Some(("HEADLESS-1".into(), 1600, 900)));
        assert!(rx.try_recv().is_err());
        let inner = display.inner.blocking_lock();
        assert_eq!(inner.resolution, (1920, 1080));
        assert_eq!((inner.width, inner.height), (1920, 1080));
    }

    #[test]
    fn managed_headless_resize_side_effects_run_only_after_headless_resize_succeeds() {
        let mut inner = headless_inner_for_resize_test((1920, 1080));
        let mut called = None;
        let decision = ResizeDecision {
            target: ResizeTarget::ManagedHeadlessOutput,
            width: 1600,
            height: 900,
        };

        let desktop_size = apply_resize_decision_with(
            &mut inner,
            decision,
            |name, width, height, _scale| {
                called = Some((name.to_string(), width, height));
                Ok(())
            },
            refresh_headless_layout_for_test,
        )
        .expect("resize applies");

        assert_eq!(called, Some(("HEADLESS-1".into(), 1600, 900)));
        assert_eq!(
            desktop_size,
            DesktopSize {
                width: 1600,
                height: 900
            }
        );
        assert_eq!(inner.resolution, (1600, 900));
        assert_eq!((inner.width, inner.height), (1600, 900));
    }

    #[test]
    fn managed_headless_resize_failure_preserves_existing_presentation_state() {
        let mut inner = headless_inner_for_resize_test((1920, 1080));
        let decision = ResizeDecision {
            target: ResizeTarget::ManagedHeadlessOutput,
            width: 1600,
            height: 900,
        };

        assert!(apply_resize_decision_with(
            &mut inner,
            decision,
            |_name, _width, _height, _scale| anyhow::bail!("resize failed"),
            |_layout, _output_name, _presentation| {
                panic!("layout refresh must not run after headless resize failure")
            }
        )
        .is_none());

        assert_eq!(inner.resolution, (1920, 1080));
        assert_eq!((inner.width, inner.height), (1920, 1080));
    }

    #[test]
    fn managed_headless_fixed_and_unchanged_resize_requests_remain_noops() {
        assert_eq!(
            initial_size_resize_decision(false, true, (1920, 1080), (1600, 900), None),
            None
        );
        assert_eq!(
            display_control_resize_decision(
                &single_primary(1600, 900),
                false,
                true,
                (1920, 1080),
                None
            ),
            None
        );
        assert_eq!(
            display_control_resize_decision(
                &single_primary(1920, 1080),
                false,
                false,
                (1920, 1080),
                None
            ),
            None
        );
    }
}
