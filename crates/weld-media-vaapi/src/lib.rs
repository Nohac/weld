//! Linux VA-API codec capability discovery and hardware media workers.
//!
//! FFmpeg and libva values remain inside this crate. Consumers select a
//! truthful capability and exchange only [`weld_media`] values.

mod decoder_pool;
mod device;
mod dmabuf;
mod ffmpeg;
#[cfg(feature = "diagnostic")]
mod h264;
mod probe;
mod vpp;
mod worker;

pub use decoder_pool::{
    DecodePoolLimits, VaapiDecodeCompletion, VaapiDecodeRequest, VaapiDecodeWorker,
    VaapiDecodedFrame,
};
pub use device::VaapiDevice;
pub use dmabuf::{VaapiDmabuf, VaapiDmabufObject, VaapiDmabufPlane};
pub use ffmpeg::{
    DecodeConversionTiming, DecodedPacket, EncodedPacket, FfmpegDecoder, FfmpegEncodeDevice,
    FfmpegEncoder, FfmpegVaapiDevice, PendingDecodedFrame, VaapiEncoderSettings,
};
#[cfg(feature = "diagnostic")]
pub use h264::{
    DecodedH264Frame, H264Decoder, H264Encoder, H264EncoderSettings, H264ReferenceMode,
};
pub use probe::{
    VaapiCapabilities, VaapiEncodeEntrypoint, VaapiEncodeGeometry, VaapiProbeError,
    probe_vaapi_device,
};
pub use vpp::{VppConverter, VppOutput};
pub use worker::{
    VaapiEncodeCompletion, VaapiEncodeInput, VaapiEncodeRequest, VaapiEncodeWorker,
    VaapiWorkerSubmitError,
};
