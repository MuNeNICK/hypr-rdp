mod avc420;
mod avc444;
mod backend;
mod clearcodec;
mod factory;
mod frame;
mod h264;
mod rdpegfx;
mod shared;
#[cfg(feature = "vaapi")]
mod vaapi;
#[cfg(feature = "vaapi")]
mod vaapi_sys;
#[cfg(feature = "vaapi")]
mod vpp;

#[cfg(test)]
pub(crate) mod test_support;

#[cfg(test)]
pub(crate) use avc420::avc420_full_frame_region;
pub(crate) use avc444::{avc444_dimensions_supported, Avc444FrameEncoding};
pub use backend::{FrameEncoder, H264BackendPolicy, H264RateControl};
pub use factory::HyprGfxFactory;
pub(crate) use frame::{EgfxFrameCodec, EncodedEgfxFrame, EncodedFrameState};
#[cfg(feature = "vaapi")]
pub(crate) use h264::extract_sps_pps;
pub(crate) use rdpegfx::EgfxFrameSession;
#[cfg(test)]
pub(crate) use shared::GFX_READY_GRACE;
pub use shared::{EgfxCodecPolicy, EgfxShared, DEFAULT_MAX_FRAMES_IN_FLIGHT};
pub(crate) use shared::{EgfxFrameFlowSnapshot, EgfxFrameReadiness};
#[cfg(feature = "vaapi")]
pub(crate) use vpp::{VppConverter, VppDmaBufInfo};

#[cfg(test)]
mod tests;
