#[path = "../../gstreamer-vaapi-probe/src/pattern.rs"]
mod pattern;

use std::{
    env,
    fs::{self, File},
    io::{BufWriter, Write},
    path::PathBuf,
    time::Instant,
};

use anyhow::{Context, Result};
use weld_media::VideoCodec;
use weld_media_vaapi::{FfmpegEncodeDevice, FfmpegEncoder, VaapiDevice, VaapiEncoderSettings};

const WIDTH: u32 = 944;
const HEIGHT: u32 = 484;
const FRAMES_PER_SECOND: u32 = 60;
const FRAME_COUNT: u64 = 64;
const KEYFRAME_INTERVAL: u32 = 32;

fn main() -> Result<()> {
    ffmpeg_next::init().context("could not initialize FFmpeg")?;
    // SAFETY: changing FFmpeg's process-wide log threshold accepts any documented AV_LOG value.
    unsafe { ffmpeg_next::ffi::av_log_set_level(ffmpeg_next::ffi::AV_LOG_DEBUG) };
    let output = env::var_os("WELD_FFMPEG_PROBE_OUTPUT")
        .map(PathBuf::from)
        .context("WELD_FFMPEG_PROBE_OUTPUT is not set")?;
    let render_node = env::var_os("WELD_RENDER_NODE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/dev/dri/renderD128"));
    let codec = env::var("WELD_FFMPEG_CODEC")
        .unwrap_or_else(|_| "av1".to_owned());
    let codec = match codec.as_str() {
        "av1" => VideoCodec::Av1,
        "h264" => VideoCodec::H264,
        other => anyhow::bail!("unsupported probe codec {other}; expected av1 or h264"),
    };
    let bitrate_bits = env::var("WELD_FFMPEG_BITRATE_BITS")
        .context("WELD_FFMPEG_BITRATE_BITS is not set")?
        .parse::<u64>()?;
    fs::create_dir_all(output.join("reference"))?;

    let device = VaapiDevice::open(&render_node)?;
    let vpp = device.vpp_converter()?;
    let settings = VaapiEncoderSettings::try_new(
        codec,
        bitrate_bits,
        FRAMES_PER_SECOND,
        KEYFRAME_INTERVAL,
    )?;
    let geometry = device.encode_geometry(codec)?;
    let ffmpeg_encode = FfmpegEncodeDevice::open(&render_node)?;
    let mut encoder = FfmpegEncoder::new(settings, geometry, &ffmpeg_encode, WIDTH, HEIGHT)?;
    let started = Instant::now();
    let mut modifier = None;
    let mut payload = stream_header(codec, WIDTH, HEIGHT, FRAMES_PER_SECOND)?;
    let mut packet_count = 0_u64;

    for sequence in 0..FRAME_COUNT {
        let pixels = pattern::frame(WIDTH, HEIGHT, sequence)?;
        pattern::write_ppm(
            &output
                .join("reference")
                .join(format!("frame-{sequence:03}.ppm")),
            WIDTH,
            HEIGHT,
            &pixels,
        )?;
        let source = vpp.upload_bgra(WIDTH, HEIGHT, &pixels)?;
        modifier.get_or_insert(source.primary_modifier()?);
        let timestamp_micros = sequence
            .checked_mul(16_667)
            .context("probe timestamp overflow")?;
        let packet = encoder.encode(source, timestamp_micros)?;
        append_packet(codec, &mut payload, sequence, &packet.payload)?;
        packet_count = packet_count
            .checked_add(1)
            .context("probe packet count overflow")?;
    }

    finish_stream(codec, &mut payload, packet_count)?;
    fs::write(output.join(output_file(codec)), &payload)?;
    let mut report = BufWriter::new(File::create(output.join("probe.txt"))?);
    writeln!(report, "render-node={}", render_node.display())?;
    writeln!(report, "geometry={WIDTH}x{HEIGHT}")?;
    writeln!(report, "fps={FRAMES_PER_SECOND}")?;
    writeln!(report, "frames={FRAME_COUNT}")?;
    writeln!(report, "bitrate-bits={bitrate_bits}")?;
    writeln!(report, "keyframe-interval={KEYFRAME_INTERVAL}")?;
    writeln!(report, "codec={}", codec_name(codec))?;
    writeln!(report, "stream={}", output_file(codec))?;
    writeln!(report, "input-fourcc=XR24")?;
    writeln!(
        report,
        "input-modifier={:#018x}",
        modifier.context("no frame was encoded")?
    )?;
    writeln!(report, "filter=hwmap+scale_vaapi")?;
    writeln!(report, "encoder={}_vaapi", codec_name(codec))?;
    writeln!(report, "packets={packet_count}")?;
    writeln!(report, "bytes={}", payload.len())?;
    writeln!(report, "elapsed-ms={}", started.elapsed().as_millis())?;
    write_sample_contract(&mut report, "black", pattern::BLACK_SAMPLE)?;
    write_sample_contract(&mut report, "white", pattern::WHITE_SAMPLE)?;
    write_sample_contract(&mut report, "mid-gray", pattern::MID_GRAY_SAMPLE)?;
    report.flush()?;

    println!("output={}", output.display());
    println!("ffmpeg-bytes={}", payload.len());
    println!("ffmpeg-packets={packet_count}");
    Ok(())
}

fn codec_name(codec: VideoCodec) -> &'static str {
    match codec {
        VideoCodec::Av1 => "av1",
        VideoCodec::H264 => "h264",
        VideoCodec::Vp9 => "vp9",
    }
}

fn output_file(codec: VideoCodec) -> &'static str {
    match codec {
        VideoCodec::Av1 => "ffmpeg.ivf",
        VideoCodec::H264 => "ffmpeg.h264",
        VideoCodec::Vp9 => "ffmpeg.vp9",
    }
}

fn stream_header(codec: VideoCodec, width: u32, height: u32, fps: u32) -> Result<Vec<u8>> {
    if codec != VideoCodec::Av1 {
        return Ok(Vec::new());
    }
    let mut header = Vec::with_capacity(32);
    header.extend_from_slice(b"DKIF");
    header.extend_from_slice(&0_u16.to_le_bytes());
    header.extend_from_slice(&32_u16.to_le_bytes());
    header.extend_from_slice(b"AV01");
    header.extend_from_slice(&u16::try_from(width)?.to_le_bytes());
    header.extend_from_slice(&u16::try_from(height)?.to_le_bytes());
    header.extend_from_slice(&fps.to_le_bytes());
    header.extend_from_slice(&1_u32.to_le_bytes());
    header.extend_from_slice(&0_u32.to_le_bytes());
    header.extend_from_slice(&0_u32.to_le_bytes());
    Ok(header)
}

fn append_packet(
    codec: VideoCodec,
    stream: &mut Vec<u8>,
    container_timestamp: u64,
    packet: &[u8],
) -> Result<()> {
    if codec == VideoCodec::Av1 {
        stream.extend_from_slice(&u32::try_from(packet.len())?.to_le_bytes());
        stream.extend_from_slice(&container_timestamp.to_le_bytes());
    }
    stream.extend_from_slice(packet);
    Ok(())
}

fn finish_stream(codec: VideoCodec, stream: &mut [u8], packet_count: u64) -> Result<()> {
    if codec == VideoCodec::Av1 {
        stream
            .get_mut(24..28)
            .context("AV1 IVF header is incomplete")?
            .copy_from_slice(&u32::try_from(packet_count)?.to_le_bytes());
    }
    Ok(())
}

fn write_sample_contract(
    report: &mut impl Write,
    name: &str,
    point: pattern::SamplePoint,
) -> Result<()> {
    writeln!(report, "sample-{name}-x={}", point.x)?;
    writeln!(report, "sample-{name}-y={}", point.y)?;
    writeln!(
        report,
        "sample-{name}-offset={}",
        pattern::sample_offset(WIDTH, point)?
    )?;
    Ok(())
}
