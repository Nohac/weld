mod gstreamer_encoder;
mod pattern;

use std::{
    env,
    fs::{self, File},
    io::{BufWriter, Write},
    path::PathBuf,
    time::Instant,
};

use anyhow::{Context, Result};
use gstreamer_encoder::{GstreamerEncoder, supported_xrgb_modifiers};
use weld_media::{MediaFrameId, MediaStreamId, StreamGeneration};
use weld_media_vaapi::{H264EncoderSettings, H264ReferenceMode, VaapiDevice, VppOutput};

const WIDTH: u32 = 944;
const HEIGHT: u32 = 484;
const FRAMES_PER_SECOND: u32 = 60;
const FRAME_COUNT: u64 = 64;
const BITRATE_BITS: u64 = 64_000_000;
const KEYFRAME_INTERVAL: u16 = 32;

fn main() -> Result<()> {
    let output = env::var_os("WELD_GSTREAMER_PROBE_OUTPUT")
        .map(PathBuf::from)
        .context("WELD_GSTREAMER_PROBE_OUTPUT is not set")?;
    let render_node = env::var_os("WELD_RENDER_NODE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/dev/dri/renderD128"));

    fs::create_dir_all(output.join("reference")).with_context(|| {
        format!(
            "could not create probe output directory {}",
            output.display()
        )
    })?;

    println!("output={}", output.display());
    println!("render-node={}", render_node.display());
    println!(
        "geometry={}x{} fps={} frames={}",
        WIDTH, HEIGHT, FRAMES_PER_SECOND, FRAME_COUNT
    );

    let device = VaapiDevice::open(&render_node)?;
    let vpp = device.vpp_converter()?;
    let settings = H264EncoderSettings::try_new(
        BITRATE_BITS,
        FRAMES_PER_SECOND,
        KEYFRAME_INTERVAL,
        18,
        36,
        H264ReferenceMode::LowDelay,
    )?;
    let mut cros_encoder = device.h264_encoder(WIDTH, HEIGHT, settings)?;
    let coded_size = cros_encoder.coded_size();
    let mut cros_payload = Vec::new();
    let cros_started = Instant::now();
    let mut gstreamer_encoder = None;
    let gstreamer_modifiers = supported_xrgb_modifiers(&render_node)?;

    for sequence in 0..FRAME_COUNT {
        let pixels = pattern::frame(WIDTH, HEIGHT, sequence)?;
        let reference_path = output
            .join("reference")
            .join(format!("frame-{sequence:03}.ppm"));
        pattern::write_ppm(&reference_path, WIDTH, HEIGHT, &pixels)?;

        let source =
            vpp.upload_bgra_with_modifiers(WIDTH, HEIGHT, &pixels, gstreamer_modifiers.clone())?;
        if gstreamer_encoder.is_none() {
            gstreamer_encoder = Some(GstreamerEncoder::new(
                &render_node,
                WIDTH,
                HEIGHT,
                FRAMES_PER_SECOND,
                source.primary_modifier()?,
            )?);
        }
        gstreamer_encoder
            .as_ref()
            .context("GStreamer encoder was not initialized")?
            .push(source.try_clone()?, sequence, FRAMES_PER_SECOND)?;

        let nv12 = vpp.convert_padded(
            &source,
            WIDTH,
            HEIGHT,
            coded_size.0,
            coded_size.1,
            VppOutput::Nv12,
        )?;
        let timestamp_micros = sequence
            .checked_mul(1_000_000)
            .and_then(|value| value.checked_div(u64::from(FRAMES_PER_SECOND)))
            .context("cros-codecs probe timestamp overflow")?;
        let access_unit = cros_encoder.encode(
            MediaFrameId::new(MediaStreamId::new(1), StreamGeneration::new(1), sequence),
            timestamp_micros,
            &nv12,
        )?;
        cros_payload.extend_from_slice(&access_unit.payload);
    }

    let cros_elapsed = cros_started.elapsed();
    let gstreamer_output = gstreamer_encoder
        .context("probe generated no GStreamer input")?
        .finish()?;
    fs::write(output.join("cros.h264"), &cros_payload)
        .context("could not write cros-codecs H.264 stream")?;
    fs::write(output.join("gstreamer.h264"), &gstreamer_output.payload)
        .context("could not write GStreamer H.264 stream")?;

    let report_path = output.join("probe.txt");
    let mut report = BufWriter::new(
        File::create(&report_path)
            .with_context(|| format!("could not create {}", report_path.display()))?,
    );
    writeln!(report, "render-node={}", render_node.display())?;
    writeln!(report, "geometry={}x{}", WIDTH, HEIGHT)?;
    writeln!(report, "coded-geometry={}x{}", coded_size.0, coded_size.1)?;
    writeln!(report, "fps={FRAMES_PER_SECOND}")?;
    writeln!(report, "frames={FRAME_COUNT}")?;
    write_sample_contract(&mut report, "black", pattern::BLACK_SAMPLE)?;
    write_sample_contract(&mut report, "white", pattern::WHITE_SAMPLE)?;
    write_sample_contract(&mut report, "mid-gray", pattern::MID_GRAY_SAMPLE)?;
    writeln!(report, "bitrate-bits={BITRATE_BITS}")?;
    writeln!(report, "keyframe-interval={KEYFRAME_INTERVAL}")?;
    writeln!(report, "cros-bytes={}", cros_payload.len())?;
    writeln!(report, "cros-elapsed-ms={}", cros_elapsed.as_millis())?;
    writeln!(report, "gstreamer-bytes={}", gstreamer_output.payload.len())?;
    writeln!(
        report,
        "gstreamer-packets={}",
        gstreamer_output.packet_count
    )?;
    writeln!(
        report,
        "gstreamer-elapsed-ms={}",
        gstreamer_output.elapsed.as_millis()
    )?;
    writeln!(
        report,
        "gstreamer-requested-input-caps={}",
        gstreamer_output.requested_input_caps
    )?;
    writeln!(
        report,
        "gstreamer-negotiated-input-caps={}",
        gstreamer_output.negotiated_input_caps
    )?;
    writeln!(
        report,
        "gstreamer-postprocess-caps={}",
        gstreamer_output.postprocess_caps
    )?;
    writeln!(
        report,
        "gstreamer-encoded-caps={}",
        gstreamer_output.encoded_caps
    )?;
    writeln!(
        report,
        "gstreamer-postprocess-factory={}",
        gstreamer_output.postprocess_factory
    )?;
    writeln!(
        report,
        "gstreamer-encoder-factory={}",
        gstreamer_output.encoder_factory
    )?;
    writeln!(
        report,
        "gstreamer-rate-control={}",
        gstreamer_output.rate_control
    )?;
    for plugin_path in gstreamer_output.plugin_paths {
        writeln!(report, "gstreamer-plugin={plugin_path}")?;
    }
    report.flush()?;

    println!("report={}", report_path.display());
    println!("cros-bytes={}", cros_payload.len());
    println!("gstreamer-bytes={}", gstreamer_output.payload.len());

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
