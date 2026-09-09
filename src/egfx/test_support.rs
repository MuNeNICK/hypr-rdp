use ironrdp_core::{encode_vec, Decode, ReadCursor};
use ironrdp_dvc::pdu::{DrdynvcDataPdu, DrdynvcServerPdu};
use ironrdp_egfx::pdu::{
    Avc444BitmapStream, Codec1Type, Encoding, FrameAcknowledgePdu, GfxPdu, QueueDepth,
    WireToSurface1Pdu,
};
use ironrdp_server::{EgfxServerMessage, GfxDvcBridge, GfxServerHandle, ServerEvent};
use openh264::decoder::{Decoder, DecoderConfig};
use openh264::formats::YUVSource;
use openh264::OpenH264API;
use std::sync::Arc;
use tokio::sync::mpsc;
use yuv::{
    bgra_to_yuv444, BufferStoreMut, YuvConversionMode, YuvPlanarImageMut, YuvRange,
    YuvStandardMatrix,
};

use super::{EgfxCodecPolicy, EgfxShared, HyprGfxFactory, DEFAULT_MAX_FRAMES_IN_FLIGHT};

pub(crate) const TEST_CHANNEL_ID: u32 = 1007;

pub(crate) struct TestGfxSession {
    pub(crate) shared: Arc<EgfxShared>,
    pub(crate) bridge: GfxDvcBridge,
    pub(crate) handle: GfxServerHandle,
    pub(crate) event_tx: mpsc::UnboundedSender<ServerEvent>,
    pub(crate) event_rx: mpsc::UnboundedReceiver<ServerEvent>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ExpectedAvc444Encoding {
    LumaAndChroma,
    Luma,
    Chroma,
}

impl ExpectedAvc444Encoding {
    fn wire(self) -> Encoding {
        match self {
            Self::LumaAndChroma => Encoding::LUMA_AND_CHROMA,
            Self::Luma => Encoding::LUMA,
            Self::Chroma => Encoding::CHROMA,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TestQueueDepth {
    AvailableBytes(u32),
}

impl TestQueueDepth {
    fn wire(self) -> QueueDepth {
        match self {
            Self::AvailableBytes(bytes) => QueueDepth::AvailableBytes(bytes),
        }
    }
}

impl From<TestQueueDepth> for QueueDepth {
    fn from(value: TestQueueDepth) -> Self {
        value.wire()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct EncodedFrameSummary {
    pub(crate) surface_id: u16,
    pub(crate) frame_id: u32,
}

#[derive(Debug)]
pub(crate) struct GfxPduTrace {
    pdus: Vec<GfxPdu>,
}

impl GfxPduTrace {
    pub(in crate::egfx) fn as_slice(&self) -> &[GfxPdu] {
        &self.pdus
    }

    pub(in crate::egfx) fn len(&self) -> usize {
        self.pdus.len()
    }

    pub(in crate::egfx) fn is_empty(&self) -> bool {
        self.pdus.is_empty()
    }

    pub(crate) fn assert_empty(&self) {
        assert!(
            self.pdus.is_empty(),
            "expected no GFX PDUs: {:?}",
            self.pdus
        );
    }

    pub(crate) fn assert_no_encoded_frame(&self) {
        assert!(
            self.pdus
                .iter()
                .all(|pdu| !matches!(pdu, GfxPdu::WireToSurface1(_))),
            "expected no encoded frame PDU: {:?}",
            self.pdus
        );
    }

    pub(crate) fn frame_id(&self) -> u32 {
        match self.pdus.iter().find_map(|pdu| match pdu {
            GfxPdu::StartFrame(start) => Some(start.frame_id),
            _ => None,
        }) {
            Some(frame_id) => frame_id,
            None => panic!("expected StartFrame in PDU list"),
        }
    }

    pub(crate) fn first_created_surface_id(&self) -> u16 {
        self.pdus
            .iter()
            .find_map(|pdu| match pdu {
                GfxPdu::CreateSurface(create) => Some(create.surface_id),
                _ => None,
            })
            .expect("expected CreateSurface PDU")
    }

    pub(crate) fn contains_delete_surface(&self, surface_id: u16) -> bool {
        self.pdus.iter().any(
            |pdu| matches!(pdu, GfxPdu::DeleteSurface(delete) if delete.surface_id == surface_id),
        )
    }

    pub(crate) fn contains_map_surface_to_output(&self, surface_id: u16) -> bool {
        self.pdus.iter().any(
            |pdu| matches!(pdu, GfxPdu::MapSurfaceToOutput(map) if map.surface_id == surface_id),
        )
    }

    pub(crate) fn assert_initial_surface_setup_precedes_logical_frame(
        &self,
        width: u16,
        height: u16,
    ) -> EncodedFrameSummary {
        assert_eq!(self.pdus.len(), 6);
        let surface_id = match &self.pdus[1] {
            GfxPdu::CreateSurface(create) => create.surface_id,
            other => panic!("expected CreateSurface second, got {other:?}"),
        };
        match &self.pdus[0] {
            GfxPdu::ResetGraphics(reset) => {
                assert_eq!(reset.width, u32::from(width));
                assert_eq!(reset.height, u32::from(height));
                assert!(reset.monitors.is_empty());
            }
            other => panic!("expected ResetGraphics first, got {other:?}"),
        }
        match &self.pdus[2] {
            GfxPdu::MapSurfaceToOutput(map) => assert_eq!(map.surface_id, surface_id),
            other => panic!("expected MapSurfaceToOutput third, got {other:?}"),
        }
        let start = match &self.pdus[3] {
            GfxPdu::StartFrame(start) => start,
            other => panic!("expected StartFrame after surface setup, got {other:?}"),
        };
        let wire = match &self.pdus[4] {
            GfxPdu::WireToSurface1(wire) => wire,
            other => panic!("expected WireToSurface1 inside logical frame, got {other:?}"),
        };
        let end = match &self.pdus[5] {
            GfxPdu::EndFrame(end) => end,
            other => panic!("expected EndFrame after WireToSurface1, got {other:?}"),
        };

        assert_eq!(wire.surface_id, surface_id);
        assert_eq!(end.frame_id, start.frame_id);

        EncodedFrameSummary {
            surface_id,
            frame_id: start.frame_id,
        }
    }

    pub(crate) fn assert_sendable_avc444_frame(
        &self,
        expected_encoding: ExpectedAvc444Encoding,
    ) -> EncodedFrameSummary {
        let wire = self
            .pdus
            .iter()
            .find_map(|pdu| match pdu {
                GfxPdu::WireToSurface1(wire) => Some(wire),
                _ => None,
            })
            .expect("AVC444 frame emits WireToSurface1");
        assert_eq!(wire.codec_id, Codec1Type::Avc444v2);

        let expected_encoding = expected_encoding.wire();
        let mut cursor = ReadCursor::new(&wire.bitmap_data);
        let bitmap = Avc444BitmapStream::decode(&mut cursor).expect("AVC444 payload decodes");
        assert_eq!(bitmap.encoding, expected_encoding);
        assert!(!bitmap.stream1.data.is_empty());
        assert!(!bitmap.stream1.rectangles.is_empty());

        if expected_encoding == Encoding::LUMA_AND_CHROMA {
            let stream2 = bitmap.stream2.expect("LC=0 carries stream2");
            assert!(!stream2.data.is_empty());
            assert!(!stream2.rectangles.is_empty());
        } else {
            assert!(bitmap.stream2.is_none());
        }

        EncodedFrameSummary {
            surface_id: wire.surface_id,
            frame_id: self.frame_id(),
        }
    }
}

#[derive(Debug)]
struct DecodedYuv420Frame {
    width: usize,
    height: usize,
    y: Vec<u8>,
    u: Vec<u8>,
    v: Vec<u8>,
}

pub(crate) struct Avc444PresentationOracle {
    decoder: Decoder,
    width: usize,
    height: usize,
    previous_luma: Option<DecodedYuv420Frame>,
    previous_chroma: Option<DecodedYuv420Frame>,
    previous_image: Option<Vec<u8>>,
    previous_hash: Option<u64>,
    hashes: Vec<u64>,
}

impl Avc444PresentationOracle {
    pub(crate) fn new(width: usize, height: usize) -> Self {
        Self {
            decoder: test_openh264_decoder(),
            width,
            height,
            previous_luma: None,
            previous_chroma: None,
            previous_image: None,
            previous_hash: None,
            hashes: Vec::new(),
        }
    }

    pub(crate) fn assert_trace_decodes_pictures(&mut self, trace: &GfxPduTrace) -> usize {
        let mut decoded_pictures = 0usize;
        for pdu in trace.as_slice() {
            let GfxPdu::WireToSurface1(wire) = pdu else {
                continue;
            };
            let mut bitmap_cursor = ReadCursor::new(&wire.bitmap_data);
            let bitmap =
                Avc444BitmapStream::decode(&mut bitmap_cursor).expect("AVC444 payload decodes");

            let _ = decode_h264_payload(
                &mut self.decoder,
                "AVC444 stream1",
                bitmap.stream1.data,
                self.width,
                self.height,
            );
            decoded_pictures += 1;

            match (bitmap.encoding, bitmap.stream2) {
                (Encoding::LUMA_AND_CHROMA, Some(stream2)) => {
                    let _ = decode_h264_payload(
                        &mut self.decoder,
                        "AVC444 stream2",
                        stream2.data,
                        self.width,
                        self.height,
                    );
                    decoded_pictures += 1;
                }
                (Encoding::LUMA_AND_CHROMA, None) => {
                    panic!("LC=0 AVC444 frame must carry stream2")
                }
                (_, None) => {}
                (_, Some(_)) => panic!("single-stream AVC444 frame must not carry stream2"),
            }
        }

        assert!(
            decoded_pictures > 0,
            "trace must include decoded AVC444 pictures"
        );
        decoded_pictures
    }

    pub(crate) fn assert_trace_reconstructs_visible_progress(
        &mut self,
        trace: &GfxPduTrace,
        frame_index: usize,
    ) {
        self.assert_trace_reconstructs_visible_progress_with_min_delta(trace, frame_index, 128);
    }

    pub(crate) fn assert_trace_reconstructs_visible_progress_with_min_delta(
        &mut self,
        trace: &GfxPduTrace,
        frame_index: usize,
        min_changed_bytes: usize,
    ) {
        let image = reconstruct_avc444_trace_yuv444(
            trace,
            &mut self.decoder,
            self.width,
            self.height,
            &mut self.previous_luma,
            &mut self.previous_chroma,
        );
        let hash = stable_hash(&image);

        if let (Some(previous_image), Some(previous_hash)) =
            (self.previous_image.as_ref(), self.previous_hash)
        {
            assert_ne!(
                hash, previous_hash,
                "AVC444 reconstructed presentation image must advance for frame {frame_index}"
            );
            assert!(
                significant_byte_delta(previous_image, &image) > min_changed_bytes,
                "AVC444 reconstructed presentation image changed too little for frame {frame_index}"
            );
        }

        self.hashes.push(hash);
        self.previous_hash = Some(hash);
        self.previous_image = Some(image);
    }

    pub(crate) fn assert_distinct_reconstructed_frames(&mut self, expected: usize) {
        self.hashes.sort_unstable();
        self.hashes.dedup();
        assert_eq!(
            self.hashes.len(),
            expected,
            "each synthetic AVC444 frame must produce a distinct reconstructed presentation image"
        );
    }

    pub(crate) fn assert_trace_matches_bgra_with_bounded_yuv444_error(
        &mut self,
        trace: &GfxPduTrace,
        frame_index: usize,
        bgra: &[u8],
        stride: usize,
        max_mean_abs_error: f32,
        max_large_error_ratio: f32,
    ) {
        let image = reconstruct_avc444_trace_yuv444(
            trace,
            &mut self.decoder,
            self.width,
            self.height,
            &mut self.previous_luma,
            &mut self.previous_chroma,
        );
        let expected = bgra_to_yuv444_reference(self.width, self.height, bgra, stride);
        let metrics = yuv444_error_metrics(&expected, &image);

        assert!(
            metrics.mean_abs_error <= max_mean_abs_error,
            "AVC444 reconstructed YUV444 mean abs error too high for frame {frame_index}: {} > {}",
            metrics.mean_abs_error,
            max_mean_abs_error
        );
        assert!(
            metrics.large_error_ratio <= max_large_error_ratio,
            "AVC444 reconstructed YUV444 large-error ratio too high for frame {frame_index}: {} > {}",
            metrics.large_error_ratio,
            max_large_error_ratio
        );

        let hash = stable_hash(&image);
        self.hashes.push(hash);
        self.previous_hash = Some(hash);
        self.previous_image = Some(image);
    }
}

fn test_openh264_decoder() -> Decoder {
    // A separate decoder instance validates the encoded stream.
    let api = OpenH264API::from_source();
    Decoder::with_api_config(api, DecoderConfig::default())
        .expect("OpenH264 decoder must initialize for AVC444 stream-decode coverage")
}

fn copy_decoded_yuv420(picture: &impl YUVSource) -> DecodedYuv420Frame {
    let (width, height) = picture.dimensions();
    let (stride_y, stride_u, stride_v) = picture.strides();
    let mut y = Vec::with_capacity(width * height);
    let mut u = Vec::with_capacity(width * height / 4);
    let mut v = Vec::with_capacity(width * height / 4);

    for row in picture.y().chunks(stride_y).take(height) {
        y.extend_from_slice(&row[..width]);
    }
    for row in picture.u().chunks(stride_u).take(height / 2) {
        u.extend_from_slice(&row[..width / 2]);
    }
    for row in picture.v().chunks(stride_v).take(height / 2) {
        v.extend_from_slice(&row[..width / 2]);
    }

    DecodedYuv420Frame {
        width,
        height,
        y,
        u,
        v,
    }
}

fn decode_h264_payload(
    decoder: &mut Decoder,
    label: &str,
    data: &[u8],
    width: usize,
    height: usize,
) -> DecodedYuv420Frame {
    assert!(!data.is_empty(), "{label} H.264 payload must be non-empty");
    let picture = decoder
        .decode(data)
        .unwrap_or_else(|error| panic!("{label} H.264 payload must decode: {error:#}"))
        .unwrap_or_else(|| panic!("{label} H.264 payload must produce a picture"));
    assert_eq!(
        picture.dimensions(),
        (width, height),
        "{label} decoded dimensions must match the EGFX surface"
    );
    copy_decoded_yuv420(&picture)
}

fn reconstruct_avc444_v2_yuv444(
    width: usize,
    height: usize,
    luma: &DecodedYuv420Frame,
    chroma: &DecodedYuv420Frame,
) -> Vec<u8> {
    assert_eq!((luma.width, luma.height), (width, height));
    assert_eq!((chroma.width, chroma.height), (width, height));
    let chroma_w = width / 2;
    let chroma_h = height / 2;
    let quarter_w = width / 4;
    let mut y = luma.y.clone();
    let mut u = vec![0; width * height];
    let mut v = vec![0; width * height];

    for cy in 0..chroma_h {
        for cx in 0..chroma_w {
            let src = cy * chroma_w + cx;
            let x = cx * 2;
            let row0 = (cy * 2) * width;
            let row1 = row0 + width;
            for dst in [row0 + x, row0 + x + 1, row1 + x, row1 + x + 1] {
                u[dst] = luma.u[src];
                v[dst] = luma.v[src];
            }
        }
    }

    for row in 0..height {
        let aux_base = row * width;
        let out_base = row * width;
        for cx in 0..chroma_w {
            let x = cx * 2 + 1;
            u[out_base + x] = chroma.y[aux_base + cx];
            v[out_base + x] = chroma.y[aux_base + cx + chroma_w];
        }
    }

    for cy in 0..chroma_h {
        let aux_base = cy * chroma_w;
        let out_base = (cy * 2 + 1) * width;
        for qx in 0..quarter_w {
            let src = aux_base + qx;
            let x = qx * 4;
            u[out_base + x] = chroma.u[src];
            v[out_base + x] = chroma.u[src + quarter_w];
            u[out_base + x + 2] = chroma.v[src];
            v[out_base + x + 2] = chroma.v[src + quarter_w];
        }
    }

    y.extend_from_slice(&u);
    y.extend_from_slice(&v);
    y
}

fn reconstruct_yuv420_as_yuv444(width: usize, height: usize, luma: &DecodedYuv420Frame) -> Vec<u8> {
    assert_eq!((luma.width, luma.height), (width, height));
    let chroma_w = width / 2;
    let chroma_h = height / 2;
    let mut y = luma.y.clone();
    let mut u = vec![0; width * height];
    let mut v = vec![0; width * height];

    for cy in 0..chroma_h {
        for cx in 0..chroma_w {
            let src = cy * chroma_w + cx;
            let x = cx * 2;
            let row0 = (cy * 2) * width;
            let row1 = row0 + width;
            for dst in [row0 + x, row0 + x + 1, row1 + x, row1 + x + 1] {
                u[dst] = luma.u[src];
                v[dst] = luma.v[src];
            }
        }
    }

    y.extend_from_slice(&u);
    y.extend_from_slice(&v);
    y
}

fn reconstruct_avc444_trace_yuv444(
    trace: &GfxPduTrace,
    decoder: &mut Decoder,
    width: usize,
    height: usize,
    previous_luma: &mut Option<DecodedYuv420Frame>,
    previous_chroma: &mut Option<DecodedYuv420Frame>,
) -> Vec<u8> {
    let mut reconstructed = None;
    for pdu in trace.as_slice() {
        let GfxPdu::WireToSurface1(wire) = pdu else {
            continue;
        };
        let mut bitmap_cursor = ReadCursor::new(&wire.bitmap_data);
        let bitmap =
            Avc444BitmapStream::decode(&mut bitmap_cursor).expect("AVC444 payload decodes");
        match (bitmap.encoding, bitmap.stream2) {
            (Encoding::LUMA_AND_CHROMA, Some(stream2)) => {
                let luma = decode_h264_payload(
                    decoder,
                    "AVC444 luma stream",
                    bitmap.stream1.data,
                    width,
                    height,
                );
                let chroma = decode_h264_payload(
                    decoder,
                    "AVC444 chroma stream",
                    stream2.data,
                    width,
                    height,
                );
                reconstructed = Some(reconstruct_avc444_v2_yuv444(width, height, &luma, &chroma));
                *previous_luma = Some(luma);
                *previous_chroma = Some(chroma);
            }
            (Encoding::LUMA_AND_CHROMA, None) => {
                panic!("LC=0 AVC444 frame must carry stream2")
            }
            (Encoding::LUMA, None) => {
                let luma = decode_h264_payload(
                    decoder,
                    "AVC444 luma stream",
                    bitmap.stream1.data,
                    width,
                    height,
                );
                reconstructed = Some(reconstruct_yuv420_as_yuv444(width, height, &luma));
                *previous_luma = Some(luma);
            }
            (Encoding::CHROMA, None) => {
                let chroma = decode_h264_payload(
                    decoder,
                    "AVC444 chroma stream",
                    bitmap.stream1.data,
                    width,
                    height,
                );
                let luma = previous_luma
                    .as_ref()
                    .expect("LC=2 AVC444 frame requires a previous luma frame");
                reconstructed = Some(reconstruct_avc444_v2_yuv444(width, height, luma, &chroma));
                *previous_chroma = Some(chroma);
            }
            (_, Some(_)) => panic!("single-stream AVC444 frame must not carry stream2"),
            (other, None) => {
                panic!("unsupported AVC444 LC value in presentation oracle: {other:?}")
            }
        }
    }

    reconstructed.expect("trace must include an AVC444 WireToSurface frame")
}

fn stable_hash(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn significant_byte_delta(left: &[u8], right: &[u8]) -> usize {
    assert_eq!(left.len(), right.len());
    left.iter()
        .zip(right)
        .filter(|(a, b)| a.abs_diff(**b) > 2)
        .count()
}

fn bgra_to_yuv444_reference(width: usize, height: usize, bgra: &[u8], stride: usize) -> Vec<u8> {
    let mut y = vec![0u8; width * height];
    let mut u = vec![0u8; width * height];
    let mut v = vec![0u8; width * height];
    let mut image = YuvPlanarImageMut {
        y_plane: BufferStoreMut::Borrowed(&mut y),
        y_stride: width as u32,
        u_plane: BufferStoreMut::Borrowed(&mut u),
        u_stride: width as u32,
        v_plane: BufferStoreMut::Borrowed(&mut v),
        v_stride: width as u32,
        width: width as u32,
        height: height as u32,
    };

    bgra_to_yuv444(
        &mut image,
        bgra,
        stride as u32,
        YuvRange::Full,
        YuvStandardMatrix::Bt709,
        YuvConversionMode::Balanced,
    )
    .expect("BGRA test frame converts to YUV444");

    y.extend_from_slice(&u);
    y.extend_from_slice(&v);
    y
}

struct Yuv444ErrorMetrics {
    mean_abs_error: f32,
    large_error_ratio: f32,
}

fn yuv444_error_metrics(expected: &[u8], actual: &[u8]) -> Yuv444ErrorMetrics {
    assert_eq!(expected.len(), actual.len());
    let mut total_error = 0usize;
    let mut large_errors = 0usize;
    for (&expected, &actual) in expected.iter().zip(actual) {
        let error = expected.abs_diff(actual) as usize;
        total_error += error;
        if error > 32 {
            large_errors += 1;
        }
    }

    Yuv444ErrorMetrics {
        mean_abs_error: total_error as f32 / expected.len() as f32,
        large_error_ratio: large_errors as f32 / expected.len() as f32,
    }
}

pub(in crate::egfx) fn decode_gfx_output(message: &ironrdp_dvc::DvcMessage) -> GfxPdu {
    let wrapped = encode_vec(&**message).expect("DVC message encodes");
    assert_eq!(&wrapped[0..2], &[0xe0, 0x04]);
    let mut cursor = ReadCursor::new(&wrapped[2..]);
    GfxPdu::decode(&mut cursor).expect("GFX PDU decodes")
}

pub(in crate::egfx) fn decode_avc444_wire_to_surface(
    message: &ironrdp_dvc::DvcMessage,
) -> WireToSurface1Pdu {
    match decode_gfx_output(message) {
        GfxPdu::WireToSurface1(pdu) => pdu,
        other => panic!("expected WireToSurface1, got {other:?}"),
    }
}

pub(in crate::egfx) fn assert_wire_to_surface_frame(
    pdus: &[GfxPdu],
    surface_id: u16,
    codec_id: Codec1Type,
) -> &WireToSurface1Pdu {
    assert_eq!(pdus.len(), 3);
    let start = match &pdus[0] {
        GfxPdu::StartFrame(start) => start,
        other => panic!("expected StartFrame, got {other:?}"),
    };
    let wire = match &pdus[1] {
        GfxPdu::WireToSurface1(wire) => wire,
        other => panic!("expected WireToSurface1, got {other:?}"),
    };
    let end = match &pdus[2] {
        GfxPdu::EndFrame(end) => end,
        other => panic!("expected EndFrame, got {other:?}"),
    };

    assert_eq!(end.frame_id, start.frame_id);
    assert_eq!(wire.surface_id, surface_id);
    assert_eq!(wire.codec_id, codec_id);
    assert!(!wire.bitmap_data.is_empty());
    wire
}

pub(crate) fn start_gfx_channel(bridge: &mut GfxDvcBridge) {
    ironrdp_dvc::DvcProcessor::start(bridge, TEST_CHANNEL_ID).expect("channel starts");
}

pub(crate) fn process_avc444_capabilities(bridge: &mut GfxDvcBridge) {
    let caps =
        GfxPdu::CapabilitiesAdvertise(ironrdp_egfx::pdu::CapabilitiesAdvertisePdu::from_typed(&[
            ironrdp_egfx::pdu::CapabilitySet::V10_7 {
                flags: ironrdp_egfx::pdu::CapabilitiesV107Flags::empty(),
            },
        ]));
    let caps = encode_vec(&caps).expect("capabilities encode");
    let _ = ironrdp_dvc::DvcProcessor::process(bridge, TEST_CHANNEL_ID, &caps)
        .expect("capabilities process");
}

pub(crate) fn process_avc420_capabilities(bridge: &mut GfxDvcBridge) {
    let caps =
        GfxPdu::CapabilitiesAdvertise(ironrdp_egfx::pdu::CapabilitiesAdvertisePdu::from_typed(&[
            ironrdp_egfx::pdu::CapabilitySet::V8_1 {
                flags: ironrdp_egfx::pdu::CapabilitiesV81Flags::AVC420_ENABLED,
            },
        ]));
    let caps = encode_vec(&caps).expect("capabilities encode");
    let _ = ironrdp_dvc::DvcProcessor::process(bridge, TEST_CHANNEL_ID, &caps)
        .expect("capabilities process");
}

pub(crate) fn process_no_avc_capabilities(bridge: &mut GfxDvcBridge) {
    let caps =
        GfxPdu::CapabilitiesAdvertise(ironrdp_egfx::pdu::CapabilitiesAdvertisePdu::from_typed(&[
            ironrdp_egfx::pdu::CapabilitySet::V8 {
                flags: ironrdp_egfx::pdu::CapabilitiesV8Flags::empty(),
            },
        ]));
    let caps = encode_vec(&caps).expect("capabilities encode");
    let _ = ironrdp_dvc::DvcProcessor::process(bridge, TEST_CHANNEL_ID, &caps)
        .expect("capabilities process");
}

pub(crate) fn unnegotiated_egfx_shared(
    width: u16,
    height: u16,
    codec_policy: EgfxCodecPolicy,
) -> Arc<EgfxShared> {
    let shared = Arc::new(EgfxShared::with_codec_policy(
        DEFAULT_MAX_FRAMES_IN_FLIGHT,
        codec_policy,
    ));
    shared.set_surface_size(width, height);
    let (event_tx, _event_rx) = mpsc::unbounded_channel();
    let mut factory = HyprGfxFactory::new(Arc::clone(&shared));
    ironrdp_server::ServerEventSender::set_sender(&mut factory, event_tx.clone());
    let (_bridge, _handle) = ironrdp_server::GfxServerFactory::build_server_with_handle(&factory)
        .expect("EGFX server builds");
    assert!(!shared.is_ready());
    shared
}

pub(crate) fn unnegotiated_egfx_session(
    width: u16,
    height: u16,
    codec_policy: EgfxCodecPolicy,
) -> TestGfxSession {
    let shared = Arc::new(EgfxShared::with_codec_policy(
        DEFAULT_MAX_FRAMES_IN_FLIGHT,
        codec_policy,
    ));
    shared.set_surface_size(width, height);
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let mut factory = HyprGfxFactory::new(Arc::clone(&shared));
    ironrdp_server::ServerEventSender::set_sender(&mut factory, event_tx.clone());
    let (bridge, handle) = ironrdp_server::GfxServerFactory::build_server_with_handle(&factory)
        .expect("EGFX server builds");
    assert!(!shared.is_ready());
    TestGfxSession {
        shared,
        bridge,
        handle,
        event_tx,
        event_rx,
    }
}

pub(crate) fn negotiated_egfx_with_policy(
    width: u16,
    height: u16,
    codec_policy: EgfxCodecPolicy,
) -> (Arc<EgfxShared>, mpsc::UnboundedReceiver<ServerEvent>) {
    let shared = Arc::new(EgfxShared::with_codec_policy(
        DEFAULT_MAX_FRAMES_IN_FLIGHT,
        codec_policy,
    ));
    shared.set_surface_size(width, height);
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let mut factory = HyprGfxFactory::new(Arc::clone(&shared));
    ironrdp_server::ServerEventSender::set_sender(&mut factory, event_tx.clone());
    let (mut bridge, _handle) =
        ironrdp_server::GfxServerFactory::build_server_with_handle(&factory)
            .expect("EGFX server builds");
    start_gfx_channel(&mut bridge);
    process_avc444_capabilities(&mut bridge);

    assert!(shared.is_ready());
    assert!(shared.is_avc_enabled());

    (shared, event_rx)
}

pub(crate) fn negotiated_avc420_session(
    width: u16,
    height: u16,
    max_frames_in_flight: u32,
) -> (
    Arc<EgfxShared>,
    GfxDvcBridge,
    GfxServerHandle,
    mpsc::UnboundedSender<ServerEvent>,
    mpsc::UnboundedReceiver<ServerEvent>,
) {
    let shared = Arc::new(EgfxShared::with_codec_policy(
        max_frames_in_flight,
        EgfxCodecPolicy::Auto,
    ));
    shared.set_surface_size(width, height);
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let mut factory = HyprGfxFactory::new(Arc::clone(&shared));
    ironrdp_server::ServerEventSender::set_sender(&mut factory, event_tx.clone());
    let (mut bridge, handle) = ironrdp_server::GfxServerFactory::build_server_with_handle(&factory)
        .expect("EGFX server builds");
    start_gfx_channel(&mut bridge);
    process_avc420_capabilities(&mut bridge);
    assert!(shared.is_ready());
    assert!(shared.is_avc_enabled());
    assert!(!shared.is_avc444_enabled());

    (shared, bridge, handle, event_tx, event_rx)
}

pub(crate) fn ready_avc420_session(
    width: u16,
    height: u16,
) -> (
    GfxServerHandle,
    u16,
    mpsc::UnboundedSender<ServerEvent>,
    mpsc::UnboundedReceiver<ServerEvent>,
) {
    let (_shared, _bridge, handle, surface_id, event_tx, event_rx) =
        ready_tracked_avc420_session(width, height, DEFAULT_MAX_FRAMES_IN_FLIGHT);
    (handle, surface_id, event_tx, event_rx)
}

pub(crate) fn ready_tracked_avc420_session(
    width: u16,
    height: u16,
    max_frames_in_flight: u32,
) -> (
    Arc<EgfxShared>,
    GfxDvcBridge,
    GfxServerHandle,
    u16,
    mpsc::UnboundedSender<ServerEvent>,
    mpsc::UnboundedReceiver<ServerEvent>,
) {
    let (shared, bridge, handle, event_tx, event_rx) =
        negotiated_avc420_session(width, height, max_frames_in_flight);
    let surface_id =
        EgfxShared::init_surface(&handle, &event_tx, width, height).expect("surface init");
    (shared, bridge, handle, surface_id, event_tx, event_rx)
}

pub(crate) fn ready_avc444_handle(width: u16, height: u16) -> (GfxServerHandle, u16) {
    let shared = Arc::new(EgfxShared::with_codec_policy(
        DEFAULT_MAX_FRAMES_IN_FLIGHT,
        EgfxCodecPolicy::Avc444,
    ));
    shared.set_surface_size(width, height);
    let (event_tx, _event_rx) = mpsc::unbounded_channel();
    let mut factory = HyprGfxFactory::new(Arc::clone(&shared));
    ironrdp_server::ServerEventSender::set_sender(&mut factory, event_tx.clone());
    let (mut bridge, handle) = ironrdp_server::GfxServerFactory::build_server_with_handle(&factory)
        .expect("EGFX server builds");
    start_gfx_channel(&mut bridge);
    process_avc444_capabilities(&mut bridge);
    assert!(shared.is_ready());
    assert!(shared.is_avc444_enabled());

    let surface_id =
        EgfxShared::init_surface(&handle, &event_tx, width, height).expect("surface init");
    (handle, surface_id)
}

pub(crate) fn negotiated_avc444_egfx(
    width: u16,
    height: u16,
) -> (Arc<EgfxShared>, mpsc::UnboundedReceiver<ServerEvent>) {
    let (shared, event_rx) = negotiated_egfx_with_policy(width, height, EgfxCodecPolicy::Avc444);
    assert!(shared.is_avc444_enabled());
    (shared, event_rx)
}

pub(crate) fn negotiated_no_avc_egfx(
    width: u16,
    height: u16,
) -> (Arc<EgfxShared>, mpsc::UnboundedReceiver<ServerEvent>) {
    let shared = Arc::new(EgfxShared::with_codec_policy(
        DEFAULT_MAX_FRAMES_IN_FLIGHT,
        EgfxCodecPolicy::Auto,
    ));
    shared.set_surface_size(width, height);
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let mut factory = HyprGfxFactory::new(Arc::clone(&shared));
    ironrdp_server::ServerEventSender::set_sender(&mut factory, event_tx);
    let (mut bridge, _handle) =
        ironrdp_server::GfxServerFactory::build_server_with_handle(&factory)
            .expect("EGFX server builds");
    start_gfx_channel(&mut bridge);
    process_no_avc_capabilities(&mut bridge);

    assert!(shared.is_ready());
    assert!(!shared.is_avc_enabled());

    (shared, event_rx)
}

pub(crate) fn tracked_avc444_session(
    width: u16,
    height: u16,
    max_frames_in_flight: u32,
) -> TestGfxSession {
    let shared = Arc::new(EgfxShared::with_codec_policy(
        max_frames_in_flight,
        EgfxCodecPolicy::Avc444,
    ));
    shared.set_surface_size(width, height);
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let mut factory = HyprGfxFactory::new(Arc::clone(&shared));
    ironrdp_server::ServerEventSender::set_sender(&mut factory, event_tx.clone());
    let (mut bridge, handle) = ironrdp_server::GfxServerFactory::build_server_with_handle(&factory)
        .expect("EGFX server builds");
    start_gfx_channel(&mut bridge);
    process_avc444_capabilities(&mut bridge);
    assert!(shared.is_ready());
    assert!(shared.is_avc444_enabled());

    TestGfxSession {
        shared,
        bridge,
        handle,
        event_tx,
        event_rx,
    }
}

pub(crate) fn drain_gfx_pdus(event_rx: &mut mpsc::UnboundedReceiver<ServerEvent>) -> GfxPduTrace {
    let mut pdus = Vec::new();
    let mut expected_fragment_len = 0usize;
    let mut fragments = Vec::new();

    while let Ok(event) = event_rx.try_recv() {
        let ServerEvent::Egfx(EgfxServerMessage::SendMessages { messages }) = event else {
            continue;
        };

        for message in messages {
            let encoded = message.encode_unframed_pdu().expect("DVC message encodes");
            let mut cursor = ReadCursor::new(&encoded);
            let dvc = DrdynvcServerPdu::decode(&mut cursor).expect("DVC message decodes");
            let DrdynvcServerPdu::Data(data) = dvc else {
                continue;
            };

            let complete = match data {
                DrdynvcDataPdu::DataFirst(data_first) => {
                    let total_len = data_first.length() as usize;
                    if total_len == data_first.data().len() {
                        Some(data_first.into_data())
                    } else {
                        expected_fragment_len = total_len;
                        fragments = data_first.into_data();
                        None
                    }
                }
                DrdynvcDataPdu::Data(mut data) => {
                    if expected_fragment_len == 0 {
                        Some(data.into_data())
                    } else {
                        fragments.append(data.data_mut());
                        if fragments.len() == expected_fragment_len {
                            expected_fragment_len = 0;
                            Some(std::mem::take(&mut fragments))
                        } else {
                            None
                        }
                    }
                }
                other => panic!("unexpected DRDYNVC data PDU: {other:?}"),
            };

            if let Some(gfx_bytes) = complete {
                let gfx_bytes = if gfx_bytes.starts_with(&[0xe0, 0x04]) {
                    &gfx_bytes[2..]
                } else {
                    &gfx_bytes
                };
                let mut cursor = ReadCursor::new(gfx_bytes);
                pdus.push(GfxPdu::decode(&mut cursor).expect("GFX PDU decodes"));
            }
        }
    }

    GfxPduTrace { pdus }
}

pub(crate) fn ack_frame(
    bridge: &mut GfxDvcBridge,
    frame_id: u32,
    queue_depth: impl Into<QueueDepth>,
) {
    let ack = GfxPdu::FrameAcknowledge(FrameAcknowledgePdu {
        queue_depth: queue_depth.into(),
        frame_id,
        total_frames_decoded: 1,
    });
    let ack = encode_vec(&ack).expect("frame ack encodes");
    let _ = ironrdp_dvc::DvcProcessor::process(bridge, TEST_CHANNEL_ID, &ack)
        .expect("frame ack processes");
}
