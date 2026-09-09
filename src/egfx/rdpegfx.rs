use std::sync::atomic::Ordering;

use ironrdp_dvc::DvcMessage;
use ironrdp_egfx::pdu::{Avc420Region, Encoding};
use ironrdp_egfx::server::GraphicsPipelineServer;
use ironrdp_server::{EgfxServerMessage, GfxServerHandle, ServerEvent};
use tokio::sync::mpsc;

use super::{avc420, Avc444FrameEncoding, EgfxFrameReadiness, EgfxShared};
use super::{EncodedEgfxFrame, EncodedFrameState};

pub(in crate::egfx) struct QueuedRdpegfxFrame {
    pub(in crate::egfx) frame_id: u32,
    pub(in crate::egfx) dvc_messages: Vec<DvcMessage>,
    pub(in crate::egfx) channel_id: u32,
}

#[derive(Default)]
pub(crate) struct EgfxFrameSession {
    ready: bool,
    generation: u32,
    handle: Option<GfxServerHandle>,
    sender: Option<mpsc::UnboundedSender<ServerEvent>>,
    surface_id: Option<u16>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct EgfxFrameSessionRefresh {
    pub(crate) ready: bool,
    pub(crate) became_unready: bool,
    pub(crate) generation_changed: bool,
}

impl EgfxFrameSession {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn refresh(&mut self, shared: &EgfxShared) -> EgfxFrameSessionRefresh {
        let ready = shared.is_ready() && shared.is_avc_enabled();
        let became_unready = self.ready && !ready;
        let generation = shared.generation();
        let generation_changed = generation != self.generation;

        self.ready = ready;
        if became_unready {
            self.reset_transport();
        }

        if generation_changed {
            self.generation = generation;
            self.surface_id = None;
        }

        if ready && (self.handle.is_none() || self.sender.is_none()) {
            self.handle = shared.get_handle();
            self.sender = shared.get_event_sender();
        }

        EgfxFrameSessionRefresh {
            ready,
            became_unready,
            generation_changed,
        }
    }

    pub(crate) fn ensure_surface(&mut self, shared: &EgfxShared, width: u16, height: u16) -> bool {
        if !self.ready {
            return false;
        }

        if self.surface_id.is_some() {
            return true;
        }

        let (Some(handle), Some(sender)) = (&self.handle, &self.sender) else {
            return false;
        };

        self.surface_id = shared.init_or_reuse_surface(handle, sender, width, height);
        self.surface_id.is_some()
    }

    #[cfg(feature = "vaapi")]
    pub(crate) fn frame_readiness(&self, shared: &EgfxShared) -> EgfxFrameReadiness {
        let Some(handle) = &self.handle else {
            return EgfxFrameReadiness::TransportUnavailable;
        };

        shared.frame_readiness(handle)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn send_encoded_frame(
        &self,
        shared: &EgfxShared,
        frame: &EncodedEgfxFrame,
        damage_regions: &[(i32, i32, i32, i32)],
        timestamp_ms: u32,
        width: u16,
        height: u16,
        quality: u8,
    ) -> bool {
        let (Some(handle), Some(sender), Some(surface_id)) =
            (&self.handle, &self.sender, self.surface_id)
        else {
            return false;
        };

        shared.send_tracked_encoded_egfx_frame(
            handle,
            sender,
            surface_id,
            frame,
            damage_regions,
            timestamp_ms,
            width,
            height,
            quality,
        )
    }

    fn reset_transport(&mut self) {
        self.handle = None;
        self.sender = None;
        self.surface_id = None;
    }
}

impl EgfxShared {
    /// Encode sparse BGRA damage and enqueue one ClearCodec logical frame.
    /// None means retry without advancing codec state; Err requires session teardown.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn send_clearcodec_damage(
        &self,
        handle: &GfxServerHandle,
        sender: &mpsc::UnboundedSender<ServerEvent>,
        surface_id: u16,
        data: &[u8],
        width: u16,
        height: u16,
        stride: usize,
        damage: &[(i32, i32, i32, i32)],
        timestamp_ms: u32,
        pixel_format: ironrdp_server::PixelFormat,
    ) -> anyhow::Result<Option<usize>> {
        use super::clearcodec::ClearCodecInput;
        use ironrdp_egfx::server::MixedTilePayload;
        let input = ClearCodecInput::new(data, width, height, stride, damage, pixel_format)?;
        if input.rectangles.is_empty()
            || sender.is_closed()
            || !self.is_ready()
            || self.is_avc_enabled()
            || self.should_backpressure_frames()
        {
            return Ok(None);
        }
        let mut state = self
            .clearcodec
            .lock()
            .map_err(|_| anyhow::anyhow!("ClearCodec state poisoned"))?;
        let mut server = handle
            .lock()
            .map_err(|_| anyhow::anyhow!("EGFX server poisoned"))?;
        if !server.is_ready() || server.should_backpressure() || sender.is_closed() {
            return Ok(None);
        }
        let Some(channel_id) = server.channel_id() else {
            return Ok(None);
        };
        let Some(surface) = server.get_surface(surface_id) else {
            return Ok(None);
        };
        anyhow::ensure!(
            surface.width == width && surface.height == height,
            "ClearCodec surface size mismatch"
        );
        let encoded = state.encode(&input)?;
        let bytes = encoded.iter().map(|rect| rect.data.len()).sum();
        let tiles = encoded
            .into_iter()
            .map(|rect| MixedTilePayload::ClearCodec {
                destination: rect.destination,
                bitmap_data: rect.data,
            })
            .collect();
        let Some(frame_id) = server.send_mixed_frame(surface_id, tiles, timestamp_ms) else {
            anyhow::bail!("ClearCodec frame rejected after encode");
        };
        let queued = QueuedRdpegfxFrame {
            frame_id,
            channel_id,
            dvc_messages: server.drain_output(),
        };
        if !Self::send_rdpegfx_dvc_messages(sender, queued, "ClearCodec", "clearcodec") {
            anyhow::bail!("ClearCodec transport closed after encode");
        }
        // Keep the server lock until local accounting exists, so an ACK callback
        // cannot overtake the enqueue and be lost from the local window.
        state.commit();
        self.record_frame_queued(frame_id);
        tracing::debug!(
            frame_id,
            surface_id,
            codec = "clearcodec",
            rectangles = input.rectangles.len(),
            bytes,
            "ClearCodec frame queued"
        );
        Ok(Some(bytes))
    }

    /// Prepare EGFX state for a resize (Deactivation-Reactivation).
    /// Deletes all old surfaces, sends ResetGraphics at the new dimensions,
    /// and bumps generation so the capture thread re-creates encoder/surface.
    pub fn prepare_for_resize(&self, width: u16, height: u16) {
        let handle = match self.get_handle() {
            Some(h) => h,
            None => return,
        };
        let sender = match self.get_event_sender() {
            Some(s) => s,
            None => return,
        };

        let dvc_messages;
        let channel_id;
        {
            let Ok(mut server) = handle.lock() else {
                return;
            };
            if !server.is_ready() {
                return;
            }
            channel_id = match server.channel_id() {
                Some(id) => id,
                None => return,
            };
            server.resize(width, height);
            dvc_messages = server.drain_output();
        }
        self.clear_current_surface();

        if !dvc_messages.is_empty() {
            match ironrdp_dvc::encode_dvc_messages(
                channel_id,
                dvc_messages,
                ironrdp_svc::ChannelFlags::SHOW_PROTOCOL,
            ) {
                Ok(svc_messages) => {
                    if sender
                        .send(ServerEvent::Egfx(EgfxServerMessage::SendMessages {
                            messages: svc_messages,
                        }))
                        .is_ok()
                    {
                        self.clear_frame_queue();
                        self.ready_generation.fetch_add(1, Ordering::Release);
                    }
                }
                Err(e) => {
                    tracing::error!("Failed to encode resize PDUs: {}", e);
                }
            }
        } else {
            self.clear_frame_queue();
            self.ready_generation.fetch_add(1, Ordering::Release);
        }
    }

    /// Initialize the EGFX surface (ResetGraphics + CreateSurface + MapSurfaceToOutput).
    /// Called once when EGFX becomes ready, BEFORE any frames are sent.
    /// Returns the surface_id on success.
    pub fn init_surface(
        handle: &GfxServerHandle,
        sender: &mpsc::UnboundedSender<ServerEvent>,
        width: u16,
        height: u16,
    ) -> Option<u16> {
        if sender.is_closed() {
            return None;
        }

        let Ok(mut server) = handle.lock() else {
            return None;
        };

        if !server.is_ready() {
            return None;
        }

        let channel_id = match server.channel_id() {
            Some(id) => id,
            None => {
                tracing::warn!("EGFX: no channel_id during surface init");
                return None;
            }
        };

        // Set desktop dimensions; create_surface will auto-send ResetGraphics
        // (without monitor layout) on first call. Using resize_with_monitors
        // sends ResetGraphics WITH monitors, which causes Windows clients to
        // re-negotiate capabilities and invalidate the surface.
        server.set_output_dimensions(width, height);

        let sid = server.create_surface(width, height)?;
        server.map_surface_to_output(sid, 0, 0);

        // Drain and send all setup PDUs
        let dvc_messages = server.drain_output();
        tracing::trace!(
            count = dvc_messages.len(),
            "EGFX: draining surface setup PDUs"
        );
        drop(server); // Release lock before encoding

        if !dvc_messages.is_empty() {
            match ironrdp_dvc::encode_dvc_messages(
                channel_id,
                dvc_messages,
                ironrdp_svc::ChannelFlags::SHOW_PROTOCOL,
            ) {
                Ok(svc_messages) => {
                    if sender
                        .send(ServerEvent::Egfx(EgfxServerMessage::SendMessages {
                            messages: svc_messages,
                        }))
                        .is_err()
                    {
                        return None;
                    }
                }
                Err(e) => {
                    tracing::error!("Failed to encode surface setup PDUs: {}", e);
                    return None;
                }
            }
        }

        tracing::info!(surface_id = sid, width, height, "EGFX surface initialized");
        Some(sid)
    }

    pub fn init_or_reuse_surface(
        &self,
        handle: &GfxServerHandle,
        sender: &mpsc::UnboundedSender<ServerEvent>,
        width: u16,
        height: u16,
    ) -> Option<u16> {
        if let Some(surface_id) = self.current_surface_id(width, height) {
            if let Ok(server) = handle.lock() {
                if server.is_ready() && server.get_surface(surface_id).is_some() {
                    tracing::trace!(surface_id, width, height, "EGFX surface reused");
                    return Some(surface_id);
                }
            }
            self.clear_current_surface();
        }

        let surface_id = Self::init_surface(handle, sender, width, height)?;
        self.set_current_surface(surface_id, width, height);
        Some(surface_id)
    }

    /// Return whether an EGFX frame can be queued now.
    ///
    /// This is checked before H.264 encoding so a frame that cannot be sent does
    /// not advance the encoder reference chain.
    pub fn can_send_frame(&self, handle: &GfxServerHandle) -> bool {
        self.frame_readiness(handle).is_ready()
    }

    pub(crate) fn frame_readiness(&self, handle: &GfxServerHandle) -> EgfxFrameReadiness {
        if self.should_backpressure_frames() {
            return EgfxFrameReadiness::LocalBackpressure {
                in_flight: self.frames_in_flight(),
                max: self.max_frames_in_flight(),
                client_queue_depth: self.client_queue_depth(),
                ack_suspended: self.frame_ack_suspended(),
            };
        }

        Self::transport_frame_readiness(handle)
    }

    pub(in crate::egfx) fn transport_frame_readiness(
        handle: &GfxServerHandle,
    ) -> EgfxFrameReadiness {
        let Ok(server) = handle.lock() else {
            return EgfxFrameReadiness::TransportUnavailable;
        };
        if !server.is_ready() {
            return EgfxFrameReadiness::TransportNotReady;
        }
        if server.channel_id().is_none() {
            return EgfxFrameReadiness::TransportNoChannel;
        }
        if server.should_backpressure() {
            return EgfxFrameReadiness::TransportBackpressure {
                in_flight: server.frames_in_flight(),
                client_queue_depth: server.client_queue_depth(),
            };
        }
        EgfxFrameReadiness::Ready
    }

    pub(in crate::egfx) fn rdpegfx_event_sender_closed(
        sender: &mpsc::UnboundedSender<ServerEvent>,
        trace_name: &'static str,
    ) -> bool {
        if sender.is_closed() {
            tracing::trace!("{trace_name}: EGFX event channel already closed");
            return true;
        }
        false
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn send_tracked_encoded_egfx_frame(
        &self,
        handle: &GfxServerHandle,
        sender: &mpsc::UnboundedSender<ServerEvent>,
        surface_id: u16,
        frame: &EncodedEgfxFrame,
        damage_regions: &[(i32, i32, i32, i32)],
        timestamp_ms: u32,
        width: u16,
        height: u16,
        quality: u8,
    ) -> bool {
        if frame.state() != EncodedFrameState::Sendable {
            return false;
        }

        match frame {
            EncodedEgfxFrame::Avc420(h264_data) => {
                let regions =
                    avc420::damage_regions_to_avc420(damage_regions, width, height, quality);
                if regions.is_empty() {
                    let full_frame_region =
                        [avc420::avc420_full_frame_region(width, height, quality)];
                    self.send_tracked_avc420_rdpegfx_frame(
                        handle,
                        sender,
                        surface_id,
                        h264_data,
                        &full_frame_region,
                        timestamp_ms,
                    )
                } else {
                    self.send_tracked_avc420_rdpegfx_frame(
                        handle,
                        sender,
                        surface_id,
                        h264_data,
                        &regions,
                        timestamp_ms,
                    )
                }
            }
            EncodedEgfxFrame::Avc444(frame) => {
                let stream1_regions = avc420::damage_regions_to_avc420(
                    &frame.stream1_regions,
                    width,
                    height,
                    quality,
                );
                let stream2_regions = (!frame.stream2_regions.is_empty()).then(|| {
                    avc420::damage_regions_to_avc420(&frame.stream2_regions, width, height, quality)
                });
                self.send_tracked_avc444_rdpegfx_frame(
                    handle,
                    sender,
                    surface_id,
                    frame.encoding,
                    &frame.stream1,
                    &stream1_regions,
                    (!frame.stream2_regions.is_empty()).then_some(&frame.stream2[..]),
                    stream2_regions.as_deref(),
                    timestamp_ms,
                )
            }
        }
    }

    pub(in crate::egfx) fn send_tracked_avc420_rdpegfx_frame(
        &self,
        handle: &GfxServerHandle,
        sender: &mpsc::UnboundedSender<ServerEvent>,
        surface_id: u16,
        h264_data: &[u8],
        regions: &[Avc420Region],
        timestamp_ms: u32,
    ) -> bool {
        if !self.can_send_frame(handle) {
            return false;
        }

        if regions.is_empty() {
            tracing::trace!("queue_avc420_rdpegfx_frame: no regions");
            return false;
        }

        let Some(queued) =
            Self::queue_avc420_rdpegfx_frame(handle, surface_id, h264_data, regions, timestamp_ms)
        else {
            return false;
        };

        let frame_id = queued.frame_id;
        if Self::send_rdpegfx_dvc_messages(sender, queued, "AVC420", "avc420_rdpegfx_frame") {
            self.record_frame_queued(frame_id);
            true
        } else {
            false
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(in crate::egfx) fn send_tracked_avc444_rdpegfx_frame(
        &self,
        handle: &GfxServerHandle,
        sender: &mpsc::UnboundedSender<ServerEvent>,
        surface_id: u16,
        encoding: Avc444FrameEncoding,
        stream1_data: &[u8],
        stream1_regions: &[Avc420Region],
        stream2_data: Option<&[u8]>,
        stream2_regions: Option<&[Avc420Region]>,
        timestamp_ms: u32,
    ) -> bool {
        if Self::rdpegfx_event_sender_closed(sender, "avc444_rdpegfx_frame") {
            return false;
        }

        if !self.can_send_frame(handle) {
            return false;
        }

        let Some(send_encoding) =
            validate_avc444_send_shape(encoding, stream1_regions, stream2_data, stream2_regions)
        else {
            return false;
        };

        let Some(queued) = Self::queue_avc444_rdpegfx_frame(
            handle,
            surface_id,
            send_encoding,
            stream1_data,
            stream1_regions,
            stream2_data,
            stream2_regions,
            timestamp_ms,
        ) else {
            return false;
        };

        let frame_id = queued.frame_id;
        if Self::send_rdpegfx_dvc_messages(sender, queued, "AVC444", "avc444_rdpegfx_frame") {
            self.record_frame_queued(frame_id);
            true
        } else {
            false
        }
    }

    pub(in crate::egfx) fn queue_avc420_rdpegfx_frame(
        handle: &GfxServerHandle,
        surface_id: u16,
        h264_data: &[u8],
        regions: &[Avc420Region],
        timestamp_ms: u32,
    ) -> Option<QueuedRdpegfxFrame> {
        if regions.is_empty() {
            tracing::trace!("queue_avc420_rdpegfx_frame: no regions");
            return None;
        }

        Self::queue_rdpegfx_frame(handle, "avc420_rdpegfx_frame", |server| {
            server
                .send_avc420_frame(surface_id, h264_data, regions, timestamp_ms)
                .or_else(|| {
                    tracing::trace!("avc420_rdpegfx_frame: send returned None");
                    None
                })
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub(in crate::egfx) fn queue_avc444_rdpegfx_frame(
        handle: &GfxServerHandle,
        surface_id: u16,
        encoding: Encoding,
        stream1_data: &[u8],
        stream1_regions: &[Avc420Region],
        stream2_data: Option<&[u8]>,
        stream2_regions: Option<&[Avc420Region]>,
        timestamp_ms: u32,
    ) -> Option<QueuedRdpegfxFrame> {
        Self::queue_rdpegfx_frame(handle, "avc444_rdpegfx_frame", |server| {
            server
                .send_avc444v2_frame(
                    surface_id,
                    encoding,
                    stream1_data,
                    stream1_regions,
                    stream2_data,
                    stream2_regions,
                    timestamp_ms,
                )
                .or_else(|| {
                    tracing::trace!("avc444_rdpegfx_frame: avc444 frame returned None");
                    None
                })
        })
    }

    pub(in crate::egfx) fn queue_rdpegfx_frame(
        handle: &GfxServerHandle,
        trace_name: &'static str,
        queue_frame: impl FnOnce(&mut GraphicsPipelineServer) -> Option<u32>,
    ) -> Option<QueuedRdpegfxFrame> {
        let (frame_id, dvc_messages, channel_id) = {
            let Ok(mut server) = handle.lock() else {
                return None;
            };

            if !server.is_ready() {
                tracing::trace!("{trace_name}: server not ready");
                return None;
            }
            if server.should_backpressure() {
                tracing::trace!(
                    in_flight = server.frames_in_flight(),
                    "{trace_name}: backpressure"
                );
                return None;
            }

            let channel_id = match server.channel_id() {
                Some(id) => id,
                None => {
                    tracing::trace!("{trace_name}: no channel_id");
                    return None;
                }
            };

            let frame_id = queue_frame(&mut server)?;
            let dvc_messages = server.drain_output();
            (frame_id, dvc_messages, channel_id)
        };

        if dvc_messages.is_empty() {
            return None;
        }

        Some(QueuedRdpegfxFrame {
            frame_id,
            dvc_messages,
            channel_id,
        })
    }

    pub(in crate::egfx) fn send_rdpegfx_dvc_messages(
        sender: &mpsc::UnboundedSender<ServerEvent>,
        queued: QueuedRdpegfxFrame,
        codec_name: &'static str,
        trace_name: &'static str,
    ) -> bool {
        match ironrdp_dvc::encode_dvc_messages(
            queued.channel_id,
            queued.dvc_messages,
            ironrdp_svc::ChannelFlags::SHOW_PROTOCOL,
        ) {
            Ok(svc_messages) => {
                if sender
                    .send(ServerEvent::Egfx(EgfxServerMessage::SendMessages {
                        messages: svc_messages,
                    }))
                    .is_err()
                {
                    tracing::trace!("{trace_name}: EGFX event channel closed");
                    return false;
                }
            }
            Err(e) => {
                tracing::error!("Failed to encode EGFX {} frame: {}", codec_name, e);
                return false;
            }
        }

        true
    }
}

pub(in crate::egfx) fn validate_avc444_send_shape(
    encoding: Avc444FrameEncoding,
    stream1_regions: &[Avc420Region],
    stream2_data: Option<&[u8]>,
    stream2_regions: Option<&[Avc420Region]>,
) -> Option<Encoding> {
    let has_stream2_data = stream2_data.is_some();
    let has_stream2_regions = stream2_regions.is_some_and(|regions| !regions.is_empty());
    match encoding {
        Avc444FrameEncoding::LumaAndChroma => {
            if !has_stream2_data || !has_stream2_regions {
                tracing::trace!("validate_avc444_rdpegfx_shape: LC=0 requires stream2");
                return None;
            }
        }
        Avc444FrameEncoding::Luma | Avc444FrameEncoding::Chroma => {
            if stream2_data.is_some() || stream2_regions.is_some() {
                tracing::trace!("validate_avc444_rdpegfx_shape: LC=1/2 forbids stream2");
                return None;
            }
        }
    }

    if stream1_regions.is_empty() {
        tracing::trace!("validate_avc444_rdpegfx_shape: no regions");
        return None;
    }

    Some(match encoding {
        Avc444FrameEncoding::LumaAndChroma => Encoding::LUMA_AND_CHROMA,
        Avc444FrameEncoding::Luma => Encoding::LUMA,
        Avc444FrameEncoding::Chroma => Encoding::CHROMA,
    })
}
