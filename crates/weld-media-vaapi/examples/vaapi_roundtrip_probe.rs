use std::{collections::VecDeque, path::PathBuf, time::Instant};

use anyhow::{Context, Result, ensure};
use weld_media::{EncodedFrameKind, VideoCodec};
use weld_media_vaapi::{
    EncodedPacket, FfmpegDecoder, FfmpegEncodeDevice, FfmpegEncoder, FfmpegVaapiDevice,
    VaapiDevice, VaapiDmabuf, VaapiEncoderSettings, VppConverter,
};

const WIDTH: u32 = 944;
const HEIGHT: u32 = 484;
const FRAME_COUNT: u64 = 8;
const PIXEL_TOLERANCE: u8 = 16;
// Odd-width AV1 pad boundary measured 23 levels total error versus 7-9
// in the interior. Keep an explicit edge bound, not a global relaxation.
// See docs/vaapi-workarounds.md; the responsible stage is not isolated.
const PAD_EDGE_TOLERANCE: u8 = 32;
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
        let mut packets = Vec::new();

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
            packets.push(packet);
        }
        println!(
            "codec={codec:?} A/B: identical pre-encoded packets, depths1,2,2,1 to compare both orders; always-backlogged best case, NOT 60fps pacing or a full benchmark. Depth1 reserves one extra hardware frame, so is NOT the previous binary. Decoder open excluded; frame0 includes startup allocations. Pixel readback/printing excluded from replay time."
        );
        for (pass, depth) in [1, 2, 2, 1].into_iter().enumerate() {
            replay(&packets, codec, pass, depth, &ffmpeg_vaapi, &vpp)?;
        }
    }

    validate_small_av1_popup(&device, &vpp, &ffmpeg_encode, &ffmpeg_vaapi)?;
    validate_encoder_generation_reuse(&device, &ffmpeg_encode)?;
    validate_odd_av1_extents(&device, &vpp, &ffmpeg_encode, &ffmpeg_vaapi)?;
    Ok(())
}

fn validate_odd_av1_extents(
    device: &VaapiDevice,
    vpp: &VppConverter,
    ffmpeg_encode: &FfmpegEncodeDevice,
    ffmpeg_vaapi: &FfmpegVaapiDevice,
) -> Result<()> {
    let settings = VaapiEncoderSettings::try_new(VideoCodec::Av1, 8_000_000, 60, 32)?;
    let geometry = device.encode_geometry(VideoCodec::Av1)?;
    // Height 833 reproduced the reset. Odd-width coverage is deliberately last:
    // it extends validation beyond the original height-only reproduction.
    for (width, height) in [(1280, 833), (960, 637), (1281, 833)] {
        let padded = geometry.coded_extent(width, height)?;
        println!("codec=Av1 odd-extent starting visible={width}x{height} padded={padded:?}");
        let mut encoder = FfmpegEncoder::new(settings, geometry, ffmpeg_encode, width, height)?;
        let mut decoder = FfmpegDecoder::new(VideoCodec::Av1, ffmpeg_vaapi, 1)?;
        for sequence in 0..3_u64 {
            let seed = u8::try_from(23 + sequence * 17)?;
            let source = device.create_xrgb_probe_frame(width, height, vec![0], seed)?;
            let edge_points = [
                (width - 1, height - 1),
                (
                    width - 1 - u32::from(padded.0 > width),
                    height - 1 - u32::from(padded.1 > height),
                ),
                (width / 2, height / 2),
            ];
            let source_samples = vpp.sample_xrgb_bgra(&source, &edge_points)?;
            let timestamp = sequence * 16_667;
            let packet = encoder.encode(source, timestamp)?;
            ensure!(
                packet.timestamp_micros == timestamp,
                "AV1 encode timestamp changed"
            );
            if sequence == 0 {
                ensure!(
                    packet.kind == EncodedFrameKind::Keyframe,
                    "new AV1 context needs keyframe"
                );
            }
            let frame =
                decoder
                    .submit(&packet.payload, timestamp)?
                    .finish(width, height, &[0], vpp)?;
            ensure!(
                frame.timestamp_micros == timestamp,
                "AV1 decode timestamp changed"
            );
            ensure!(
                frame.coded_width >= padded.0 && frame.coded_height >= padded.1,
                "AV1 decoded storage is smaller than the padded picture"
            );
            ensure!(
                (frame.dmabuf.width, frame.dmabuf.height) == (width, height),
                "AV1 output was not cropped to the original visible extent"
            );
            // A gray ramp checks luma/geometry, not chroma fidelity at the pad edge.
            println!(
                "edge diagnostic sequence={sequence} source={source_samples:?} decoded={:?}",
                vpp.sample_xrgb_bgra(&frame.dmabuf, &edge_points)?
            );
            validate_padded_pixels(vpp, &frame.dmabuf, width, height, seed, padded)?;
        }
        println!("codec=Av1 odd-extent passed visible={width}x{height} frames=3 crop+luma=true");
    }
    Ok(())
}

fn replay(
    packets: &[EncodedPacket],
    codec: VideoCodec,
    pass: usize,
    depth: usize,
    device: &FfmpegVaapiDevice,
    vpp: &VppConverter,
) -> Result<()> {
    let mut decoder = FfmpegDecoder::new(codec, device, depth)?;
    let mut pending = VecDeque::new();
    let mut output = Vec::new();
    let replay_started = Instant::now();
    for (sequence, packet) in packets.iter().enumerate() {
        let started = Instant::now();
        let frame = decoder.submit(&packet.payload, packet.timestamp_micros)?;
        let submitted = Instant::now();
        pending.push_back((sequence, frame, started, submitted));
        if pending.len() == depth {
            let (sequence, frame, started, submitted) =
                pending.pop_front().context("pending frame missing")?;
            let finishing = Instant::now();
            let decoded = frame.finish(WIDTH, HEIGHT, &[0], vpp)?;
            output.push((
                sequence,
                decoded,
                submitted.duration_since(started),
                finishing.duration_since(submitted),
                started.elapsed(),
            ));
        }
    }
    // Includes the final prefetched frame, with exactly the same validation.
    for (sequence, frame, started, submitted) in pending {
        let finishing = Instant::now();
        let decoded = frame.finish(WIDTH, HEIGHT, &[0], vpp)?;
        output.push((
            sequence,
            decoded,
            submitted.duration_since(started),
            finishing.duration_since(submitted),
            started.elapsed(),
        ));
    }
    let elapsed = replay_started.elapsed();
    ensure!(
        output.len() == packets.len(),
        "{codec:?} depth{depth} lost frames"
    );
    for (sequence, frame, submission, pending, residence) in output {
        ensure!(
            frame.timestamp_micros == packets[sequence].timestamp_micros,
            "decoder changed timestamp"
        );
        ensure!(
            frame.dmabuf.width == WIDTH && frame.dmabuf.height == HEIGHT,
            "decoder changed visible extent"
        );
        ensure!(
            frame.dmabuf.fourcc == DRM_FORMAT_XRGB8888,
            "decoder VPP output is not XRGB8888"
        );
        validate_pixels(
            vpp,
            &frame.dmabuf,
            WIDTH,
            HEIGHT,
            u8::try_from(sequence * 17)?,
        )?;
        println!(
            "codec={codec:?} pass={pass} depth={depth} frame={sequence} startup={} submission_us={} pending_us={} decode_sync_us={} conversion_setup_us={} conversion_sync_us={} residence_us={} pixels_valid=true",
            sequence == 0,
            submission.as_micros(),
            pending.as_micros(),
            frame.timing.decode_sync.as_micros(),
            frame.timing.conversion_setup.as_micros(),
            frame.timing.conversion_sync.as_micros(),
            residence.as_micros()
        );
    }
    println!(
        "codec={codec:?} pass={pass} depth={depth} decoded_frames={} replay_us={} visible={WIDTH}x{HEIGHT}",
        packets.len(),
        elapsed.as_micros()
    );
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
    let mut decoder = FfmpegDecoder::new(VideoCodec::Av1, ffmpeg_vaapi, 1)?;
    let source = device.create_xrgb_probe_frame(WIDTH, HEIGHT, vec![0], SEED)?;
    let packet = encoder.encode(source, 0)?;
    let frame = decoder
        .submit(&packet.payload, packet.timestamp_micros)?
        .finish(WIDTH, HEIGHT, &[0], vpp)?;
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
        validate_pixel((width, height), seed, (x, y), sample, PIXEL_TOLERANCE)?;
    }
    Ok(())
}

fn validate_padded_pixels(
    vpp: &VppConverter,
    frame: &VaapiDmabuf,
    width: u32,
    height: u32,
    seed: u8,
    padded: (u32, u32),
) -> Result<()> {
    let right_pad = padded.0 > width;
    let bottom_pad = padded.1 > height;
    let mut points = vec![
        (0, 0),
        (width - 1, 0),
        (0, height - 1),
        (width / 2, height / 2),
        (width - 1, height - 1),
    ];
    if right_pad {
        points.push((width - 2, 0));
    }
    if bottom_pad {
        points.push((0, height - 2));
    }
    points.push((
        width - 1 - u32::from(right_pad),
        height - 1 - u32::from(bottom_pad),
    ));
    let samples = vpp.sample_xrgb_bgra(frame, &points)?;
    for (point, sample) in points.into_iter().zip(samples) {
        let at_pad = (right_pad && point.0 == width - 1) || (bottom_pad && point.1 == height - 1);
        let tolerance = if at_pad {
            PAD_EDGE_TOLERANCE
        } else {
            PIXEL_TOLERANCE
        };
        validate_pixel((width, height), seed, point, sample, tolerance)?;
    }
    Ok(())
}

fn validate_pixel(
    extent: (u32, u32),
    seed: u8,
    point: (u32, u32),
    sample: [u8; 4],
    tolerance: u8,
) -> Result<()> {
    let horizontal = (point.0 * 63) / extent.0.saturating_sub(1).max(1);
    let vertical = (point.1 * 63) / extent.1.saturating_sub(1).max(1);
    let expected = u8::try_from(u32::from(seed) + horizontal + vertical)?;
    for channel in sample[..3].iter().copied() {
        ensure!(
            channel.abs_diff(expected) <= tolerance,
            "decoded pixel {point:?} differs from {expected}: {sample:?} (tolerance {tolerance})"
        );
    }
    Ok(())
}
