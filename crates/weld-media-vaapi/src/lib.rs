//! Linux VA-API codec capability discovery and hardware media workers.
//!
//! All libva and cros-codecs values remain inside this crate. Consumers select
//! a truthful capability and exchange only [`weld_media`] values.

mod dmabuf;
mod h264;
mod probe;
mod vpp;

pub use dmabuf::{VaapiDmabuf, VaapiDmabufObject, VaapiDmabufPlane};
pub use h264::{H264Encoder, decode_h264_frame};
pub use probe::{H264EncodeEntrypoint, VaapiCapabilities, VaapiProbeError, probe_vaapi_device};
#[cfg(feature = "diagnostic")]
pub use vpp::create_xrgb_probe_frame;
pub use vpp::{VppConverter, VppOutput};
