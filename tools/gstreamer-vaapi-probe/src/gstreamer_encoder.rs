use std::{
    os::fd::{FromRawFd, IntoRawFd, OwnedFd},
    path::Path,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail, ensure};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_allocators::{DmaBufAllocator, prelude::DmaBufAllocatorExtManual};
use gstreamer_app::{AppSink, AppSrc};
use gstreamer_video::{
    VideoColorMatrix, VideoColorPrimaries, VideoColorRange, VideoColorimetry, VideoFormat,
    VideoFrameFlags, VideoInfo, VideoInfoDmaDrm, VideoMeta, VideoTransferFunction,
    dma_drm_fourcc_from_str,
};
use weld_media_vaapi::VaapiDmabuf;

const DRM_FORMAT_XRGB8888: u32 = u32::from_le_bytes(*b"XR24");
const BITRATE_KILOBITS: u32 = 64_000;
const KEYFRAME_INTERVAL: u32 = 32;

pub(crate) fn supported_xrgb_modifiers(render_node: &Path) -> Result<Vec<u64>> {
    gst::init().context("could not initialize GStreamer")?;
    let postprocess = element_for_device("vapostproc", "postproc", render_node)?;
    let sink = postprocess
        .static_pad("sink")
        .context("GStreamer VA postprocessor has no sink pad")?;
    let caps = sink.pad_template_caps();
    let mut modifiers = Vec::new();

    for (structure, features) in caps.iter_with_features() {
        if !features.contains("memory:DMABuf") {
            continue;
        }
        let Ok(formats) = structure.get::<gst::List>("drm-format") else {
            continue;
        };
        for value in formats.as_slice() {
            let Ok(format) = value.get::<String>() else {
                continue;
            };
            let Ok((fourcc, modifier)) = dma_drm_fourcc_from_str(&format) else {
                continue;
            };
            if fourcc == DRM_FORMAT_XRGB8888 && !modifiers.contains(&modifier) {
                modifiers.push(modifier);
            }
        }
    }

    ensure!(
        !modifiers.is_empty(),
        "GStreamer VA postprocessor exposes no XRGB DMA-BUF modifier on {}",
        render_node.display()
    );
    Ok(modifiers)
}

pub(crate) struct GstreamerEncoder {
    pipeline: gst::Pipeline,
    app_src: AppSrc,
    app_sink: AppSink,
    input_caps: gst::Caps,
    input_modifier: u64,
    postprocess: gst::Element,
    encoder: gst::Element,
    rate_control: String,
    started: Instant,
}

pub(crate) struct GstreamerOutput {
    pub(crate) payload: Vec<u8>,
    pub(crate) packet_count: u64,
    pub(crate) elapsed: Duration,
    pub(crate) requested_input_caps: String,
    pub(crate) negotiated_input_caps: String,
    pub(crate) postprocess_caps: String,
    pub(crate) encoded_caps: String,
    pub(crate) postprocess_factory: String,
    pub(crate) encoder_factory: String,
    pub(crate) rate_control: String,
    pub(crate) plugin_paths: Vec<String>,
}

impl GstreamerEncoder {
    pub(crate) fn new(
        render_node: &Path,
        width: u32,
        height: u32,
        frames_per_second: u32,
        modifier: u64,
    ) -> Result<Self> {
        gst::init().context("could not initialize GStreamer")?;

        let colorimetry = VideoColorimetry::new(
            VideoColorRange::Range0_255,
            VideoColorMatrix::Rgb,
            VideoTransferFunction::Srgb,
            VideoColorPrimaries::Bt709,
        );
        let video_info = VideoInfo::builder(VideoFormat::Bgrx, width, height)
            .fps((i32::try_from(frames_per_second)?, 1))
            .colorimetry(&colorimetry)
            .build()
            .context("could not describe GStreamer XRGB input")?;
        let dma_info = VideoInfoDmaDrm::from_video_info(&video_info, modifier)
            .context("could not describe GStreamer DMA-BUF input")?;
        ensure!(
            dma_info.fourcc() == DRM_FORMAT_XRGB8888,
            "GStreamer translated the probe input to an unexpected DRM format"
        );
        let mut input_caps = dma_info
            .to_caps()
            .context("could not build GStreamer DMA-BUF caps")?;
        let input_structure = input_caps
            .make_mut()
            .structure_mut(0)
            .context("GStreamer DMA-BUF caps have no structure")?;
        input_structure.set(
            "framerate",
            gst::Fraction::new(i32::try_from(frames_per_second)?, 1),
        );
        input_structure.set("colorimetry", colorimetry.to_string());
        println!("gstreamer-input-caps={input_caps}");

        let app_src = gst::ElementFactory::make("appsrc")
            .name("weld-probe-source")
            .build()
            .context("GStreamer appsrc is unavailable")?
            .downcast::<AppSrc>()
            .map_err(|_| anyhow::anyhow!("GStreamer appsrc has an unexpected type"))?;
        app_src.set_caps(Some(&input_caps));
        app_src.set_format(gst::Format::Time);
        app_src.set_is_live(false);
        // The probe owns exactly 64 frames, so an additional byte threshold is
        // unnecessary and can be smaller than one DMA-BUF object.
        app_src.set_block(false);
        app_src.set_max_bytes(0);

        let postprocess = element_for_device("vapostproc", "postproc", render_node)?;
        let encoder = element_for_device("vah264enc", "h264enc", render_node)?;
        encoder.set_property("bitrate", BITRATE_KILOBITS);
        encoder.set_property("key-int-max", KEYFRAME_INTERVAL);
        encoder.set_property("b-frames", 0_u32);
        encoder.set_property("cabac", false);
        encoder.set_property("dct8x8", false);
        encoder.set_property("min-qp", 18_u32);
        encoder.set_property("max-qp", 36_u32);
        encoder.set_property("ref-frames", 1_u32);
        let rate_control = set_enum_property_by_nick(&encoder, "rate-control", "cbr")?;

        let output_colorimetry = VideoColorimetry::new(
            VideoColorRange::Range16_235,
            VideoColorMatrix::Bt709,
            VideoTransferFunction::Srgb,
            VideoColorPrimaries::Bt709,
        );
        let postprocess_caps = gst::Caps::builder("video/x-raw")
            .features(["memory:VAMemory"])
            .field("format", "NV12")
            .field("width", i32::try_from(width)?)
            .field("height", i32::try_from(height)?)
            .field(
                "framerate",
                gst::Fraction::new(i32::try_from(frames_per_second)?, 1),
            )
            .field("colorimetry", output_colorimetry.to_string())
            .build();
        let postprocess_filter = gst::ElementFactory::make("capsfilter")
            .name("weld-probe-va-memory")
            .property("caps", &postprocess_caps)
            .build()
            .context("GStreamer capsfilter is unavailable")?;
        let parser = gst::ElementFactory::make("h264parse")
            .name("weld-probe-parser")
            .property("config-interval", -1_i32)
            .build()
            .context("GStreamer H.264 parser is unavailable")?;
        let encoded_caps = gst::Caps::builder("video/x-h264")
            .field("stream-format", "byte-stream")
            .field("alignment", "au")
            .field("profile", "constrained-baseline")
            .build();
        let encoded_filter = gst::ElementFactory::make("capsfilter")
            .name("weld-probe-annex-b")
            .property("caps", &encoded_caps)
            .build()
            .context("GStreamer capsfilter is unavailable")?;
        let app_sink = gst::ElementFactory::make("appsink")
            .name("weld-probe-sink")
            .property("sync", false)
            .build()
            .context("GStreamer appsink is unavailable")?
            .downcast::<AppSink>()
            .map_err(|_| anyhow::anyhow!("GStreamer appsink has an unexpected type"))?;

        let pipeline = gst::Pipeline::new();
        pipeline
            .add_many([
                app_src.upcast_ref(),
                &postprocess,
                &postprocess_filter,
                &encoder,
                &parser,
                &encoded_filter,
                app_sink.upcast_ref(),
            ])
            .context("could not assemble GStreamer probe pipeline")?;
        gst::Element::link_many([
            app_src.upcast_ref(),
            &postprocess,
            &postprocess_filter,
            &encoder,
            &parser,
            &encoded_filter,
            app_sink.upcast_ref(),
        ])
        .context("could not link GStreamer probe pipeline")?;
        pipeline
            .set_state(gst::State::Playing)
            .context("could not start GStreamer probe pipeline")?;

        Ok(Self {
            pipeline,
            app_src,
            app_sink,
            input_caps,
            input_modifier: modifier,
            postprocess,
            encoder,
            rate_control,
            started: Instant::now(),
        })
    }

    pub(crate) fn push(
        &self,
        frame: VaapiDmabuf,
        sequence: u64,
        frames_per_second: u32,
    ) -> Result<()> {
        let frame_duration = gst::ClockTime::from_nseconds(
            1_000_000_000_u64
                .checked_div(u64::from(frames_per_second))
                .context("GStreamer probe frame rate is zero")?,
        );
        let pts = frame_duration
            .checked_mul(sequence)
            .context("GStreamer probe timestamp overflow")?;
        let buffer = dmabuf_buffer(frame, pts, frame_duration)?;
        self.app_src.push_buffer(buffer).map_err(|error| {
            anyhow::anyhow!("could not submit GStreamer probe frame: {error:?}")
        })?;
        Ok(())
    }

    pub(crate) fn finish(self) -> Result<GstreamerOutput> {
        self.app_src
            .end_of_stream()
            .map_err(|error| anyhow::anyhow!("could not end GStreamer probe stream: {error:?}"))?;

        let mut payload = Vec::new();
        let mut packet_count = 0_u64;
        loop {
            match self.app_sink.pull_sample() {
                Ok(sample) => {
                    let buffer = sample
                        .buffer()
                        .context("GStreamer encoded sample has no buffer")?;
                    let map = buffer
                        .map_readable()
                        .context("could not map GStreamer encoded output")?;
                    payload.extend_from_slice(map.as_slice());
                    packet_count = packet_count
                        .checked_add(1)
                        .context("GStreamer packet count overflow")?;
                }
                Err(_) if self.app_sink.is_eos() => break,
                Err(error) => {
                    return Err(anyhow::anyhow!(
                        "could not receive GStreamer encoded output: {error}"
                    ));
                }
            }
        }

        if let Some(error) = first_pipeline_error(&self.pipeline) {
            bail!("GStreamer probe pipeline failed: {error}");
        }
        ensure!(packet_count > 0, "GStreamer encoded no access units");
        ensure!(!payload.is_empty(), "GStreamer encoded output is empty");

        let negotiated_input = current_pad_caps(&self.postprocess, "sink")?;
        let negotiated_features = negotiated_input
            .features(0)
            .context("GStreamer negotiated input caps have no memory features")?;
        ensure!(
            negotiated_features.contains("memory:DMABuf"),
            "GStreamer postprocessing fell back from DMA-BUF input: {negotiated_input}"
        );
        let negotiated_dma = VideoInfoDmaDrm::from_caps(&negotiated_input)
            .context("GStreamer negotiated input is not DMA_DRM")?;
        ensure!(
            negotiated_dma.fourcc() == DRM_FORMAT_XRGB8888
                && negotiated_dma.modifier() == self.input_modifier,
            "GStreamer changed the input DRM format or modifier: {negotiated_input}"
        );

        let postprocess_caps = current_pad_caps(&self.postprocess, "src")?.to_string();
        ensure!(
            postprocess_caps.contains("memory:VAMemory"),
            "GStreamer postprocessing fell back from VA memory: {postprocess_caps}"
        );
        let encoded_caps = current_pad_caps(&self.encoder, "src")?.to_string();
        let postprocess_factory = factory_name(&self.postprocess)?;
        let encoder_factory = factory_name(&self.encoder)?;
        let plugin_paths = [plugin_path(&self.postprocess), plugin_path(&self.encoder)]
            .into_iter()
            .collect::<Result<Vec<_>>>()?;

        self.pipeline
            .set_state(gst::State::Null)
            .context("could not stop GStreamer probe pipeline")?;

        Ok(GstreamerOutput {
            payload,
            packet_count,
            elapsed: self.started.elapsed(),
            requested_input_caps: self.input_caps.to_string(),
            negotiated_input_caps: negotiated_input.to_string(),
            postprocess_caps,
            encoded_caps,
            postprocess_factory,
            encoder_factory,
            rate_control: self.rate_control,
            plugin_paths,
        })
    }
}

fn element_for_device(
    generic_factory: &str,
    device_suffix: &str,
    render_node: &Path,
) -> Result<gst::Element> {
    let node_name = render_node
        .file_name()
        .and_then(|name| name.to_str())
        .context("render node has no UTF-8 basename")?;
    let candidates = [
        format!("va{node_name}{device_suffix}"),
        generic_factory.to_owned(),
    ];
    let mut attempts = Vec::new();

    for candidate in &candidates {
        let Ok(element) = gst::ElementFactory::make(candidate).build() else {
            attempts.push(format!("{candidate}=unavailable"));
            continue;
        };
        let device_path = element.property::<String>("device-path");
        if Path::new(&device_path) == render_node {
            return Ok(element);
        }
        attempts.push(format!("{candidate}={device_path}"));
    }

    bail!(
        "no GStreamer {device_suffix} factory matched {} ({})",
        render_node.display(),
        attempts.join(", ")
    )
}

fn dmabuf_buffer(
    frame: VaapiDmabuf,
    pts: gst::ClockTime,
    duration: gst::ClockTime,
) -> Result<gst::Buffer> {
    ensure!(
        frame.fourcc == DRM_FORMAT_XRGB8888,
        "probe frame is not XRGB"
    );
    ensure!(
        frame.objects.len() == 1 && frame.planes.len() == 1,
        "GStreamer probe requires one-object, one-plane XRGB DMA-BUF input"
    );
    let mut objects = frame.objects.into_iter();
    let object = objects.next().context("probe DMA-BUF has no object")?;
    let plane = frame
        .planes
        .first()
        .copied()
        .context("probe DMA-BUF has no plane")?;
    ensure!(
        plane.object_index == 0,
        "probe plane references another object"
    );

    let object_size = usize::try_from(object.size)?;
    let allocator = DmaBufAllocator::new();
    let raw_fd = object.file_descriptor.into_raw_fd();
    let memory = {
        // SAFETY: `raw_fd` is a unique owned descriptor. On success GStreamer
        // becomes its sole owner. On failure the descriptor is reconstructed
        // immediately below and dropped exactly once.
        let allocation = unsafe { allocator.alloc_dmabuf(raw_fd, object_size) };
        match allocation {
            Ok(memory) => memory,
            Err(error) => {
                // SAFETY: allocation failed before GStreamer accepted
                // ownership, and `raw_fd` still denotes the unique descriptor.
                drop(unsafe { OwnedFd::from_raw_fd(raw_fd) });
                return Err(error).context("could not wrap probe DMA-BUF for GStreamer");
            }
        }
    };

    let mut buffer = gst::Buffer::new();
    let buffer_ref = buffer
        .get_mut()
        .context("new GStreamer buffer is unexpectedly shared")?;
    buffer_ref.append_memory(memory);
    VideoMeta::add_full(
        buffer_ref,
        VideoFrameFlags::empty(),
        VideoFormat::Bgrx,
        frame.width,
        frame.height,
        &[usize::try_from(plane.offset)?],
        &[i32::try_from(plane.stride)?],
    )
    .context("could not attach XRGB layout to GStreamer DMA-BUF")?;
    buffer_ref.set_pts(pts);
    buffer_ref.set_duration(duration);
    Ok(buffer)
}

fn current_pad_caps(element: &gst::Element, pad_name: &str) -> Result<gst::Caps> {
    let pad = element
        .static_pad(pad_name)
        .with_context(|| format!("{} has no {pad_name} pad", element.name()))?;
    pad.current_caps()
        .with_context(|| format!("{} negotiated no {pad_name} caps", element.name()))
}

fn set_enum_property_by_nick(element: &gst::Element, property: &str, nick: &str) -> Result<String> {
    let specification = element
        .find_property(property)
        .with_context(|| format!("{} has no {property} property", element.name()))?;
    let enum_class = gst::glib::EnumClass::with_type(specification.value_type())
        .with_context(|| format!("{} {property} is not an enum", element.name()))?;
    let value = enum_class
        .to_value_by_nick(nick)
        .with_context(|| format!("{} {property} has no {nick} value", element.name()))?;
    element.set_property_from_value(property, &value);
    let current = element.property_value(property);
    let (_, current_value) = gst::glib::EnumValue::from_value(&current)
        .with_context(|| format!("could not read {} {property}", element.name()))?;
    ensure!(
        current_value.nick() == nick,
        "{} did not retain {property}={nick}",
        element.name()
    );
    Ok(current_value.nick().to_owned())
}

fn factory_name(element: &gst::Element) -> Result<String> {
    element
        .factory()
        .map(|factory| factory.name().to_string())
        .with_context(|| format!("{} has no element factory", element.name()))
}

fn plugin_path(element: &gst::Element) -> Result<String> {
    let factory = element
        .factory()
        .with_context(|| format!("{} has no element factory", element.name()))?;
    let plugin = factory
        .plugin()
        .with_context(|| format!("{} has no plugin", factory.name()))?;
    plugin
        .filename()
        .map(|path| path.display().to_string())
        .with_context(|| format!("{} plugin has no filename", factory.name()))
}

fn first_pipeline_error(pipeline: &gst::Pipeline) -> Option<String> {
    let bus = pipeline.bus()?;
    for message in bus.iter_timed(gst::ClockTime::ZERO) {
        if let gst::MessageView::Error(error) = message.view() {
            return Some(format!(
                "{}: {} ({:?})",
                error
                    .src()
                    .map(|source| source.path_string())
                    .unwrap_or_else(|| "unknown source".into()),
                error.error(),
                error.debug()
            ));
        }
    }
    None
}
