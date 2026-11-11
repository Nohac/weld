use std::{error::Error, fmt, path::Path};

use anyhow::{Context, ensure};
use weld_media::VideoCodec;
pub use weld_vaapi::EncodeEntrypoint as VaapiEncodeEntrypoint;
use weld_vaapi::{Profile, VaapiDevice};

/// Queried encoder limits plus Weld's conservative coded-geometry policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VaapiEncodeGeometry {
    codec: VideoCodec,
    entrypoint: VaapiEncodeEntrypoint,
    minimum_width: Option<u32>,
    minimum_height: Option<u32>,
    maximum_width: u32,
    maximum_height: u32,
}

impl VaapiEncodeGeometry {
    pub fn coded_extent(
        self,
        visible_width: u32,
        visible_height: u32,
    ) -> anyhow::Result<(u32, u32)> {
        ensure!(
            visible_width > 0 && visible_height > 0,
            "VA-API encoder visible extent is empty"
        );
        ensure!(
            visible_width <= self.maximum_width && visible_height <= self.maximum_height,
            "visible extent {visible_width}x{visible_height} exceeds VA-API encoder maximum {}x{}",
            self.maximum_width,
            self.maximum_height
        );
        let mut coded_width = self
            .minimum_width
            .map_or(visible_width, |minimum| visible_width.max(minimum));
        let mut coded_height = self
            .minimum_height
            .map_or(visible_height, |minimum| visible_height.max(minimum));
        if self.codec == VideoCodec::Av1 {
            // Mesa can re-raise AV1 padding beyond its checked align-2 bound.
            // Even NV12 picture dimensions avoid that path on both VCN4 and
            // VCN5. This is backend policy, not an AV1 bitstream restriction.
            // pad_vaapi preserves the visible rectangle; see docs/vaapi-workarounds.md.
            coded_width = coded_width
                .checked_next_multiple_of(2)
                .context("AV1 coded width alignment overflow")?;
            coded_height = coded_height
                .checked_next_multiple_of(2)
                .context("AV1 coded height alignment overflow")?;
        }
        ensure!(
            coded_width <= self.maximum_width && coded_height <= self.maximum_height,
            "coded extent {coded_width}x{coded_height} exceeds VA-API encoder maximum {}x{}",
            self.maximum_width,
            self.maximum_height
        );
        Ok((coded_width, coded_height))
    }

    pub const fn entrypoint(self) -> VaapiEncodeEntrypoint {
        self.entrypoint
    }

    pub const fn minimum(self) -> (Option<u32>, Option<u32>) {
        (self.minimum_width, self.minimum_height)
    }

    pub const fn maximum(self) -> (u32, u32) {
        (self.maximum_width, self.maximum_height)
    }
}

/// Hardware capabilities used by Weld's initial FFmpeg VA-API backend.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VaapiCapabilities {
    pub vendor: String,
    pub h264_decode: bool,
    pub h264_encode: Option<VaapiEncodeEntrypoint>,
    pub av1_decode: bool,
    pub av1_encode: Option<VaapiEncodeEntrypoint>,
    pub video_processing: bool,
}

impl VaapiCapabilities {
    pub const fn supports_encode(&self, codec: VideoCodec) -> bool {
        let encoder = match codec {
            VideoCodec::H264 => self.h264_encode.is_some(),
            VideoCodec::Av1 => self.av1_encode.is_some(),
            VideoCodec::Vp9 => false,
        };
        encoder && self.video_processing
    }
    pub const fn supports_decode(&self, codec: VideoCodec) -> bool {
        let decoder = match codec {
            VideoCodec::H264 => self.h264_decode,
            VideoCodec::Av1 => self.av1_decode,
            VideoCodec::Vp9 => false,
        };
        decoder && self.video_processing
    }

    pub const fn supports_round_trip(&self, codec: VideoCodec) -> bool {
        let codec = match codec {
            VideoCodec::H264 => self.h264_decode && self.h264_encode.is_some(),
            VideoCodec::Av1 => self.av1_decode && self.av1_encode.is_some(),
            VideoCodec::Vp9 => false,
        };
        codec && self.video_processing
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
    let device = VaapiDevice::open(render_node.as_ref()).map_err(|error| {
        VaapiProbeError::Environment(format!(
            "could not open {}: {error:#}",
            render_node.as_ref().display()
        ))
    })?;
    let query = || -> anyhow::Result<_> {
        Ok((
            device.vendor()?,
            device.profile(Profile::H264ConstrainedBaseline)?,
            device.profile(Profile::Av1Main)?,
            device.supports_processing()?,
        ))
    };
    let (vendor, h264, av1, video_processing) =
        query().map_err(|error| VaapiProbeError::Operation(format!("{error:#}")))?;
    Ok(VaapiCapabilities {
        vendor,
        h264_decode: h264.decode,
        h264_encode: h264.encode,
        av1_decode: av1.decode,
        av1_encode: av1.encode,
        video_processing,
    })
}

pub(crate) fn query_encode_geometry(
    device: &VaapiDevice,
    codec: VideoCodec,
) -> anyhow::Result<VaapiEncodeGeometry> {
    let profile = match codec {
        VideoCodec::H264 => Profile::H264ConstrainedBaseline,
        VideoCodec::Av1 => Profile::Av1Main,
        VideoCodec::Vp9 => anyhow::bail!("VP9 VA-API encoding is not implemented"),
    };
    let entrypoint = device
        .profile(profile)?
        .encode
        .context("VA-API exposes no supported encode entrypoint")?;
    let limits = device.encode_limits(profile, entrypoint)?;
    let mut minimum_width = limits.minimum_width;
    let mut minimum_height = limits.minimum_height;
    let maximum_width = limits.maximum_width.unwrap_or(i32::MAX as u32);
    let maximum_height = limits.maximum_height.unwrap_or(i32::MAX as u32);
    if codec == VideoCodec::Av1 {
        minimum_width.get_or_insert(128);
        minimum_height.get_or_insert(128);
    }
    if let Some(minimum) = minimum_width {
        ensure!(
            minimum <= maximum_width,
            "VA-API encoder minimum width exceeds its maximum"
        );
    }
    if let Some(minimum) = minimum_height {
        ensure!(
            minimum <= maximum_height,
            "VA-API encoder minimum height exceeds its maximum"
        );
    }
    Ok(VaapiEncodeGeometry {
        codec,
        entrypoint,
        minimum_width,
        minimum_height,
        maximum_width,
        maximum_height,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_geometry_pads_to_minimum_and_rejects_maximum() {
        let geometry = VaapiEncodeGeometry {
            codec: VideoCodec::Av1,
            entrypoint: VaapiEncodeEntrypoint::Slice,
            minimum_width: Some(128),
            minimum_height: Some(128),
            maximum_width: 8192,
            maximum_height: 4352,
        };
        assert_eq!(
            geometry.coded_extent(192, 64).expect("small popup"),
            (192, 128)
        );
        assert_eq!(
            geometry.coded_extent(944, 484).expect("ordinary window"),
            (944, 484)
        );
        assert!(geometry.coded_extent(8193, 484).is_err());
    }

    #[test]
    fn absent_minimum_preserves_visible_extent() {
        let geometry = VaapiEncodeGeometry {
            codec: VideoCodec::H264,
            entrypoint: VaapiEncodeEntrypoint::Slice,
            minimum_width: None,
            minimum_height: None,
            maximum_width: 4096,
            maximum_height: 4096,
        };
        assert_eq!(
            geometry.coded_extent(7, 5).expect("unrestricted minimum"),
            (7, 5)
        );
    }

    fn av1_geometry() -> VaapiEncodeGeometry {
        VaapiEncodeGeometry {
            codec: VideoCodec::Av1,
            entrypoint: VaapiEncodeEntrypoint::Slice,
            minimum_width: Some(128),
            minimum_height: Some(128),
            maximum_width: 8192,
            maximum_height: 4352,
        }
    }

    #[test]
    fn av1_coded_geometry_rounds_odd_dimensions_without_changing_even_ones() {
        for (visible, expected) in [
            ((1280, 833), (1280, 834)),
            ((1281, 833), (1282, 834)),
            ((960, 637), (960, 638)),
            ((1280, 834), (1280, 834)),
            ((192, 64), (192, 128)),
            ((8192, 4352), (8192, 4352)),
        ] {
            assert_eq!(
                av1_geometry()
                    .coded_extent(visible.0, visible.1)
                    .expect("extent"),
                expected,
                "visible={visible:?}"
            );
        }
    }

    #[test]
    fn av1_alignment_applies_after_minimum_and_rechecks_maximum() {
        let geometry = VaapiEncodeGeometry {
            minimum_width: Some(129),
            minimum_height: Some(131),
            ..av1_geometry()
        };
        assert_eq!(geometry.coded_extent(1, 1).expect("minimum"), (130, 132));
        for geometry in [
            VaapiEncodeGeometry {
                maximum_width: 1281,
                ..av1_geometry()
            },
            VaapiEncodeGeometry {
                maximum_height: 833,
                ..av1_geometry()
            },
        ] {
            assert!(geometry.coded_extent(1281, 833).is_err());
        }
        assert!(geometry.coded_extent(0, 1).is_err());
        assert!(geometry.coded_extent(1, 0).is_err());
    }

    #[test]
    fn av1_alignment_rejects_overflow_in_either_dimension() {
        let geometry = VaapiEncodeGeometry {
            maximum_width: u32::MAX,
            maximum_height: u32::MAX,
            ..av1_geometry()
        };
        assert!(geometry.coded_extent(u32::MAX, 128).is_err());
        assert!(geometry.coded_extent(128, u32::MAX).is_err());
    }

    #[test]
    fn even_av1_pictures_stay_within_vcn_padding_bounds() {
        for dimension in 128..=1024 {
            let (width, height) = av1_geometry()
                .coded_extent(dimension, dimension)
                .expect("bounded extent");
            for (width_alignment, height_alignment) in [(64, 16), (8, 2)] {
                assert!(width.next_multiple_of(width_alignment) - width <= width_alignment - 2);
                assert!(height.next_multiple_of(height_alignment) - height <= height_alignment - 2);
            }
            // The older VCN special case for h % 16 == 8 uses h+2 instead;
            // its padding is also within the bound. This is not a Mesa emulator.
            let legacy_height = if height % 16 == 8 {
                height + 2
            } else {
                height.next_multiple_of(16)
            };
            assert!(legacy_height - height <= 14);
        }
    }

    #[test]
    fn decode_support_requires_the_codec_and_video_processing() {
        let mut capabilities = VaapiCapabilities {
            vendor: "test".to_owned(),
            h264_decode: true,
            h264_encode: None,
            av1_decode: false,
            av1_encode: None,
            video_processing: true,
        };
        assert!(capabilities.supports_decode(VideoCodec::H264));
        assert!(!capabilities.supports_decode(VideoCodec::Av1));
        capabilities.video_processing = false;
        assert!(!capabilities.supports_decode(VideoCodec::H264));
    }

    #[test]
    fn encoder_support_does_not_require_a_decoder_but_does_require_vpp() {
        let mut capabilities = VaapiCapabilities {
            vendor: "test".to_owned(),
            h264_decode: false,
            av1_decode: false,
            h264_encode: Some(VaapiEncodeEntrypoint::Slice),
            av1_encode: Some(VaapiEncodeEntrypoint::LowPowerSlice),
            video_processing: true,
        };
        for codec in [VideoCodec::H264, VideoCodec::Av1] {
            assert!(capabilities.supports_encode(codec));
            assert!(!capabilities.supports_decode(codec));
        }
        assert!(!capabilities.supports_encode(VideoCodec::Vp9));
        capabilities.video_processing = false;
        assert!(!capabilities.supports_encode(VideoCodec::Av1));
    }
}
