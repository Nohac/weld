use crate::VideoCodec;
use anyhow::{Context, Result, ensure};

const MAX_CONFIGURATION_BYTES: usize = 1024 * 1024;

/// Extract all SPS/PPS NAL units from the first H.264 Annex B access unit.
///
/// MediaCodec needs these before opening; in-band-only decoders need not use
/// this. Original start codes and escaped payloads are preserved. This checks
/// framing and allocation bounds, not SPS/PPS semantics (the codec validates
/// those). Missing/truncated parameter sets and oversized output are errors.
pub fn h264_annex_b_headers(access_unit: &[u8]) -> Result<Vec<u8>> {
    let mut starts = access_unit
        .windows(3)
        .enumerate()
        .filter(|(_, bytes)| *bytes == [0, 0, 1])
        .map(|(index, _)| {
            let prefix = if index > 0 && access_unit[index - 1] == 0 {
                index - 1
            } else {
                index
            };
            (prefix, index + 3)
        })
        .peekable();
    let &(first, _) = starts
        .peek()
        .context("first H.264 access unit is not Annex B")?;
    ensure!(
        access_unit[..first].iter().all(|byte| *byte == 0),
        "first H.264 access unit has invalid leading bytes"
    );
    let mut headers = Vec::new();
    let mut found_sps = false;
    let mut found_pps = false;
    while let Some((prefix, header)) = starts.next() {
        let end = starts
            .peek()
            .map_or(access_unit.len(), |(prefix, _)| *prefix);
        let nal = access_unit
            .get(header..end)
            .filter(|nal| !nal.is_empty())
            .context("first H.264 access unit has an empty NAL unit")?;
        ensure!(nal[0] & 0x80 == 0, "invalid H.264 NAL header");
        let kind = nal[0] & 0x1f;
        if matches!(kind, 7 | 8) {
            ensure!(nal.len() > 1, "truncated H.264 parameter set");
            let bytes = &access_unit[prefix..end];
            ensure!(
                bytes.len() <= MAX_CONFIGURATION_BYTES - headers.len(),
                "H.264 codec configuration exceeds 1 MiB"
            );
            headers.extend_from_slice(bytes);
            found_sps |= kind == 7;
            found_pps |= kind == 8;
        }
    }
    ensure!(
        found_sps && found_pps,
        "first H.264 access unit is missing SPS/PPS headers"
    );
    Ok(headers)
}

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
            extra.len() <= MAX_CONFIGURATION_BYTES,
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
    fn h264_configuration_preserves_all_parameter_sets_and_mixed_start_codes() {
        let sps = [0, 0, 0, 1, 0x67, 0x42, 0, 0, 3, 1, 0x80];
        let pps = [0, 0, 1, 0x68, 0x80];
        let packet = [
            &[0, 0, 1, 0x09, 0x10][..],
            &sps,
            &pps,
            &[0, 0, 0, 1, 0x65, 0x80],
            &sps,
            &pps,
        ]
        .concat();
        assert_eq!(
            h264_annex_b_headers(&packet).unwrap(),
            [&sps[..], &pps, &sps, &pps].concat()
        );
    }

    #[test]
    fn h264_configuration_rejects_missing_and_truncated_headers() {
        for packet in [
            &[][..],
            &[0, 0, 1],
            &[0, 0, 1, 0x67],
            &[0, 0, 1, 0x67, 0x80],
            &[0, 0, 1, 0x68, 0x80],
            &[0, 0, 1, 0xe7, 0x80],
            &[1, 0, 0, 1, 0x67, 0x80],
            &[0, 0, 1, 0x67, 0, 0, 1, 0x68, 0x80],
        ] {
            assert!(h264_annex_b_headers(packet).is_err(), "accepted {packet:?}");
        }
    }

    #[test]
    fn h264_configuration_bounds_only_parameter_sets_not_slice_payloads() {
        let headers = [0, 0, 1, 0x67, 0x80, 0, 0, 1, 0x68, 0x80];
        let mut packet = headers.to_vec();
        packet.extend_from_slice(&[0, 0, 1, 0x65]);
        packet.resize(MAX_CONFIGURATION_BYTES + 100, 0x55);
        assert_eq!(h264_annex_b_headers(&packet).unwrap(), headers);
        packet[headers.len() + 3] = 0x67;
        assert!(h264_annex_b_headers(&packet).is_err());
    }

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
