//! Linux VA-API codec capability discovery and hardware media workers.
//!
//! All libva and cros-codecs values remain inside this crate. Consumers select
//! a truthful capability and exchange only [`weld_media`] values.

mod device;
mod dmabuf;
mod h264;
mod probe;
mod vpp;
mod worker;

pub use device::VaapiDevice;
pub use dmabuf::{VaapiDmabuf, VaapiDmabufObject, VaapiDmabufPlane};
pub use h264::{
    DecodedH264Frame, H264Decoder, H264Encoder, H264EncoderSettings, H264ReferenceMode,
};
pub use probe::{H264EncodeEntrypoint, VaapiCapabilities, VaapiProbeError, probe_vaapi_device};
pub use vpp::{VppConverter, VppOutput};
pub use worker::{
    VaapiDecodeCompletion, VaapiDecodeRequest, VaapiDecodeWorker, VaapiDecodedFrame,
    VaapiEncodeCompletion, VaapiEncodeInput, VaapiEncodeRequest, VaapiEncodeWorker,
    VaapiWorkerSubmitError,
};
