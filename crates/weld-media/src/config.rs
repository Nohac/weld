use crate::VideoCodec;
use anyhow::{Result, ensure};

/// Immutable codec setup, independent of FFmpeg structures and input transport.
#[derive(Clone, Debug)]
pub struct DecoderConfig {
    codec: VideoCodec,
    width: u32,
    height: u32,
    extra: Vec<u8>,
}

impl DecoderConfig {
    /// Validate allocation bounds, not device capability. Native open may still
    /// reject a codec/profile/extent. Sequence headers may be in the first AU.
    pub fn new(codec: VideoCodec, width: u32, height: u32, extra: Vec<u8>) -> Result<Self> {
        ensure!(
            (1..=8192).contains(&width) && (1..=8192).contains(&height),
            "invalid decoder extent"
        );
        ensure!(
            extra.len() <= 1024 * 1024,
            "codec configuration exceeds 1 MiB"
        );
        Ok(Self {
            codec,
            width,
            height,
            extra,
        })
    }

    /// Selected codec; this adapter never silently substitutes another codec.
    pub fn codec(&self) -> VideoCodec {
        self.codec
    }
    /// Initial coded extent; each output image carries its actual storage/crop.
    pub fn extent(&self) -> (u32, u32) {
        (self.width, self.height)
    }
    /// Optional codec initialization bytes.
    pub fn extra(&self) -> &[u8] {
        &self.extra
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configuration_rejects_unbounded_allocations_but_allows_in_band_headers() {
        assert!(DecoderConfig::new(VideoCodec::Av1, 0, 180, vec![]).is_err());
        assert!(DecoderConfig::new(VideoCodec::Av1, 320, 8193, vec![]).is_err());
        assert!(DecoderConfig::new(VideoCodec::Av1, 320, 180, vec![0; 1024 * 1024 + 1]).is_err());
        let config = DecoderConfig::new(VideoCodec::Av1, 320, 180, vec![]).unwrap();
        assert_eq!(config.extent(), (320, 180));
        assert_eq!(config.codec(), VideoCodec::Av1);
        assert!(config.extra().is_empty());
    }
}
