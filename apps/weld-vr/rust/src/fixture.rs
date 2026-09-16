//! Finite local IVF fixture, not a transport framing or playback API.

use anyhow::{Context, Result, ensure};
use weld_media::{DecoderConfig, VideoCodec};

pub struct Clip<'a> {
    pub config: DecoderConfig,
    pub frames: Vec<Frame<'a>>,
    pub rate: u64,
    pub scale: u64,
}
pub struct Frame<'a> {
    pub timestamp: u64,
    pub bytes: &'a [u8],
}

pub fn parse(bytes: &[u8]) -> Result<Clip<'_>> {
    ensure!(
        bytes.len() >= 32 && bytes.len() <= 16 * 1024 * 1024,
        "invalid fixture length"
    );
    ensure!(
        &bytes[..4] == b"DKIF" && &bytes[8..12] == b"AV01",
        "expected AV1 IVF fixture"
    );
    ensure!(
        read16(bytes, 4)? == 0 && read16(bytes, 6)? == 32,
        "unsupported IVF header"
    );
    let config = DecoderConfig::new(
        VideoCodec::Av1,
        u32::from(read16(bytes, 12)?),
        u32::from(read16(bytes, 14)?),
        vec![],
    )?;
    let rate = u64::from(read32(bytes, 16)?);
    let scale = u64::from(read32(bytes, 20)?);
    ensure!(rate > 0 && scale > 0, "invalid fixture time base");
    let expected = usize::try_from(read32(bytes, 24)?)?;
    ensure!(
        (1..=120).contains(&expected),
        "fixture frame count must be 1..=120"
    );
    let mut frames = Vec::with_capacity(expected);
    let mut offset = 32usize;
    while offset < bytes.len() {
        ensure!(frames.len() < expected, "extra fixture frame");
        let length = usize::try_from(read32(bytes, offset)?)?;
        ensure!(
            length > 0 && length <= 1024 * 1024,
            "invalid fixture frame length"
        );
        let pts = u64::from_le_bytes(
            bytes
                .get(offset + 4..offset + 12)
                .context("truncated fixture timestamp")?
                .try_into()?,
        );
        let timestamp = pts
            .checked_mul(scale)
            .and_then(|v| v.checked_mul(1_000_000))
            .context("fixture time overflow")?
            / rate;
        if let Some(previous) = frames.last() {
            let previous: &Frame<'_> = previous;
            ensure!(
                timestamp > previous.timestamp,
                "non-increasing fixture timestamp"
            );
        }
        ensure!(timestamp <= 10_000_000, "fixture exceeds ten seconds");
        offset += 12;
        let end = offset
            .checked_add(length)
            .context("fixture length overflow")?;
        frames.push(Frame {
            timestamp,
            bytes: bytes
                .get(offset..end)
                .context("truncated fixture payload")?,
        });
        offset = end;
    }
    ensure!(frames.len() == expected, "missing fixture frames");
    Ok(Clip {
        config,
        frames,
        rate,
        scale,
    })
}

fn read16(bytes: &[u8], offset: usize) -> Result<u16> {
    Ok(u16::from_le_bytes(
        bytes
            .get(offset..offset + 2)
            .context("truncated IVF")?
            .try_into()?,
    ))
}
fn read32(bytes: &[u8], offset: usize) -> Result<u32> {
    Ok(u32::from_le_bytes(
        bytes
            .get(offset..offset + 4)
            .context("truncated IVF")?
            .try_into()?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cargo_generated_fixture_is_complete() {
        let clip = parse(include_bytes!(concat!(env!("OUT_DIR"), "/panel-av1.ivf"))).unwrap();
        assert_eq!(clip.config.extent(), (320, 180));
        assert_eq!(clip.frames.len(), 120);
        assert_eq!(clip.frames[0].timestamp, 0);
        assert_eq!(clip.frames[119].timestamp, 3_966_666);
    }

    fn sample() -> Vec<u8> {
        let mut bytes = vec![0; 32];
        bytes[..4].copy_from_slice(b"DKIF");
        bytes[6] = 32;
        bytes[8..12].copy_from_slice(b"AV01");
        bytes[12..14].copy_from_slice(&320u16.to_le_bytes());
        bytes[14..16].copy_from_slice(&180u16.to_le_bytes());
        bytes[16] = 30;
        bytes[20] = 1;
        bytes[24] = 2;
        for pts in [0u64, 1] {
            bytes.extend(1u32.to_le_bytes());
            bytes.extend(pts.to_le_bytes());
            bytes.push(42);
        }
        bytes
    }
    #[test]
    fn finite_frames_preserve_payload_and_timestamps() {
        let bytes = sample();
        let clip = parse(&bytes).unwrap();
        assert_eq!(clip.config.extent(), (320, 180));
        assert_eq!((clip.rate, clip.scale), (30, 1));
        assert_eq!(clip.frames[1].timestamp, 33_333);
        assert_eq!(clip.frames[0].bytes, &[42]);
    }
    #[test]
    fn malformed_and_unbounded_fixtures_fail() {
        for end in [0, 20, 31, 35, 44, 57] {
            assert!(parse(&sample()[..end]).is_err());
        }
        let mut bytes = sample();
        bytes[24] = 121;
        assert!(parse(&bytes).is_err());
        let mut bytes = sample();
        bytes[16] = 0;
        assert!(parse(&bytes).is_err());
        let mut bytes = sample();
        bytes[49..57].fill(0);
        assert!(parse(&bytes).is_err());
    }
}
