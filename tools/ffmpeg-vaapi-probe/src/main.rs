mod ffmpeg_encoder;
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
use ffmpeg_encoder::{FfmpegEncoder, VideoCodec};
use weld_media_vaapi::VaapiDevice;

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
        .unwrap_or_else(|_| "av1".to_owned())
        .parse::<VideoCodec>()?;
    let bitrate_bits = env::var("WELD_FFMPEG_BITRATE_BITS")
        .context("WELD_FFMPEG_BITRATE_BITS is not set")?
        .parse::<u64>()?;
    fs::create_dir_all(output.join("reference"))?;

    let device = VaapiDevice::open(&render_node)?;
    let vpp = device.vpp_converter()?;
    let mut encoder = FfmpegEncoder::new(
        codec,
        &render_node,
        WIDTH,
        HEIGHT,
        FRAMES_PER_SECOND,
        bitrate_bits,
        KEYFRAME_INTERVAL,
    )?;
    let started = Instant::now();
    let mut modifier = None;

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
        encoder.push(source, i64::try_from(sequence)?)?;
    }

    let encoded = encoder.finish()?;
    fs::write(output.join(codec.output_file()), &encoded.payload)?;
    let mut report = BufWriter::new(File::create(output.join("probe.txt"))?);
    writeln!(report, "render-node={}", render_node.display())?;
    writeln!(report, "geometry={WIDTH}x{HEIGHT}")?;
    writeln!(report, "fps={FRAMES_PER_SECOND}")?;
    writeln!(report, "frames={FRAME_COUNT}")?;
    writeln!(report, "bitrate-bits={bitrate_bits}")?;
    writeln!(report, "keyframe-interval={KEYFRAME_INTERVAL}")?;
    writeln!(report, "codec={}", codec.name())?;
    writeln!(report, "stream={}", codec.output_file())?;
    writeln!(report, "input-fourcc=XR24")?;
    writeln!(
        report,
        "input-modifier={:#018x}",
        modifier.context("no frame was encoded")?
    )?;
    writeln!(report, "filter={}", encoded.filter_description)?;
    writeln!(report, "encoder={}", encoded.encoder_name)?;
    writeln!(report, "packets={}", encoded.packet_count)?;
    writeln!(report, "bytes={}", encoded.payload.len())?;
    writeln!(report, "elapsed-ms={}", started.elapsed().as_millis())?;
    write_sample_contract(&mut report, "black", pattern::BLACK_SAMPLE)?;
    write_sample_contract(&mut report, "white", pattern::WHITE_SAMPLE)?;
    write_sample_contract(&mut report, "mid-gray", pattern::MID_GRAY_SAMPLE)?;
    report.flush()?;

    println!("output={}", output.display());
    println!("ffmpeg-bytes={}", encoded.payload.len());
    println!("ffmpeg-packets={}", encoded.packet_count);
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
