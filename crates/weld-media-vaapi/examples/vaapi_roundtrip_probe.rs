use std::path::PathBuf;

use anyhow::{Context, Result, ensure};
use weld_media::{EncodedFrameKind, VideoCodec};
use weld_media_vaapi::{
    FfmpegDecoder, FfmpegEncodeDevice, FfmpegEncoder, FfmpegVaapiDevice, VaapiDevice, VaapiDmabuf,
    VaapiEncoderSettings, VppConverter,
};

const WIDTH: u32 = 944;
const HEIGHT: u32 = 484;
const FRAME_COUNT: u64 = 8;
const PIXEL_TOLERANCE: u8 = 16;
const DRM_FORMAT_XRGB8888: u32 = u32::from_le_bytes(*b"XR24");

fn main() -> Result<()> {
    let render_node = std::env::var_os("WELD_RENDER_NODE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/dev/dri/renderD128"));
    let device = VaapiDevice::open(&render_node)?;
    let vpp = device.vpp_converter()?;
    let ffmpeg_encode = FfmpegEncodeDevice::open(&render_node)?;
    let ffmpeg_vaapi = FfmpegVaapiDevice::open(&render_node)?;

    for codec in [VideoCodec::H264, VideoCodec::Av1] {
        let bitrate = match codec {
            VideoCodec::H264 => 16_000_000,
            VideoCodec::Av1 => 8_000_000,
            VideoCodec::Vp9 => return Err(anyhow::anyhow!("probe selected unsupported VP9")),
        };
        let settings = VaapiEncoderSettings::try_new(codec, bitrate, 60, 32)?;
        let geometry = device.encode_geometry(codec)?;
        let mut encoder = FfmpegEncoder::new(settings, geometry, &ffmpeg_encode, WIDTH, HEIGHT)?;
        let mut decoder = FfmpegDecoder::new(codec, &ffmpeg_vaapi)?;
        let mut decoded_count = 0;

        for sequence in 0..FRAME_COUNT {
            let source = device.create_xrgb_probe_frame(
                WIDTH,
                HEIGHT,
                vec![0],
                u8::try_from(sequence * 17)?,
            )?;
            let timestamp_micros = sequence
                .checked_mul(16_667)
                .context("probe timestamp overflow")?;
            let packet = encoder.encode(source, timestamp_micros)?;
            ensure!(
                packet.timestamp_micros == timestamp_micros,
                "encoder changed the frame timestamp"
            );
            if sequence == 0 {
                ensure!(
                    packet.kind == EncodedFrameKind::Keyframe,
                    "new codec generation did not begin with a keyframe"
                );
            }
            let decoded = decoder.decode_and_convert(
                &packet.payload,
                packet.timestamp_micros,
                WIDTH,
                HEIGHT,
                &[0],
                &vpp,
            )?;
            ensure!(
                decoded.len() == 1,
                "{codec:?} did not produce exactly one decoded frame per packet"
            );
            let frame = decoded.into_iter().next().context("decoded frame absent")?;
            ensure!(
                frame.timestamp_micros == timestamp_micros,
                "decoder changed the frame timestamp"
            );
            ensure!(
                frame.dmabuf.width == WIDTH && frame.dmabuf.height == HEIGHT,
                "decoder did not crop coded storage to the visible extent"
            );
            ensure!(
                frame.dmabuf.fourcc == DRM_FORMAT_XRGB8888,
                "decoder VPP output is not XRGB8888"
            );
            validate_pixels(
                &vpp,
                &frame.dmabuf,
                WIDTH,
                HEIGHT,
                u8::try_from(sequence * 17)?,
            )?;
            decoded_count += 1;
            println!(
                "codec={codec:?} frame={sequence} kind={:?} bytes={}",
                packet.kind,
                packet.payload.len()
            );
        }
        ensure!(
            decoded_count == FRAME_COUNT,
            "{codec:?} round trip lost frames"
        );
        println!(
            "codec={codec:?} one-packet-per-frame=true decoded-frames={decoded_count} visible={WIDTH}x{HEIGHT}"
        );
    }

    validate_small_av1_popup(&device, &vpp, &ffmpeg_encode, &ffmpeg_vaapi)?;
    validate_encoder_generation_reuse(&device, &ffmpeg_encode)?;
    Ok(())
}

fn validate_encoder_generation_reuse(
    device: &VaapiDevice,
    ffmpeg_encode: &FfmpegEncodeDevice,
) -> Result<()> {
    const GENERATIONS: u8 = 16;
    const WIDTH: u32 = 192;
    const HEIGHT: u32 = 64;

    let settings = VaapiEncoderSettings::try_new(VideoCodec::Av1, 8_000_000, 60, 32)?;
    let geometry = device.encode_geometry(VideoCodec::Av1)?;
    let descriptors_before = open_descriptor_count()?;
    for generation in 0..GENERATIONS {
        let source =
            device.create_xrgb_probe_frame(WIDTH, HEIGHT, vec![0], generation.saturating_add(1))?;
        let mut encoder = FfmpegEncoder::new(settings, geometry, ffmpeg_encode, WIDTH, HEIGHT)?;
        let packet = encoder.encode(source, u64::from(generation) * 16_667)?;
        ensure!(
            !packet.payload.is_empty(),
            "generation probe emitted no bytes"
        );
    }
    let descriptors_after = open_descriptor_count()?;
    ensure!(
        descriptors_after == descriptors_before,
        "encoder generation rotation changed open descriptor count from {descriptors_before} to {descriptors_after}"
    );
    println!(
        "encoder-device-reused=true generations={GENERATIONS} open-descriptors={descriptors_after}"
    );
    Ok(())
}

fn open_descriptor_count() -> Result<usize> {
    Ok(std::fs::read_dir("/proc/self/fd")?.count())
}

fn validate_small_av1_popup(
    device: &VaapiDevice,
    vpp: &VppConverter,
    ffmpeg_encode: &FfmpegEncodeDevice,
    ffmpeg_vaapi: &FfmpegVaapiDevice,
) -> Result<()> {
    const WIDTH: u32 = 192;
    const HEIGHT: u32 = 64;
    const SEED: u8 = 23;

    let settings = VaapiEncoderSettings::try_new(VideoCodec::Av1, 8_000_000, 60, 32)?;
    let geometry = device.encode_geometry(VideoCodec::Av1)?;
    let mut encoder = FfmpegEncoder::new(settings, geometry, ffmpeg_encode, WIDTH, HEIGHT)?;
    let mut decoder = FfmpegDecoder::new(VideoCodec::Av1, ffmpeg_vaapi)?;
    let source = device.create_xrgb_probe_frame(WIDTH, HEIGHT, vec![0], SEED)?;
    let packet = encoder.encode(source, 0)?;
    let decoded = decoder.decode_and_convert(
        &packet.payload,
        packet.timestamp_micros,
        WIDTH,
        HEIGHT,
        &[0],
        vpp,
    )?;
    ensure!(
        decoded.len() == 1,
        "small AV1 popup did not produce exactly one decoded frame"
    );
    let frame = decoded.into_iter().next().context("decoded frame absent")?;
    ensure!(
        frame.coded_width == WIDTH && frame.coded_height == 128,
        "small AV1 popup was not padded to the expected hardware coded extent"
    );
    ensure!(
        frame.dmabuf.width == WIDTH && frame.dmabuf.height == HEIGHT,
        "small AV1 popup was not cropped back to its visible extent"
    );
    validate_pixels(vpp, &frame.dmabuf, WIDTH, HEIGHT, SEED)?;
    println!(
        "codec=Av1 small-popup=true coded={}x{} visible={WIDTH}x{HEIGHT}",
        frame.coded_width, frame.coded_height
    );
    Ok(())
}

fn validate_pixels(
    vpp: &VppConverter,
    frame: &VaapiDmabuf,
    width: u32,
    height: u32,
    seed: u8,
) -> Result<()> {
    let points = [
        (0, 0),
        (width - 1, 0),
        (0, height - 1),
        (width / 2, height / 2),
        (width - 1, height - 1),
    ];
    let samples = vpp.sample_xrgb_bgra(frame, &points)?;
    for (&(x, y), sample) in points.iter().zip(samples) {
        let horizontal = (x * 63) / width.saturating_sub(1).max(1);
        let vertical = (y * 63) / height.saturating_sub(1).max(1);
        let expected = u8::try_from(u32::from(seed) + horizontal + vertical)?;
        for channel in sample[..3].iter().copied() {
            ensure!(
                channel.abs_diff(expected) <= PIXEL_TOLERANCE,
                "decoded pixel ({x}, {y}) differs from {expected}: {sample:?}"
            );
        }
    }
    Ok(())
}
