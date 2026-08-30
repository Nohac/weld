use std::{error::Error, fmt, path::Path};

use cros_codecs::libva::{
    Display, VA_STATUS_ERROR_UNSUPPORTED_PROFILE, VAEntrypoint, VAProfile, VaError,
};

/// Hardware H.264 entrypoint selected on one VA-API device.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum H264EncodeEntrypoint {
    Slice,
    LowPowerSlice,
}

/// Capabilities needed by the first opaque H.264 round-trip tracer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VaapiCapabilities {
    pub vendor: String,
    pub h264_decode: bool,
    pub h264_encode: Option<H264EncodeEntrypoint>,
    pub video_processing: bool,
}

impl VaapiCapabilities {
    pub const fn supports_h264_round_trip(&self) -> bool {
        self.h264_decode && self.h264_encode.is_some() && self.video_processing
    }
}

/// Failure category used to keep environment errors distinct from unsupported hardware.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum VaapiProbeError {
    Environment(String),
    Operation(String),
}

impl fmt::Display for VaapiProbeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Environment(message) => {
                write!(formatter, "VA-API environment unavailable: {message}")
            }
            Self::Operation(message) => {
                write!(formatter, "VA-API capability query failed: {message}")
            }
        }
    }
}

impl Error for VaapiProbeError {}

/// Opens the exact DRM render node selected by Weld and queries the first tracer path.
pub fn probe_vaapi_device(
    render_node: impl AsRef<Path>,
) -> Result<VaapiCapabilities, VaapiProbeError> {
    let display = Display::open_drm_display(render_node.as_ref()).map_err(|error| {
        VaapiProbeError::Environment(format!(
            "could not open {}: {error}",
            render_node.as_ref().display()
        ))
    })?;
    let vendor = display
        .query_vendor_string()
        .map_err(|error| VaapiProbeError::Operation(error.to_owned()))?;

    let h264 = supported_entrypoints(&display, VAProfile::VAProfileH264ConstrainedBaseline)?;
    let h264_encode = if h264.contains(&VAEntrypoint::VAEntrypointEncSliceLP) {
        Some(H264EncodeEntrypoint::LowPowerSlice)
    } else if h264.contains(&VAEntrypoint::VAEntrypointEncSlice) {
        Some(H264EncodeEntrypoint::Slice)
    } else {
        None
    };
    let h264_decode = h264.contains(&VAEntrypoint::VAEntrypointVLD);
    let video_processing = supported_entrypoints(&display, VAProfile::VAProfileNone)?
        .contains(&VAEntrypoint::VAEntrypointVideoProc);

    Ok(VaapiCapabilities {
        vendor,
        h264_decode,
        h264_encode,
        video_processing,
    })
}

fn supported_entrypoints(
    display: &Display,
    profile: VAProfile::Type,
) -> Result<Vec<VAEntrypoint::Type>, VaapiProbeError> {
    match display.query_config_entrypoints(profile) {
        Ok(entrypoints) => Ok(entrypoints),
        Err(error) if is_unsupported_profile(&error) => Ok(Vec::new()),
        Err(error) => Err(VaapiProbeError::Operation(error.to_string())),
    }
}

fn is_unsupported_profile(error: &VaError) -> bool {
    error.va_status() as u32 == VA_STATUS_ERROR_UNSUPPORTED_PROFILE
}
