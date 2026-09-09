use super::factory::{capability_avc_support, preferred_capabilities_for_policy};
use super::test_support::{
    ack_frame, assert_wire_to_surface_frame, decode_avc444_wire_to_surface, decode_gfx_output,
    drain_gfx_pdus, negotiated_avc420_session, ready_avc420_session, ready_avc444_handle,
    ready_tracked_avc420_session, tracked_avc444_session, unnegotiated_egfx_session,
    TEST_CHANNEL_ID,
};
use super::*;
use ironrdp_core::{encode_vec, Decode, Encode, ReadCursor};
use ironrdp_dvc::DvcProcessor as _;
use ironrdp_egfx::pdu::{
    Avc420BitmapStream, Avc420Region, Avc444BitmapStream, Codec1Type, Encoding, GfxPdu,
    PixelFormat, QuantQuality, QueueDepth, WireToSurface1Pdu,
};
use ironrdp_pdu::geometry::ExclusiveRectangle;
use std::sync::Arc;
use tokio::sync::mpsc;

#[test]
fn full_frame_region_uses_rdpegfx_exclusive_bounds() {
    let region = avc420_full_frame_region(1280, 720, 23);
    assert_eq!(region.left, 0);
    assert_eq!(region.top, 0);
    assert_eq!(region.right, 1280);
    assert_eq!(region.bottom, 720);
    assert_eq!(region.quantization_parameter, 23);
    assert_eq!(region.quality, 77);
}

#[test]
fn surface_init_emits_reset_create_map_in_order_without_monitor_layout() {
    let (_shared, _bridge, handle, event_tx, mut event_rx) =
        negotiated_avc420_session(1280, 720, DEFAULT_MAX_FRAMES_IN_FLIGHT);

    let surface_id = EgfxShared::init_surface(&handle, &event_tx, 1280, 720).expect("surface init");
    let pdus = drain_gfx_pdus(&mut event_rx);

    assert_eq!(pdus.len(), 3);
    match &pdus.as_slice()[0] {
        GfxPdu::ResetGraphics(reset) => {
            assert_eq!(reset.width, 1280);
            assert_eq!(reset.height, 720);
            assert!(reset.monitors.is_empty());
        }
        other => panic!("expected ResetGraphics first, got {other:?}"),
    }
    match &pdus.as_slice()[1] {
        GfxPdu::CreateSurface(create) => {
            assert_eq!(create.surface_id, surface_id);
            assert_eq!(create.width, 1280);
            assert_eq!(create.height, 720);
            assert_eq!(create.pixel_format, PixelFormat::XRgb);
        }
        other => panic!("expected CreateSurface second, got {other:?}"),
    }
    match &pdus.as_slice()[2] {
        GfxPdu::MapSurfaceToOutput(map) => {
            assert_eq!(map.surface_id, surface_id);
            assert_eq!(map.output_origin_x, 0);
            assert_eq!(map.output_origin_y, 0);
        }
        other => panic!("expected MapSurfaceToOutput third, got {other:?}"),
    }
}

#[test]
fn egfx_output_downscaling_surface_lifecycle_uses_presentation_dimensions() {
    let (_shared, _bridge, handle, event_tx, mut event_rx) =
        negotiated_avc420_session(1920, 1080, DEFAULT_MAX_FRAMES_IN_FLIGHT);

    let surface_id =
        EgfxShared::init_surface(&handle, &event_tx, 1920, 1080).expect("surface init");
    let pdus = drain_gfx_pdus(&mut event_rx);

    assert_eq!(pdus.len(), 3);
    match &pdus.as_slice()[0] {
        GfxPdu::ResetGraphics(reset) => {
            assert_eq!(reset.width, 1920);
            assert_eq!(reset.height, 1080);
            assert!(reset.monitors.is_empty());
        }
        other => panic!("expected ResetGraphics first, got {other:?}"),
    }
    match &pdus.as_slice()[1] {
        GfxPdu::CreateSurface(create) => {
            assert_eq!(create.surface_id, surface_id);
            assert_eq!(create.width, 1920);
            assert_eq!(create.height, 1080);
            assert_eq!(create.pixel_format, PixelFormat::XRgb);
        }
        other => panic!("expected CreateSurface second, got {other:?}"),
    }
    match &pdus.as_slice()[2] {
        GfxPdu::MapSurfaceToOutput(map) => {
            assert_eq!(map.surface_id, surface_id);
            assert_eq!(map.output_origin_x, 0);
            assert_eq!(map.output_origin_y, 0);
        }
        other => panic!("expected MapSurfaceToOutput third, got {other:?}"),
    }
}

#[test]
fn surface_reuse_does_not_emit_duplicate_create_or_map_for_same_size() {
    let (shared, _bridge, handle, event_tx, mut event_rx) =
        negotiated_avc420_session(1280, 720, DEFAULT_MAX_FRAMES_IN_FLIGHT);

    let first_surface_id = shared
        .init_or_reuse_surface(&handle, &event_tx, 1280, 720)
        .expect("surface init");
    let first_pdus = drain_gfx_pdus(&mut event_rx);
    assert_eq!(first_pdus.len(), 3);

    let second_surface_id = shared
        .init_or_reuse_surface(&handle, &event_tx, 1280, 720)
        .expect("surface reused");
    let second_pdus = drain_gfx_pdus(&mut event_rx);

    assert_eq!(second_surface_id, first_surface_id);
    assert!(second_pdus.is_empty());
}

#[test]
fn frame_session_owns_surface_and_encoded_send() {
    let (shared, _bridge, _handle, _event_tx, mut event_rx) =
        negotiated_avc420_session(64, 64, DEFAULT_MAX_FRAMES_IN_FLIGHT);
    let mut session = EgfxFrameSession::new();

    let refresh = session.refresh(&shared);
    assert!(refresh.ready);
    assert!(session.ensure_surface(&shared, 64, 64));

    let setup_pdus = drain_gfx_pdus(&mut event_rx);
    assert_eq!(setup_pdus.len(), 3);
    let surface_id = shared
        .current_surface_id(64, 64)
        .expect("session initialized surface");

    let encoded = EncodedEgfxFrame::avc420(vec![0; 33]);
    assert_eq!(encoded.state(), EncodedFrameState::Sendable);
    assert!(session.send_encoded_frame(&shared, &encoded, &[(0, 0, 64, 64)], 123, 64, 64, 21,));

    let frame_pdus = drain_gfx_pdus(&mut event_rx);
    let wire = assert_wire_to_surface_frame(frame_pdus.as_slice(), surface_id, Codec1Type::Avc420);
    assert_eq!(wire.destination_rectangle.left, 0);
    assert_eq!(wire.destination_rectangle.top, 0);
    assert_eq!(wire.destination_rectangle.right, 64);
    assert_eq!(wire.destination_rectangle.bottom, 64);
}

#[test]
fn frame_send_before_surface_init_is_rejected_without_queueing() {
    let (shared, _bridge, handle, event_tx, mut event_rx) =
        negotiated_avc420_session(64, 64, DEFAULT_MAX_FRAMES_IN_FLIGHT);
    let regions = [Avc420Region::new(0, 0, 64, 64, 21, 79)];

    assert!(!shared.send_tracked_avc420_rdpegfx_frame(
        &handle,
        &event_tx,
        0,
        &[0, 0, 1, 0x65, 0xaa],
        &regions,
        123,
    ));
    assert_eq!(shared.frames_in_flight(), 0);
    assert!(drain_gfx_pdus(&mut event_rx).is_empty());
}

#[test]
fn resize_deletes_old_surface_then_resets_without_monitor_layout() {
    let (shared, _bridge, handle, surface_id, _event_tx, mut event_rx) =
        ready_tracked_avc420_session(640, 480, DEFAULT_MAX_FRAMES_IN_FLIGHT);
    let _ = drain_gfx_pdus(&mut event_rx);

    shared.prepare_for_resize(800, 600);
    let pdus = drain_gfx_pdus(&mut event_rx);

    assert_eq!(pdus.len(), 2);
    match &pdus.as_slice()[0] {
        GfxPdu::DeleteSurface(delete) => assert_eq!(delete.surface_id, surface_id),
        other => panic!("expected DeleteSurface first, got {other:?}"),
    }
    match &pdus.as_slice()[1] {
        GfxPdu::ResetGraphics(reset) => {
            assert_eq!(reset.width, 800);
            assert_eq!(reset.height, 600);
            assert!(reset.monitors.is_empty());
        }
        other => panic!("expected ResetGraphics second, got {other:?}"),
    }

    let server = handle.lock().expect("server lock");
    assert!(server.get_surface(surface_id).is_none());
}

#[test]
fn surface_reinit_after_resize_emits_create_map_without_second_reset() {
    let (shared, _bridge, handle, old_surface_id, event_tx, mut event_rx) =
        ready_tracked_avc420_session(640, 480, DEFAULT_MAX_FRAMES_IN_FLIGHT);
    let _ = drain_gfx_pdus(&mut event_rx);

    shared.prepare_for_resize(800, 600);
    let _ = drain_gfx_pdus(&mut event_rx);
    let new_surface_id =
        EgfxShared::init_surface(&handle, &event_tx, 800, 600).expect("surface reinit");
    let pdus = drain_gfx_pdus(&mut event_rx);

    assert_ne!(new_surface_id, old_surface_id);
    assert_eq!(pdus.len(), 2);
    match &pdus.as_slice()[0] {
        GfxPdu::CreateSurface(create) => {
            assert_eq!(create.surface_id, new_surface_id);
            assert_eq!(create.width, 800);
            assert_eq!(create.height, 600);
        }
        other => panic!("expected CreateSurface first, got {other:?}"),
    }
    match &pdus.as_slice()[1] {
        GfxPdu::MapSurfaceToOutput(map) => assert_eq!(map.surface_id, new_surface_id),
        other => panic!("expected MapSurfaceToOutput second, got {other:?}"),
    }
}

#[test]
fn avc420_rdpegfx_queue_emits_logical_frame_wire_shape() {
    let (handle, surface_id, _event_tx, mut event_rx) = ready_avc420_session(64, 64);
    let _ = drain_gfx_pdus(&mut event_rx);

    let regions = [Avc420Region::new(4, 6, 20, 22, 19, 81)];
    let queued = EgfxShared::queue_avc420_rdpegfx_frame(
        &handle,
        surface_id,
        &[0, 0, 1, 0x65, 0xaa, 0xbb],
        &regions,
        123,
    )
    .expect("AVC420 frame queues");

    let pdus: Vec<_> = queued.dvc_messages.iter().map(decode_gfx_output).collect();
    let wire = assert_wire_to_surface_frame(pdus.as_slice(), surface_id, Codec1Type::Avc420);
    let mut bitmap_cursor = ReadCursor::new(&wire.bitmap_data);
    let bitmap = Avc420BitmapStream::decode(&mut bitmap_cursor).expect("AVC420 payload decodes");

    assert_eq!(wire.pixel_format, PixelFormat::XRgb);
    assert_eq!(
        wire.destination_rectangle,
        ExclusiveRectangle {
            left: 4,
            top: 6,
            right: 20,
            bottom: 22,
        }
    );
    assert_eq!(bitmap.rectangles[0].left, 4);
    assert_eq!(bitmap.rectangles[0].right, 20);
    assert_eq!(bitmap.quant_qual_vals[0].quantization_parameter, 19);
    assert_eq!(bitmap.quant_qual_vals[0].quality, 81);
    assert_eq!(bitmap.data, &[0, 0, 1, 0x65, 0xaa, 0xbb]);
}

#[test]
fn tracked_frame_ack_releases_local_queue_depth() {
    let (shared, mut bridge, handle, surface_id, event_tx, mut event_rx) =
        ready_tracked_avc420_session(64, 64, 1);
    let _ = drain_gfx_pdus(&mut event_rx);
    let regions = [Avc420Region::new(0, 0, 64, 64, 21, 79)];

    assert!(shared.send_tracked_avc420_rdpegfx_frame(
        &handle,
        &event_tx,
        surface_id,
        &[0, 0, 1, 0x65, 0xaa],
        &regions,
        123,
    ));
    assert_eq!(shared.frames_in_flight(), 1);
    assert_eq!(
        shared.frame_readiness(&handle),
        EgfxFrameReadiness::LocalBackpressure {
            in_flight: 1,
            max: 1,
            client_queue_depth: 0,
            ack_suspended: false,
        }
    );
    assert!(!shared.can_send_frame(&handle));

    let pdus = drain_gfx_pdus(&mut event_rx);
    let frame_id = match &pdus.as_slice()[0] {
        GfxPdu::StartFrame(start) => start.frame_id,
        other => panic!("expected StartFrame, got {other:?}"),
    };
    ack_frame(&mut bridge, frame_id, QueueDepth::AvailableBytes(8));

    assert_eq!(shared.frames_in_flight(), 0);
    assert_eq!(shared.client_queue_depth(), 8);
    assert!(shared.can_send_frame(&handle));

    assert!(shared.send_tracked_avc420_rdpegfx_frame(
        &handle,
        &event_tx,
        surface_id,
        &[0, 0, 1, 0x65, 0xbb],
        &regions,
        124,
    ));
    assert_eq!(shared.frames_in_flight(), 1);
    assert_eq!(
        shared.frame_readiness(&handle),
        EgfxFrameReadiness::LocalBackpressure {
            in_flight: 1,
            max: 1,
            client_queue_depth: 8,
            ack_suspended: false,
        }
    );
    assert!(!shared.can_send_frame(&handle));
}

#[test]
fn frame_readiness_distinguishes_transport_backpressure_from_local_queue_policy() {
    let (shared, _bridge, handle, surface_id, event_tx, mut event_rx) =
        ready_tracked_avc420_session(64, 64, DEFAULT_MAX_FRAMES_IN_FLIGHT);
    let _ = drain_gfx_pdus(&mut event_rx);
    handle
        .lock()
        .expect("server handle locks")
        .set_max_frames_in_flight(1);
    let regions = [Avc420Region::new(0, 0, 64, 64, 21, 79)];

    assert!(shared.send_tracked_avc420_rdpegfx_frame(
        &handle,
        &event_tx,
        surface_id,
        &[0, 0, 1, 0x65, 0xaa],
        &regions,
        123,
    ));

    assert_eq!(shared.frames_in_flight(), 1);
    assert_eq!(
        shared.frame_readiness(&handle),
        EgfxFrameReadiness::TransportBackpressure {
            in_flight: 1,
            client_queue_depth: 0,
        }
    );
    assert!(!shared.can_send_frame(&handle));
}

#[test]
fn frame_readiness_distinguishes_transport_not_ready_before_capabilities() {
    let session = unnegotiated_egfx_session(64, 64, EgfxCodecPolicy::Auto);

    assert_eq!(
        session.shared.frame_readiness(&session.handle),
        EgfxFrameReadiness::TransportNotReady
    );
    assert!(!session.shared.can_send_frame(&session.handle));
}

#[test]
fn default_queue_policy_backpressures_before_first_frame_ack() {
    let (shared, _bridge, handle, surface_id, event_tx, mut event_rx) =
        ready_tracked_avc420_session(64, 64, DEFAULT_MAX_FRAMES_IN_FLIGHT);
    let _ = drain_gfx_pdus(&mut event_rx);
    let regions = [Avc420Region::new(0, 0, 64, 64, 21, 79)];

    for index in 0..DEFAULT_MAX_FRAMES_IN_FLIGHT {
        assert!(
            shared.send_tracked_avc420_rdpegfx_frame(
                &handle,
                &event_tx,
                surface_id,
                &[0, 0, 1, 0x65, index as u8],
                &regions,
                123 + index,
            ),
            "frame {index} should fit the default local window"
        );
    }

    assert_eq!(shared.frames_in_flight(), DEFAULT_MAX_FRAMES_IN_FLIGHT);
    assert!(!shared.frame_ack_stream_established());
    assert_eq!(
        shared.frame_readiness(&handle),
        EgfxFrameReadiness::LocalBackpressure {
            in_flight: DEFAULT_MAX_FRAMES_IN_FLIGHT,
            max: DEFAULT_MAX_FRAMES_IN_FLIGHT,
            client_queue_depth: 0,
            ack_suspended: false,
        }
    );
    assert!(!shared.can_send_frame(&handle));
    assert!(!shared.send_tracked_avc420_rdpegfx_frame(
        &handle,
        &event_tx,
        surface_id,
        &[0, 0, 1, 0x65, 0xff],
        &regions,
        999,
    ));
}

#[test]
fn tracked_queue_policy_backpressures_after_ack_stream_stalls() {
    let (shared, mut bridge, handle, surface_id, event_tx, mut event_rx) =
        ready_tracked_avc420_session(64, 64, DEFAULT_MAX_FRAMES_IN_FLIGHT);
    let _ = drain_gfx_pdus(&mut event_rx);
    let regions = [Avc420Region::new(0, 0, 64, 64, 21, 79)];

    assert!(shared.send_tracked_avc420_rdpegfx_frame(
        &handle,
        &event_tx,
        surface_id,
        &[0, 0, 1, 0x65, 0xaa],
        &regions,
        123,
    ));
    let pdus = drain_gfx_pdus(&mut event_rx);
    let first_frame_id = match &pdus.as_slice()[0] {
        GfxPdu::StartFrame(start) => start.frame_id,
        other => panic!("expected StartFrame, got {other:?}"),
    };
    ack_frame(
        &mut bridge,
        first_frame_id,
        QueueDepth::AvailableBytes(661_655),
    );

    assert_eq!(shared.frames_in_flight(), 0);
    assert_eq!(
        shared.frame_flow_snapshot().last_acked_frame_id,
        first_frame_id
    );
    assert!(shared.frame_ack_stream_established());

    for index in 0..DEFAULT_MAX_FRAMES_IN_FLIGHT {
        assert!(
            shared.send_tracked_avc420_rdpegfx_frame(
                &handle,
                &event_tx,
                surface_id,
                &[0, 0, 1, 0x65, index as u8],
                &regions,
                124 + index,
            ),
            "frame {index} should fit the ACK-established local window"
        );
    }

    assert_eq!(shared.frames_in_flight(), DEFAULT_MAX_FRAMES_IN_FLIGHT);
    assert_eq!(
        shared.frame_readiness(&handle),
        EgfxFrameReadiness::LocalBackpressure {
            in_flight: DEFAULT_MAX_FRAMES_IN_FLIGHT,
            max: DEFAULT_MAX_FRAMES_IN_FLIGHT,
            client_queue_depth: 661_655,
            ack_suspended: false,
        }
    );
    assert!(!shared.can_send_frame(&handle));
    assert!(!shared.send_tracked_avc420_rdpegfx_frame(
        &handle,
        &event_tx,
        surface_id,
        &[0, 0, 1, 0x65, 0xff],
        &regions,
        999,
    ));
}

#[test]
fn preferred_frame_rate_drops_as_ack_window_fills() {
    let (shared, mut bridge, handle, surface_id, event_tx, mut event_rx) =
        ready_tracked_avc420_session(64, 64, DEFAULT_MAX_FRAMES_IN_FLIGHT);
    let _ = drain_gfx_pdus(&mut event_rx);
    let regions = [Avc420Region::new(0, 0, 64, 64, 21, 79)];

    assert_eq!(shared.preferred_frame_rate(30), 30);

    assert!(shared.send_tracked_avc420_rdpegfx_frame(
        &handle,
        &event_tx,
        surface_id,
        &[0, 0, 1, 0x65, 0xaa],
        &regions,
        123,
    ));
    assert_eq!(shared.frames_in_flight(), 1);
    assert_eq!(shared.preferred_frame_rate(30), 30);

    assert!(shared.send_tracked_avc420_rdpegfx_frame(
        &handle,
        &event_tx,
        surface_id,
        &[0, 0, 1, 0x65, 0xbb],
        &regions,
        124,
    ));
    assert_eq!(shared.frames_in_flight(), 2);
    assert_eq!(shared.preferred_frame_rate(30), 9);

    assert!(shared.send_tracked_avc420_rdpegfx_frame(
        &handle,
        &event_tx,
        surface_id,
        &[0, 0, 1, 0x65, 0xcc],
        &regions,
        125,
    ));
    assert_eq!(shared.frames_in_flight(), 3);
    assert_eq!(shared.preferred_frame_rate(30), 7);

    let pdus = drain_gfx_pdus(&mut event_rx);
    let first_frame_id = match &pdus.as_slice()[0] {
        GfxPdu::StartFrame(start) => start.frame_id,
        other => panic!("expected StartFrame, got {other:?}"),
    };
    ack_frame(&mut bridge, first_frame_id, QueueDepth::Suspend);

    assert!(shared.frame_ack_suspended());
    assert_eq!(shared.preferred_frame_rate(30), 30);
}

#[test]
fn tracked_queue_policy_honors_client_ack_suspend() {
    let (shared, mut bridge, handle, surface_id, event_tx, mut event_rx) =
        ready_tracked_avc420_session(64, 64, 1);
    let _ = drain_gfx_pdus(&mut event_rx);
    let regions = [Avc420Region::new(0, 0, 64, 64, 21, 79)];

    assert!(shared.send_tracked_avc420_rdpegfx_frame(
        &handle,
        &event_tx,
        surface_id,
        &[0, 0, 1, 0x65, 0xaa],
        &regions,
        123,
    ));
    let pdus = drain_gfx_pdus(&mut event_rx);
    let frame_id = match &pdus.as_slice()[0] {
        GfxPdu::StartFrame(start) => start.frame_id,
        other => panic!("expected StartFrame, got {other:?}"),
    };
    ack_frame(&mut bridge, frame_id, QueueDepth::Suspend);

    assert!(shared.frame_ack_suspended());
    assert_eq!(shared.frames_in_flight(), 0);
    assert!(shared.can_send_frame(&handle));

    assert!(shared.send_tracked_avc420_rdpegfx_frame(
        &handle,
        &event_tx,
        surface_id,
        &[0, 0, 1, 0x65, 0xbb],
        &regions,
        124,
    ));
    assert!(shared.send_tracked_avc420_rdpegfx_frame(
        &handle,
        &event_tx,
        surface_id,
        &[0, 0, 1, 0x65, 0xcc],
        &regions,
        125,
    ));
    assert_eq!(shared.frames_in_flight(), 2);
    assert!(shared.can_send_frame(&handle));
}

#[test]
fn resize_generation_ignores_stale_frame_ack() {
    let (shared, mut bridge, handle, surface_id, event_tx, mut event_rx) =
        ready_tracked_avc420_session(64, 64, 2);
    let _ = drain_gfx_pdus(&mut event_rx);
    let regions = [Avc420Region::new(0, 0, 64, 64, 21, 79)];

    assert!(shared.send_tracked_avc420_rdpegfx_frame(
        &handle,
        &event_tx,
        surface_id,
        &[0, 0, 1, 0x65, 0xaa],
        &regions,
        123,
    ));
    let first_pdus = drain_gfx_pdus(&mut event_rx);
    let stale_frame_id = match &first_pdus.as_slice()[0] {
        GfxPdu::StartFrame(start) => start.frame_id,
        other => panic!("expected StartFrame, got {other:?}"),
    };
    assert_eq!(shared.frames_in_flight(), 1);

    shared.prepare_for_resize(64, 64);
    let _ = drain_gfx_pdus(&mut event_rx);
    assert_eq!(shared.frames_in_flight(), 0);
    let new_surface_id =
        EgfxShared::init_surface(&handle, &event_tx, 64, 64).expect("surface reinit");
    let _ = drain_gfx_pdus(&mut event_rx);

    assert!(shared.send_tracked_avc420_rdpegfx_frame(
        &handle,
        &event_tx,
        new_surface_id,
        &[0, 0, 1, 0x65, 0xbb],
        &regions,
        124,
    ));
    let second_pdus = drain_gfx_pdus(&mut event_rx);
    let current_frame_id = match &second_pdus.as_slice()[0] {
        GfxPdu::StartFrame(start) => start.frame_id,
        other => panic!("expected StartFrame, got {other:?}"),
    };
    assert_eq!(shared.frames_in_flight(), 1);

    ack_frame(&mut bridge, stale_frame_id, QueueDepth::AvailableBytes(4));
    assert_eq!(shared.frames_in_flight(), 1);

    ack_frame(&mut bridge, current_frame_id, QueueDepth::AvailableBytes(5));
    assert_eq!(shared.frames_in_flight(), 0);
    assert_eq!(shared.client_queue_depth(), 5);
}

#[test]
fn avc444v2_rdpegfx_queue_emits_logical_frame_wire_shape() {
    let (handle, surface_id) = ready_avc444_handle(64, 64);
    let stream1_regions = [Avc420Region::new(0, 0, 32, 32, 21, 79)];
    let stream2_regions = [Avc420Region::new(16, 8, 64, 48, 22, 78)];

    let queued = EgfxShared::queue_avc444_rdpegfx_frame(
        &handle,
        surface_id,
        Encoding::LUMA_AND_CHROMA,
        &[1, 2, 3],
        &stream1_regions,
        Some(&[4, 5]),
        Some(&stream2_regions),
        123,
    )
    .expect("LC=0 AVC444v2 frame queues");
    let pdus: Vec<_> = queued.dvc_messages.iter().map(decode_gfx_output).collect();
    let wire = assert_wire_to_surface_frame(pdus.as_slice(), surface_id, Codec1Type::Avc444v2);
    let mut bitmap_cursor = ReadCursor::new(&wire.bitmap_data);
    let bitmap = Avc444BitmapStream::decode(&mut bitmap_cursor).expect("AVC444 payload decodes");

    assert_eq!(bitmap.encoding, Encoding::LUMA_AND_CHROMA);
    assert!(!bitmap.stream1.data.is_empty());
    assert_eq!(bitmap.stream1.rectangles[0].left, 0);
    assert_eq!(bitmap.stream1.rectangles[0].right, 32);
    let stream2 = bitmap.stream2.expect("LC=0 carries stream2");
    assert!(!stream2.data.is_empty());
    assert_eq!(stream2.rectangles[0].left, 16);
    assert_eq!(stream2.rectangles[0].right, 64);
}

#[test]
fn avc444_stream_info_encodes_lc_and_stream1_size() {
    let rectangle = ExclusiveRectangle {
        left: 0,
        top: 0,
        right: 15,
        bottom: 15,
    };
    let quant = QuantQuality {
        quantization_parameter: 20,
        progressive: false,
        quality: 80,
    };
    let stream1 = Avc420BitmapStream {
        rectangles: vec![rectangle.clone()],
        quant_qual_vals: vec![quant.clone()],
        data: &[1, 2, 3, 4],
    };
    let stream2 = Avc420BitmapStream {
        rectangles: vec![rectangle],
        quant_qual_vals: vec![quant],
        data: &[5, 6, 7],
    };
    let stream1_size = stream1.size();
    let avc444 = Avc444BitmapStream {
        encoding: Encoding::LUMA_AND_CHROMA,
        stream1,
        stream2: Some(stream2),
    };

    let encoded = encode_vec(&avc444).expect("AVC444 stream encodes");
    let stream_info = u32::from_le_bytes(encoded[0..4].try_into().unwrap());

    assert_eq!(stream_info & 0x3fff_ffff, stream1_size as u32);
    assert_eq!(
        stream_info >> 30,
        u32::from(Encoding::LUMA_AND_CHROMA.bits())
    );
}

#[test]
fn avc444_luma_only_decodes_stream1_without_stream2() {
    let rectangle = ExclusiveRectangle {
        left: 2,
        top: 4,
        right: 18,
        bottom: 20,
    };
    let quant = QuantQuality {
        quantization_parameter: 18,
        progressive: false,
        quality: 82,
    };
    let avc444 = Avc444BitmapStream {
        encoding: Encoding::LUMA,
        stream1: Avc420BitmapStream {
            rectangles: vec![rectangle.clone()],
            quant_qual_vals: vec![quant.clone()],
            data: &[1, 3, 5, 7],
        },
        stream2: None,
    };

    let encoded = encode_vec(&avc444).expect("AVC444 stream encodes");
    let mut cursor = ReadCursor::new(&encoded);
    let decoded = Avc444BitmapStream::decode(&mut cursor).expect("AVC444 stream decodes");

    assert_eq!(decoded.encoding, Encoding::LUMA);
    assert_eq!(decoded.stream1.rectangles, vec![rectangle]);
    assert_eq!(decoded.stream1.quant_qual_vals, vec![quant]);
    assert_eq!(decoded.stream1.data, &[1, 3, 5, 7]);
    assert!(decoded.stream2.is_none());
}

#[test]
fn avc444_chroma_only_decodes_stream1_without_stream2() {
    let rectangle = ExclusiveRectangle {
        left: 4,
        top: 2,
        right: 11,
        bottom: 7,
    };
    let quant = QuantQuality {
        quantization_parameter: 23,
        progressive: false,
        quality: 77,
    };
    let avc444 = Avc444BitmapStream {
        encoding: Encoding::CHROMA,
        stream1: Avc420BitmapStream {
            rectangles: vec![rectangle.clone()],
            quant_qual_vals: vec![quant.clone()],
            data: &[9, 8, 7, 6],
        },
        stream2: None,
    };

    let encoded = encode_vec(&avc444).expect("AVC444 stream encodes");
    let mut cursor = ReadCursor::new(&encoded);
    let decoded = Avc444BitmapStream::decode(&mut cursor).expect("AVC444 stream decodes");

    assert_eq!(decoded.encoding, Encoding::CHROMA);
    assert_eq!(decoded.stream1.rectangles, vec![rectangle]);
    assert_eq!(decoded.stream1.quant_qual_vals, vec![quant]);
    assert_eq!(decoded.stream1.data, &[9, 8, 7, 6]);
    assert!(decoded.stream2.is_none());
}

#[test]
fn wire_to_surface1_roundtrips_avc444v2_bitmap_payload() {
    let rectangle = ExclusiveRectangle {
        left: 0,
        top: 0,
        right: 32,
        bottom: 24,
    };
    let quant = QuantQuality {
        quantization_parameter: 21,
        progressive: false,
        quality: 79,
    };
    let avc444 = Avc444BitmapStream {
        encoding: Encoding::LUMA_AND_CHROMA,
        stream1: Avc420BitmapStream {
            rectangles: vec![rectangle.clone()],
            quant_qual_vals: vec![quant.clone()],
            data: &[0xaa, 0xbb, 0xcc],
        },
        stream2: Some(Avc420BitmapStream {
            rectangles: vec![rectangle.clone()],
            quant_qual_vals: vec![quant.clone()],
            data: &[0x11, 0x22],
        }),
    };
    let destination_rectangle = ExclusiveRectangle {
        left: rectangle.left,
        top: rectangle.top,
        right: rectangle.right,
        bottom: rectangle.bottom,
    };
    let pdu = WireToSurface1Pdu {
        surface_id: 7,
        codec_id: Codec1Type::Avc444v2,
        pixel_format: PixelFormat::ARgb,
        destination_rectangle: destination_rectangle.clone(),
        bitmap_data: encode_vec(&avc444).expect("AVC444 stream encodes"),
    };

    let encoded = encode_vec(&pdu).expect("WireToSurface1 encodes");
    let mut cursor = ReadCursor::new(&encoded);
    let decoded = WireToSurface1Pdu::decode(&mut cursor).expect("WireToSurface1 decodes");
    let mut bitmap_cursor = ReadCursor::new(&decoded.bitmap_data);
    let bitmap = Avc444BitmapStream::decode(&mut bitmap_cursor).expect("AVC444 payload decodes");

    assert_eq!(decoded.surface_id, 7);
    assert_eq!(decoded.codec_id, Codec1Type::Avc444v2);
    assert_eq!(decoded.pixel_format, PixelFormat::ARgb);
    assert_eq!(decoded.destination_rectangle, destination_rectangle);
    assert_eq!(bitmap.encoding, Encoding::LUMA_AND_CHROMA);
    assert_eq!(bitmap.stream1.rectangles, vec![rectangle.clone()]);
    assert_eq!(bitmap.stream1.quant_qual_vals, vec![quant.clone()]);
    assert_eq!(bitmap.stream1.data, &[0xaa, 0xbb, 0xcc]);
    let stream2 = bitmap.stream2.expect("LC=0 carries stream2");
    assert_eq!(stream2.rectangles, vec![rectangle]);
    assert_eq!(stream2.quant_qual_vals, vec![quant]);
    assert_eq!(stream2.data, &[0x11, 0x22]);
}

#[test]
fn avc444_rdpegfx_queue_maps_luma_and_chroma_to_wire_payload() {
    let (handle, surface_id) = ready_avc444_handle(64, 64);
    let stream1_regions = [Avc420Region::new(0, 0, 32, 32, 21, 79)];
    let stream2_regions = [Avc420Region::new(16, 8, 64, 48, 22, 78)];

    let queued = EgfxShared::queue_avc444_rdpegfx_frame(
        &handle,
        surface_id,
        Encoding::LUMA_AND_CHROMA,
        &[1, 2, 3],
        &stream1_regions,
        Some(&[4, 5]),
        Some(&stream2_regions),
        123,
    )
    .expect("LC=0 AVC444v2 frame queues");

    assert_eq!(queued.dvc_messages.len(), 3);
    assert!(matches!(
        decode_gfx_output(&queued.dvc_messages[0]),
        GfxPdu::StartFrame(_)
    ));
    assert!(matches!(
        decode_gfx_output(&queued.dvc_messages[2]),
        GfxPdu::EndFrame(_)
    ));
    let wire = decode_avc444_wire_to_surface(&queued.dvc_messages[1]);
    let mut bitmap_cursor = ReadCursor::new(&wire.bitmap_data);
    let bitmap = Avc444BitmapStream::decode(&mut bitmap_cursor).expect("AVC444 payload decodes");
    let stream_info = u32::from_le_bytes(wire.bitmap_data[0..4].try_into().unwrap());

    assert_eq!(wire.surface_id, surface_id);
    assert_eq!(wire.codec_id, Codec1Type::Avc444v2);
    assert_eq!(
        stream_info >> 30,
        u32::from(Encoding::LUMA_AND_CHROMA.bits())
    );
    assert_eq!(bitmap.encoding, Encoding::LUMA_AND_CHROMA);
    assert_eq!(bitmap.stream1.data, &[1, 2, 3]);
    assert_eq!(bitmap.stream1.rectangles[0].left, 0);
    assert_eq!(bitmap.stream1.rectangles[0].right, 32);
    let stream2 = bitmap.stream2.expect("LC=0 has stream2");
    assert_eq!(stream2.data, &[4, 5]);
    assert_eq!(stream2.rectangles[0].left, 16);
    assert_eq!(stream2.rectangles[0].right, 64);
    assert_eq!(wire.destination_rectangle.left, 0);
    assert_eq!(wire.destination_rectangle.top, 0);
    assert_eq!(wire.destination_rectangle.right, 64);
    assert_eq!(wire.destination_rectangle.bottom, 48);
}

#[test]
fn avc444_rdpegfx_queue_allows_empty_h264_payloads_with_regions() {
    let (handle, surface_id) = ready_avc444_handle(64, 64);
    let stream1_regions = [Avc420Region::new(0, 0, 32, 32, 21, 79)];
    let stream2_regions = [Avc420Region::new(16, 8, 64, 48, 22, 78)];

    let queued = EgfxShared::queue_avc444_rdpegfx_frame(
        &handle,
        surface_id,
        Encoding::LUMA_AND_CHROMA,
        &[],
        &stream1_regions,
        Some(&[]),
        Some(&stream2_regions),
        123,
    )
    .expect("LC=0 AVC444v2 frame with metadata queues");

    let wire = decode_avc444_wire_to_surface(&queued.dvc_messages[1]);
    let mut bitmap_cursor = ReadCursor::new(&wire.bitmap_data);
    let bitmap = Avc444BitmapStream::decode(&mut bitmap_cursor).expect("AVC444 payload decodes");

    assert_eq!(bitmap.encoding, Encoding::LUMA_AND_CHROMA);
    assert!(bitmap.stream1.data.is_empty());
    assert_eq!(bitmap.stream1.rectangles[0].left, 0);
    assert_eq!(bitmap.stream1.rectangles[0].right, 32);
    let stream2 = bitmap.stream2.expect("LC=0 has stream2");
    assert!(stream2.data.is_empty());
    assert_eq!(stream2.rectangles[0].left, 16);
    assert_eq!(stream2.rectangles[0].right, 64);
}

#[test]
fn avc444_rdpegfx_queue_maps_luma_only_and_chroma_only_to_stream1() {
    for (wire_encoding, payload) in [
        (Encoding::LUMA, &[0x10, 0x11][..]),
        (Encoding::CHROMA, &[0x20, 0x21, 0x22][..]),
    ] {
        let (handle, surface_id) = ready_avc444_handle(64, 64);
        let regions = [Avc420Region::new(4, 6, 20, 22, 18, 82)];
        let queued = EgfxShared::queue_avc444_rdpegfx_frame(
            &handle,
            surface_id,
            wire_encoding,
            payload,
            &regions,
            None,
            None,
            123,
        )
        .expect("single-stream AVC444v2 frame queues");

        assert_eq!(queued.dvc_messages.len(), 3);
        let wire = decode_avc444_wire_to_surface(&queued.dvc_messages[1]);
        let mut bitmap_cursor = ReadCursor::new(&wire.bitmap_data);
        let bitmap =
            Avc444BitmapStream::decode(&mut bitmap_cursor).expect("AVC444 payload decodes");
        let stream_info = u32::from_le_bytes(wire.bitmap_data[0..4].try_into().unwrap());

        assert_eq!(wire.surface_id, surface_id);
        assert_eq!(wire.codec_id, Codec1Type::Avc444v2);
        assert_eq!(stream_info >> 30, u32::from(wire_encoding.bits()));
        assert_eq!(bitmap.encoding, wire_encoding);
        assert_eq!(bitmap.stream1.data, payload);
        assert_eq!(bitmap.stream1.rectangles[0].left, 4);
        assert_eq!(bitmap.stream1.rectangles[0].right, 20);
        assert!(bitmap.stream2.is_none());
    }
}

#[test]
fn avc444_rdpegfx_shape_rejects_stream2_for_single_stream_lc() {
    let stream1_regions = [Avc420Region::new(0, 0, 32, 32, 21, 79)];
    let stream2_regions = [Avc420Region::new(16, 8, 64, 48, 22, 78)];

    for encoding in [Avc444FrameEncoding::Luma, Avc444FrameEncoding::Chroma] {
        assert!(super::rdpegfx::validate_avc444_send_shape(
            encoding,
            &stream1_regions,
            Some(&[4, 5]),
            Some(&stream2_regions),
        )
        .is_none());
    }
}

#[test]
fn avc444_rdpegfx_shape_rejects_lc0_without_stream2() {
    let stream1_regions = [Avc420Region::new(0, 0, 32, 32, 21, 79)];

    assert!(super::rdpegfx::validate_avc444_send_shape(
        Avc444FrameEncoding::LumaAndChroma,
        &stream1_regions,
        None,
        None,
    )
    .is_none());
}

#[test]
fn avc444_send_with_closed_event_channel_does_not_queue_frame() {
    let session = tracked_avc444_session(64, 64, DEFAULT_MAX_FRAMES_IN_FLIGHT);
    let surface_id =
        EgfxShared::init_surface(&session.handle, &session.event_tx, 64, 64).expect("surface init");
    drop(session.event_rx);
    let regions = [Avc420Region::new(0, 0, 32, 32, 21, 79)];

    assert!(!session.shared.send_tracked_avc444_rdpegfx_frame(
        &session.handle,
        &session.event_tx,
        surface_id,
        Avc444FrameEncoding::Luma,
        &[1, 2, 3],
        &regions,
        None,
        None,
        123,
    ));
    let server = session.handle.lock().expect("server lock");
    assert_eq!(server.frames_in_flight(), 0);
}

#[test]
fn resize_does_not_bump_generation_when_reset_cannot_be_sent() {
    let session = tracked_avc444_session(64, 64, DEFAULT_MAX_FRAMES_IN_FLIGHT);
    let _ =
        EgfxShared::init_surface(&session.handle, &session.event_tx, 64, 64).expect("surface init");
    let generation = session.shared.generation();

    drop(session.event_rx);
    session.shared.prepare_for_resize(64, 64);

    assert_eq!(session.shared.generation(), generation);
}

#[test]
fn reset_for_new_client_clears_negotiated_state_and_frame_queue() {
    let (shared, _bridge, _handle, _surface_id, _event_tx, _event_rx) =
        ready_tracked_avc420_session(64, 64, DEFAULT_MAX_FRAMES_IN_FLIGHT);
    shared
        .avc444_enabled
        .store(true, std::sync::atomic::Ordering::Release);
    shared.record_frame_queued(42);

    assert!(shared.is_ready());
    assert!(shared.is_avc_enabled());
    assert!(shared.is_avc444_enabled());
    assert_eq!(shared.frames_in_flight(), 1);

    shared.reset_for_new_client();

    assert!(!shared.is_ready());
    assert!(!shared.is_avc_enabled());
    assert!(!shared.is_avc444_enabled());
    assert_eq!(shared.frames_in_flight(), 0);
    assert_eq!(shared.client_queue_depth(), 0);
}

#[test]
fn building_new_gfx_server_resets_stale_negotiated_state() {
    let (shared, _bridge, _handle, _surface_id, _event_tx, _event_rx) =
        ready_tracked_avc420_session(64, 64, DEFAULT_MAX_FRAMES_IN_FLIGHT);
    shared
        .avc444_enabled
        .store(true, std::sync::atomic::Ordering::Release);
    shared.record_frame_queued(42);

    let (event_tx, _event_rx) = mpsc::unbounded_channel();
    let mut factory = HyprGfxFactory::new(Arc::clone(&shared));
    ironrdp_server::ServerEventSender::set_sender(&mut factory, event_tx);
    let (_bridge, _handle) = ironrdp_server::GfxServerFactory::build_server_with_handle(&factory)
        .expect("EGFX server builds");

    assert!(!shared.is_ready());
    assert!(!shared.is_avc_enabled());
    assert!(!shared.is_avc444_enabled());
    assert_eq!(shared.frames_in_flight(), 0);
}

#[test]
fn new_gfx_server_requires_fresh_capabilities_and_surface_setup() {
    let (shared, _old_bridge, _old_handle, _old_surface_id, event_tx, mut event_rx) =
        ready_tracked_avc420_session(64, 64, DEFAULT_MAX_FRAMES_IN_FLIGHT);
    let _ = drain_gfx_pdus(&mut event_rx);

    let mut factory = HyprGfxFactory::new(Arc::clone(&shared));
    ironrdp_server::ServerEventSender::set_sender(&mut factory, event_tx.clone());
    let (mut new_bridge, new_handle) =
        ironrdp_server::GfxServerFactory::build_server_with_handle(&factory)
            .expect("new EGFX server builds");

    assert!(!shared.is_ready());
    assert!(!shared.is_avc_enabled());
    assert_eq!(shared.frames_in_flight(), 0);
    assert!(
        shared
            .init_or_reuse_surface(&new_handle, &event_tx, 64, 64)
            .is_none(),
        "new connection must not reuse the previous surface before capabilities"
    );
    assert!(drain_gfx_pdus(&mut event_rx).is_empty());

    new_bridge.start(TEST_CHANNEL_ID).expect("channel starts");
    let caps =
        GfxPdu::CapabilitiesAdvertise(ironrdp_egfx::pdu::CapabilitiesAdvertisePdu::from_typed(&[
            ironrdp_egfx::pdu::CapabilitySet::V8_1 {
                flags: ironrdp_egfx::pdu::CapabilitiesV81Flags::AVC420_ENABLED,
            },
        ]));
    let caps = encode_vec(&caps).expect("capabilities encode");
    let _ = new_bridge
        .process(TEST_CHANNEL_ID, &caps)
        .expect("capabilities process");
    assert!(shared.is_ready());

    let new_surface_id = shared
        .init_or_reuse_surface(&new_handle, &event_tx, 64, 64)
        .expect("surface init after fresh capabilities");
    let pdus = drain_gfx_pdus(&mut event_rx);

    assert_eq!(pdus.len(), 3);
    assert!(matches!(
        &pdus.as_slice()[0],
        GfxPdu::ResetGraphics(reset) if reset.width == 64 && reset.height == 64
    ));
    assert!(matches!(
        &pdus.as_slice()[1],
        GfxPdu::CreateSurface(create)
            if create.surface_id == new_surface_id
                && create.width == 64
                && create.height == 64
    ));
    assert!(matches!(
        &pdus.as_slice()[2],
        GfxPdu::MapSurfaceToOutput(map) if map.surface_id == new_surface_id
    ));
    assert_eq!(shared.current_surface_id(64, 64), Some(new_surface_id));
}

#[test]
fn repeated_compatible_capabilities_reinitialize_surface_generation() {
    let mut session = unnegotiated_egfx_session(64, 64, EgfxCodecPolicy::Auto);
    session
        .bridge
        .start(TEST_CHANNEL_ID)
        .expect("channel starts");

    let caps =
        GfxPdu::CapabilitiesAdvertise(ironrdp_egfx::pdu::CapabilitiesAdvertisePdu::from_typed(&[
            ironrdp_egfx::pdu::CapabilitySet::V10_7 {
                flags: ironrdp_egfx::pdu::CapabilitiesV107Flags::empty(),
            },
        ]));
    let caps = encode_vec(&caps).expect("capabilities encode");

    let _ = session
        .bridge
        .process(TEST_CHANNEL_ID, &caps)
        .expect("initial capabilities process");
    let first_generation = session.shared.generation();
    assert!(!session.shared.full_frame_requested());
    let first_surface_id = session
        .shared
        .init_or_reuse_surface(&session.handle, &session.event_tx, 64, 64)
        .expect("initial surface setup");
    let initial_pdus = drain_gfx_pdus(&mut session.event_rx);
    assert_eq!(initial_pdus.len(), 3);

    let _ = session
        .bridge
        .process(TEST_CHANNEL_ID, &caps)
        .expect("repeated capabilities process");

    assert!(session.shared.generation() > first_generation);
    assert!(session.shared.full_frame_requested());

    let reset_surface_id = session
        .shared
        .init_or_reuse_surface(&session.handle, &session.event_tx, 64, 64)
        .expect("surface setup after protocol reset");
    let reset_pdus = drain_gfx_pdus(&mut session.event_rx);

    assert_eq!(reset_surface_id, first_surface_id);
    assert_eq!(reset_pdus.len(), 3);
    assert!(matches!(
        &reset_pdus.as_slice()[0],
        GfxPdu::ResetGraphics(reset) if reset.width == 64 && reset.height == 64
    ));
    assert!(matches!(
        &reset_pdus.as_slice()[1],
        GfxPdu::CreateSurface(create) if create.surface_id == reset_surface_id
    ));
    assert!(matches!(
        &reset_pdus.as_slice()[2],
        GfxPdu::MapSurfaceToOutput(map) if map.surface_id == reset_surface_id
    ));
}

#[test]
fn changed_avc_capabilities_bump_generation_for_surface_reinit() {
    let mut session = unnegotiated_egfx_session(64, 64, EgfxCodecPolicy::Auto);
    session
        .bridge
        .start(TEST_CHANNEL_ID)
        .expect("channel starts");

    let avc_caps =
        GfxPdu::CapabilitiesAdvertise(ironrdp_egfx::pdu::CapabilitiesAdvertisePdu::from_typed(&[
            ironrdp_egfx::pdu::CapabilitySet::V10_7 {
                flags: ironrdp_egfx::pdu::CapabilitiesV107Flags::empty(),
            },
        ]));
    let avc_caps = encode_vec(&avc_caps).expect("AVC capabilities encode");
    let no_avc_caps =
        GfxPdu::CapabilitiesAdvertise(ironrdp_egfx::pdu::CapabilitiesAdvertisePdu::from_typed(&[
            ironrdp_egfx::pdu::CapabilitySet::V8 {
                flags: ironrdp_egfx::pdu::CapabilitiesV8Flags::empty(),
            },
        ]));
    let no_avc_caps = encode_vec(&no_avc_caps).expect("non-AVC capabilities encode");

    let _ = session
        .bridge
        .process(TEST_CHANNEL_ID, &avc_caps)
        .expect("initial AVC capabilities process");
    let first_generation = session.shared.generation();

    let _ = session
        .bridge
        .process(TEST_CHANNEL_ID, &no_avc_caps)
        .expect("changed capabilities process");

    assert!(session.shared.generation() > first_generation);
}

#[test]
fn auto_policy_uses_v10_avc444_when_v81_lacks_avc420_flag() {
    let mut session = unnegotiated_egfx_session(64, 64, EgfxCodecPolicy::Auto);
    session
        .bridge
        .start(TEST_CHANNEL_ID)
        .expect("channel starts");

    let caps =
        GfxPdu::CapabilitiesAdvertise(ironrdp_egfx::pdu::CapabilitiesAdvertisePdu::from_typed(&[
            ironrdp_egfx::pdu::CapabilitySet::V8_1 {
                flags: ironrdp_egfx::pdu::CapabilitiesV81Flags::empty(),
            },
            ironrdp_egfx::pdu::CapabilitySet::V10 {
                flags: ironrdp_egfx::pdu::CapabilitiesV10Flags::empty(),
            },
        ]));
    let caps = encode_vec(&caps).expect("capabilities encode");

    let _ = session
        .bridge
        .process(TEST_CHANNEL_ID, &caps)
        .expect("capabilities process");

    assert!(session.shared.is_avc_enabled());
    assert!(session.shared.is_avc444_enabled());
}

#[test]
fn capability_support_respects_avc_disabled_flags_and_avc444_env_switch() {
    use ironrdp_egfx::pdu::*;

    assert_eq!(
        capability_avc_support(
            &CapabilitySet::V8_1 {
                flags: CapabilitiesV81Flags::AVC420_ENABLED,
            },
            false,
            EgfxCodecPolicy::Auto,
        ),
        (true, false)
    );
    assert_eq!(
        capability_avc_support(
            &CapabilitySet::V10_7 {
                flags: CapabilitiesV107Flags::empty(),
            },
            true,
            EgfxCodecPolicy::Auto,
        ),
        (true, false)
    );
    assert_eq!(
        capability_avc_support(
            &CapabilitySet::V10_7 {
                flags: CapabilitiesV107Flags::empty(),
            },
            false,
            EgfxCodecPolicy::Avc420,
        ),
        (true, false)
    );
    assert_eq!(
        capability_avc_support(
            &CapabilitySet::V8_1 {
                flags: CapabilitiesV81Flags::empty(),
            },
            false,
            EgfxCodecPolicy::Auto,
        ),
        (false, false)
    );
    assert_eq!(
        capability_avc_support(
            &CapabilitySet::V8 {
                flags: CapabilitiesV8Flags::empty(),
            },
            false,
            EgfxCodecPolicy::Auto,
        ),
        (false, false)
    );
    assert_eq!(
        capability_avc_support(
            &CapabilitySet::V10_7 {
                flags: CapabilitiesV107Flags::empty(),
            },
            false,
            EgfxCodecPolicy::Auto,
        ),
        (true, true)
    );
    assert_eq!(
        capability_avc_support(
            &CapabilitySet::V10_7 {
                flags: CapabilitiesV107Flags::empty(),
            },
            false,
            EgfxCodecPolicy::Avc444,
        ),
        (true, true)
    );
    assert_eq!(
        capability_avc_support(
            &CapabilitySet::V10_7 {
                flags: CapabilitiesV107Flags::empty(),
            },
            true,
            EgfxCodecPolicy::Avc444,
        ),
        (true, false)
    );
    assert_eq!(
        capability_avc_support(
            &CapabilitySet::V10_7 {
                flags: CapabilitiesV107Flags::AVC_DISABLED,
            },
            false,
            EgfxCodecPolicy::Avc444,
        ),
        (false, false)
    );
    assert_eq!(
        capability_avc_support(
            &CapabilitySet::V10 {
                flags: CapabilitiesV10Flags::AVC_DISABLED,
            },
            false,
            EgfxCodecPolicy::Avc444,
        ),
        (false, false)
    );
}

#[test]
fn preferred_capabilities_keep_v10_auto_and_avc420_fallback() {
    use ironrdp_egfx::pdu::*;

    let auto_caps = preferred_capabilities_for_policy(EgfxCodecPolicy::Auto);
    assert_eq!(
        auto_caps,
        vec![
            CapabilitySet::V10_7 {
                flags: CapabilitiesV107Flags::empty(),
            },
            CapabilitySet::V10 {
                flags: CapabilitiesV10Flags::empty(),
            },
            CapabilitySet::V8_1 {
                flags: CapabilitiesV81Flags::AVC420_ENABLED,
            },
            CapabilitySet::V8 {
                flags: CapabilitiesV8Flags::empty(),
            },
        ]
    );

    let avc444_caps = preferred_capabilities_for_policy(EgfxCodecPolicy::Avc444);
    assert!(matches!(
        avc444_caps.first(),
        Some(CapabilitySet::V10_7 { flags }) if flags.is_empty()
    ));
    assert!(avc444_caps.iter().any(|cap| matches!(
        cap,
        CapabilitySet::V8_1 { flags } if flags.contains(CapabilitiesV81Flags::AVC420_ENABLED)
    )));
}

#[test]
fn avc420_region_preserves_rect16_bounds_and_quant_quality() {
    let region = Avc420Region::new(4, 6, 20, 22, 19, 81);
    let rectangle = region.to_rectangle();
    let quant = region.to_quant_quality();

    assert_eq!(rectangle.left, 4);
    assert_eq!(rectangle.top, 6);
    assert_eq!(rectangle.right, 20);
    assert_eq!(rectangle.bottom, 22);
    assert_eq!(quant.quantization_parameter, 19);
    assert!(!quant.progressive);
    assert_eq!(quant.quality, 81);
}

#[test]
fn clearcodec_sparse_wire_frame_preserves_pixels_and_one_ack_slot() {
    use super::test_support::{negotiated_no_avc_session, ClearCodecFrameOracle};
    let mut session = negotiated_no_avc_session(160, 80);
    let sid = session
        .shared
        .init_or_reuse_surface(&session.handle, &session.event_tx, 160, 80)
        .unwrap();
    let stride = 160 * 4 + 7;
    let data: Vec<_> = (0..stride * 80).map(|i| (i % 251) as u8).collect();
    let damage = [(-4, 2, 8, 5), (100, 20, 90, 12), (102, 22, 3, 5)];
    assert!(session
        .shared
        .send_clearcodec_damage(
            &session.handle,
            &session.event_tx,
            sid,
            &data,
            160,
            80,
            stride,
            &damage,
            123,
            ironrdp_server::PixelFormat::BgrA32
        )
        .unwrap()
        .is_some());
    let trace = drain_gfx_pdus(&mut session.event_rx);
    let id =
        ClearCodecFrameOracle::new().assert_frame(&trace, sid, &data, 160, 80, stride, &damage);
    assert_eq!(session.shared.frames_in_flight(), 1);
    assert_eq!(session.handle.lock().unwrap().frames_in_flight(), 1);
    ack_frame(&mut session.bridge, id, QueueDepth::AvailableBytes(0));
    assert_eq!(session.shared.frames_in_flight(), 0);
}

#[test]
fn clearcodec_preflight_failure_does_not_advance_sequence_or_queue() {
    use super::test_support::{negotiated_no_avc_session, ClearCodecFrameOracle};
    let mut session = negotiated_no_avc_session(16, 16);
    let sid = session
        .shared
        .init_or_reuse_surface(&session.handle, &session.event_tx, 16, 16)
        .unwrap();
    drain_gfx_pdus(&mut session.event_rx);
    let data = vec![42; 16 * 16 * 4];
    let damage = [(0, 0, 16, 16)];
    let (closed, rx) = mpsc::unbounded_channel();
    drop(rx);
    assert!(session
        .shared
        .send_clearcodec_damage(
            &session.handle,
            &closed,
            sid,
            &data,
            16,
            16,
            64,
            &damage,
            0,
            ironrdp_server::PixelFormat::BgrA32
        )
        .unwrap()
        .is_none());
    assert!(session
        .shared
        .send_clearcodec_damage(
            &session.handle,
            &session.event_tx,
            sid,
            &data[..12],
            16,
            16,
            64,
            &damage,
            0,
            ironrdp_server::PixelFormat::BgrA32
        )
        .is_err());
    assert!(session
        .shared
        .send_clearcodec_damage(
            &session.handle,
            &session.event_tx,
            u16::MAX,
            &data,
            16,
            16,
            64,
            &damage,
            0,
            ironrdp_server::PixelFormat::BgrA32
        )
        .unwrap()
        .is_none());
    assert!(session
        .shared
        .send_clearcodec_damage(
            &session.handle,
            &session.event_tx,
            sid,
            &data,
            8,
            16,
            64,
            &damage,
            0,
            ironrdp_server::PixelFormat::BgrA32
        )
        .is_err());
    assert!(session
        .shared
        .send_clearcodec_damage(
            &session.handle,
            &session.event_tx,
            sid,
            &data,
            16,
            16,
            64,
            &[],
            0,
            ironrdp_server::PixelFormat::BgrA32
        )
        .unwrap()
        .is_none());
    assert_eq!(session.shared.frames_in_flight(), 0);
    assert_eq!(session.handle.lock().unwrap().frames_in_flight(), 0);
    drain_gfx_pdus(&mut session.event_rx).assert_empty();
    assert!(session
        .shared
        .send_clearcodec_damage(
            &session.handle,
            &session.event_tx,
            sid,
            &data,
            16,
            16,
            64,
            &damage,
            0,
            ironrdp_server::PixelFormat::BgrA32
        )
        .unwrap()
        .is_some());
    ClearCodecFrameOracle::new().assert_frame(
        &drain_gfx_pdus(&mut session.event_rx),
        sid,
        &data,
        16,
        16,
        64,
        &damage,
    );
}

#[test]
fn clearcodec_sequence_wraps_survives_resize_and_resets_for_new_client() {
    use super::test_support::{
        negotiated_no_avc_session, process_no_avc_capabilities, start_gfx_channel,
        ClearCodecFrameOracle,
    };
    use ironrdp_server::{GfxServerFactory, ServerEventSender};
    let mut session = negotiated_no_avc_session(8, 8);
    let mut oracle = ClearCodecFrameOracle::new();
    let mut sid = session
        .shared
        .init_or_reuse_surface(&session.handle, &session.event_tx, 8, 8)
        .unwrap();
    for frame in 0..260 {
        if frame == 128 {
            session.shared.prepare_for_resize(8, 8);
            sid = session
                .shared
                .init_or_reuse_surface(&session.handle, &session.event_tx, 8, 8)
                .unwrap();
        }
        let data = vec![frame as u8; 8 * 8 * 4];
        assert!(session
            .shared
            .send_clearcodec_damage(
                &session.handle,
                &session.event_tx,
                sid,
                &data,
                8,
                8,
                32,
                &[(0, 0, 8, 8)],
                0,
                ironrdp_server::PixelFormat::BgrA32
            )
            .unwrap()
            .is_some());
        let id = oracle.assert_frame(
            &drain_gfx_pdus(&mut session.event_rx),
            sid,
            &data,
            8,
            8,
            32,
            &[(0, 0, 8, 8)],
        );
        ack_frame(&mut session.bridge, id, QueueDepth::AvailableBytes(0));
    }
    let mut factory = HyprGfxFactory::new(Arc::clone(&session.shared));
    factory.set_sender(session.event_tx.clone());
    let (mut bridge, handle) = factory.build_server_with_handle().unwrap();
    start_gfx_channel(&mut bridge);
    process_no_avc_capabilities(&mut bridge);
    let sid = session
        .shared
        .init_or_reuse_surface(&handle, &session.event_tx, 8, 8)
        .unwrap();
    let data = vec![99; 8 * 8 * 4];
    assert!(session
        .shared
        .send_clearcodec_damage(
            &handle,
            &session.event_tx,
            sid,
            &data,
            8,
            8,
            32,
            &[(0, 0, 8, 8)],
            0,
            ironrdp_server::PixelFormat::BgrA32
        )
        .unwrap()
        .is_some());
    ClearCodecFrameOracle::new().assert_frame(
        &drain_gfx_pdus(&mut session.event_rx),
        sid,
        &data,
        8,
        8,
        32,
        &[(0, 0, 8, 8)],
    );
    assert_eq!(session.shared.frames_in_flight(), 1);
}

#[test]
fn clearcodec_negotiation_covers_v8_v81_and_avc_disabled_v10() {
    use super::test_support::{start_gfx_channel, ClearCodecFrameOracle};
    use ironrdp_egfx::pdu::{
        CapabilitiesAdvertisePdu, CapabilitiesV10Flags, CapabilitiesV81Flags, CapabilitiesV8Flags,
        CapabilitySet,
    };
    for capability in [
        CapabilitySet::V8 {
            flags: CapabilitiesV8Flags::empty(),
        },
        CapabilitySet::V8_1 {
            flags: CapabilitiesV81Flags::empty(),
        },
        CapabilitySet::V10 {
            flags: CapabilitiesV10Flags::AVC_DISABLED,
        },
    ] {
        let mut session = unnegotiated_egfx_session(16, 16, EgfxCodecPolicy::Auto);
        start_gfx_channel(&mut session.bridge);
        let caps =
            GfxPdu::CapabilitiesAdvertise(CapabilitiesAdvertisePdu::from_typed(&[capability]));
        session
            .bridge
            .process(TEST_CHANNEL_ID, &encode_vec(&caps).unwrap())
            .unwrap();
        assert!(session.shared.is_ready());
        assert!(!session.shared.is_avc_enabled());
        let sid = session
            .shared
            .init_or_reuse_surface(&session.handle, &session.event_tx, 16, 16)
            .unwrap();
        let data = vec![31; 16 * 16 * 4];
        assert!(session
            .shared
            .send_clearcodec_damage(
                &session.handle,
                &session.event_tx,
                sid,
                &data,
                16,
                16,
                64,
                &[(0, 0, 16, 16)],
                0,
                ironrdp_server::PixelFormat::BgrA32
            )
            .unwrap()
            .is_some());
        ClearCodecFrameOracle::new().assert_frame(
            &drain_gfx_pdus(&mut session.event_rx),
            sid,
            &data,
            16,
            16,
            64,
            &[(0, 0, 16, 16)],
        );
    }
}

#[test]
fn clearcodec_transport_backpressure_and_suspended_ack_preserve_sequence() {
    use super::test_support::{negotiated_no_avc_session, ClearCodecFrameOracle};
    let mut session = negotiated_no_avc_session(16, 16);
    let sid = session
        .shared
        .init_or_reuse_surface(&session.handle, &session.event_tx, 16, 16)
        .unwrap();
    session.handle.lock().unwrap().set_max_frames_in_flight(1);
    let mut oracle = ClearCodecFrameOracle::new();
    let data = vec![17; 16 * 16 * 4];
    let damage = [(0, 0, 16, 16)];
    assert!(session
        .shared
        .send_clearcodec_damage(
            &session.handle,
            &session.event_tx,
            sid,
            &data,
            16,
            16,
            64,
            &damage,
            0,
            ironrdp_server::PixelFormat::BgrA32
        )
        .unwrap()
        .is_some());
    let id = oracle.assert_frame(
        &drain_gfx_pdus(&mut session.event_rx),
        sid,
        &data,
        16,
        16,
        64,
        &damage,
    );
    assert!(!session.shared.should_backpressure_frames());
    assert!(session
        .shared
        .send_clearcodec_damage(
            &session.handle,
            &session.event_tx,
            sid,
            &data,
            16,
            16,
            64,
            &damage,
            0,
            ironrdp_server::PixelFormat::BgrA32
        )
        .unwrap()
        .is_none());
    assert_eq!(session.shared.frames_in_flight(), 1);
    drain_gfx_pdus(&mut session.event_rx).assert_empty();
    ack_frame(&mut session.bridge, id, QueueDepth::AvailableBytes(0));
    assert!(session
        .shared
        .send_clearcodec_damage(
            &session.handle,
            &session.event_tx,
            sid,
            &data,
            16,
            16,
            64,
            &damage,
            0,
            ironrdp_server::PixelFormat::BgrA32
        )
        .unwrap()
        .is_some());
    let id = oracle.assert_frame(
        &drain_gfx_pdus(&mut session.event_rx),
        sid,
        &data,
        16,
        16,
        64,
        &damage,
    );
    ack_frame(&mut session.bridge, id, QueueDepth::Suspend);
    for _ in 0..DEFAULT_MAX_FRAMES_IN_FLIGHT + 2 {
        assert!(session
            .shared
            .send_clearcodec_damage(
                &session.handle,
                &session.event_tx,
                sid,
                &data,
                16,
                16,
                64,
                &damage,
                0,
                ironrdp_server::PixelFormat::BgrA32
            )
            .unwrap()
            .is_some());
        oracle.assert_frame(
            &drain_gfx_pdus(&mut session.event_rx),
            sid,
            &data,
            16,
            16,
            64,
            &damage,
        );
    }
}
