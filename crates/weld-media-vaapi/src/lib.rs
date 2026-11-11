//! Linux VA-API codec capability discovery and hardware media workers.
//!
//! FFmpeg and libva values remain inside this crate. Consumers select a
//! truthful capability and exchange only [`weld_media`] values.

mod decoder_pool;
mod diagnostics;
mod ffmpeg;
mod probe;
mod worker;

pub use decoder_pool::{
    DecodePoolLimits, VaapiDecodeCompletion, VaapiDecodeRequest, VaapiDecodeWorker,
    VaapiDecodedFrame,
};
pub use ffmpeg::{
    DecodeConversionTiming, DecodedPacket, EncodedPacket, FfmpegDecoder, FfmpegEncodeDevice,
    FfmpegEncoder, FfmpegVaapiDevice, PendingDecodedFrame, VaapiEncoderSettings,
};
pub use probe::{
    VaapiCapabilities, VaapiEncodeEntrypoint, VaapiEncodeGeometry, VaapiProbeError,
    probe_vaapi_device,
};
pub use weld_media::WorkerSubmitError as VaapiWorkerSubmitError;
pub use weld_vaapi::{VaapiDevice, VaapiDmabuf, VaapiDmabufObject, VaapiDmabufPlane, VppConverter};
pub use worker::{VaapiEncodeCompletion, VaapiEncodeInput, VaapiEncodeRequest, VaapiEncodeWorker};
