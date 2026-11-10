//! Opt-in offscreen validation using the production FFmpeg encoder and decoder.

use super::*;
use crate::VaapiDevice;
use std::{fs, path::PathBuf};

#[test]
#[ignore = "requires an explicitly selected VA-API render node; no display server"]
fn ffmpeg_upload_decode_mapping_preserves_pixels_modifiers_and_lifetimes() -> Result<()> {
    let render_node = PathBuf::from(
        std::env::var_os("WELD_TEST_RENDER_NODE")
            .context("set WELD_TEST_RENDER_NODE to the test render node")?,
    );
    let artifacts = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/validation/ffmpeg-consolidation");
    fs::create_dir_all(&artifacts)?;
    let device = VaapiDevice::open(&render_node)?;
    let vpp = device.vpp_converter()?;
    let encoding = FfmpegEncodeDevice::open(&render_node)?;
    let decoding = FfmpegVaapiDevice::open(&render_node)?;
    let (width, height) = (98, 74);
    for codec in [VideoCodec::H264, VideoCodec::Av1] {
        let geometry = device.encode_geometry(codec)?;
        let settings = VaapiEncoderSettings::try_new(codec, 2_000_000, 60, 32)?;
        let mut encoder = FfmpegEncoder::new(settings, geometry, &encoding, width, height)?;
        let mut decoder = FfmpegDecoder::new(codec, &decoding, 2)?;
        let mut reference_encoder =
            FfmpegEncoder::new(settings, geometry, &encoding, width, height)?;
        let mut reference_decoder = FfmpegDecoder::new(codec, &decoding, 2)?;
        let mut retained = Vec::new();
        for (sequence, bgra) in [[32u8, 64, 192, 255], [64, 192, 32, 255], [192, 32, 64, 255]]
            .into_iter()
            .enumerate()
        {
            let pixels: Vec<u8> = (0..width * height)
                .flat_map(|index| {
                    [
                        bgra[0].saturating_add((index % width / 4) as u8),
                        bgra[1].saturating_add((index / width / 4) as u8),
                        bgra[2],
                        bgra[3],
                    ]
                })
                .collect();
            let pts = 1_000_000 + sequence as u64 * 16_667;
            let packet = encoder.encode_bgra(width, height, pixels.clone(), pts)?;
            assert_eq!(packet.timestamp_micros, pts);
            let pending = decoder.submit(&packet.payload, pts)?;
            let frame = pending.finish(width, height, &[0], &vpp)?;
            assert_eq!(frame.timestamp_micros, pts);
            assert_eq!((frame.dmabuf.width, frame.dmabuf.height), (width, height));
            assert_eq!(frame.dmabuf.fourcc, u32::from_le_bytes(*b"XR24"));
            assert!(
                frame
                    .dmabuf
                    .objects
                    .iter()
                    .all(|object| object.modifier == 0)
            );
            assert!(frame.coded_width >= width && frame.coded_height >= height);
            let reference =
                reference_encoder.encode(vpp.upload_bgra(width, height, &pixels)?, pts)?;
            let reference = reference_decoder.submit(&reference.payload, pts)?.finish(
                width,
                height,
                &[0],
                &vpp,
            )?;
            retained.push((frame.dmabuf, reference.dmabuf, bgra));
        }
        drop(encoder);
        drop(decoder);
        drop(reference_encoder);
        drop(reference_decoder);
        // Earlier outputs remain readable after subsequent submissions and codec retirement.
        for (sequence, (frame, reference, bgra)) in retained.iter().enumerate() {
            let path = artifacts.join(format!("{codec:?}-{sequence}.ppm"));
            vpp.write_xrgb_ppm(frame, width, height, &path)?;
            let bytes = fs::read(path)?;
            let reference_path = artifacts.join(format!("{codec:?}-{sequence}-reference.ppm"));
            vpp.write_xrgb_ppm(reference, width, height, &reference_path)?;
            let reference_bytes = fs::read(reference_path)?;
            let header = format!("P6\n{width} {height}\n255\n");
            assert!(bytes.starts_with(header.as_bytes()));
            assert!(reference_bytes.starts_with(header.as_bytes()));
            assert_eq!(bytes.len(), header.len() + (width * height * 3) as usize);
            assert_eq!(bytes.len(), reference_bytes.len());
            let maximum_difference = bytes[header.len()..]
                .iter()
                .zip(&reference_bytes[header.len()..])
                .map(|(a, b)| a.abs_diff(*b))
                .max()
                .context("empty pixel comparison")?;
            assert!(
                maximum_difference <= 3,
                "{codec:?} frame {sequence}: maximum channel difference {maximum_difference}; source={bgra:?}"
            );
        }
    }
    Ok(())
}
