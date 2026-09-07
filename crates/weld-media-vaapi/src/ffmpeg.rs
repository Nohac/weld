use std::{
    ffi::{CStr, CString, c_void},
    mem,
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    path::Path,
    ptr,
    time::{Duration, Instant},
};

use crate::{
    VaapiDmabuf, VaapiDmabufObject, VaapiDmabufPlane, VaapiEncodeGeometry, VppConverter, VppOutput,
};
use anyhow::{Context, Result, bail, ensure};
use ffmpeg_next::{Error as FfmpegError, ffi};
use weld_media::{EncodedFrameKind, VideoCodec};

const SCALE_DESCRIPTION: &str = "format=nv12:out_color_matrix=bt709:out_range=tv";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CodecKind {
    Av1,
    H264,
}

impl CodecKind {
    const fn name(self) -> &'static str {
        match self {
            Self::Av1 => "av1",
            Self::H264 => "h264",
        }
    }

    const fn encoder_name(self) -> &'static CStr {
        match self {
            Self::Av1 => c"av1_vaapi",
            Self::H264 => c"h264_vaapi",
        }
    }

    const fn profile(self) -> i32 {
        match self {
            Self::Av1 => ffi::AV_PROFILE_AV1_MAIN,
            Self::H264 => ffi::AV_PROFILE_H264_CONSTRAINED_BASELINE,
        }
    }

    const fn decoder_name(self) -> &'static CStr {
        match self {
            Self::Av1 => c"av1",
            Self::H264 => c"h264",
        }
    }
}

impl TryFrom<VideoCodec> for CodecKind {
    type Error = anyhow::Error;

    fn try_from(codec: VideoCodec) -> Result<Self> {
        match codec {
            VideoCodec::Av1 => Ok(Self::Av1),
            VideoCodec::H264 => Ok(Self::H264),
            VideoCodec::Vp9 => bail!("VP9 VA-API encoding is not implemented"),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VaapiEncoderSettings {
    codec: VideoCodec,
    bitrate_bits: u64,
    frames_per_second: u32,
    keyframe_interval: u32,
}

impl VaapiEncoderSettings {
    pub fn try_new(
        codec: VideoCodec,
        bitrate_bits: u64,
        frames_per_second: u32,
        keyframe_interval: u32,
    ) -> Result<Self> {
        CodecKind::try_from(codec)?;
        ensure!(bitrate_bits > 0, "encoder bitrate must be positive");
        // Radeon VCN lost its context while probing AV1 above this validated
        // operating point. Capability negotiation can raise the ceiling once
        // the active driver has been tested safely.
        ensure!(
            codec != VideoCodec::Av1 || bitrate_bits <= 8_000_000,
            "AV1 bitrate exceeds the validated 8 Mbps VA-API ceiling"
        );
        ensure!(frames_per_second > 0, "encoder frame rate must be positive");
        ensure!(keyframe_interval > 0, "keyframe interval must be positive");
        Ok(Self {
            codec,
            bitrate_bits,
            frames_per_second,
            keyframe_interval,
        })
    }

    pub const fn codec(self) -> VideoCodec {
        self.codec
    }

    pub const fn bitrate_bits(self) -> u64 {
        self.bitrate_bits
    }

    /// Validate settings for a replacement encoder while preserving codec,
    /// nominal cadence and GOP. This does not mutate a live FFmpeg context.
    pub fn with_bitrate(self, bitrate_bits: u64) -> Result<Self> {
        Self::try_new(
            self.codec,
            bitrate_bits,
            self.frames_per_second,
            self.keyframe_interval,
        )
    }
}

pub struct EncodedPacket {
    pub payload: Vec<u8>,
    pub timestamp_micros: u64,
    pub kind: EncodedFrameKind,
}

pub struct FfmpegEncodeDevice {
    drm: BufferRef,
    vaapi: BufferRef,
}

impl FfmpegEncodeDevice {
    pub fn open(render_node: &Path) -> Result<Self> {
        ffmpeg_next::init().context("could not initialize FFmpeg")?;
        let render_node = CString::new(render_node.as_os_str().as_encoded_bytes())?;
        let drm =
            BufferRef::hardware_device(ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_DRM, &render_node)?;
        let vaapi =
            BufferRef::derived_hardware_device(ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI, &drm)?;
        Ok(Self { drm, vaapi })
    }
}

pub struct FfmpegVaapiDevice(BufferRef);

impl FfmpegVaapiDevice {
    pub fn open(render_node: &Path) -> Result<Self> {
        ffmpeg_next::init().context("could not initialize FFmpeg")?;
        let render_node = CString::new(render_node.as_os_str().as_encoded_bytes())?;
        BufferRef::hardware_device(ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI, &render_node)
            .map(Self)
    }
}

pub struct FfmpegEncoder {
    filter: FilterPipeline,
    codec: CodecContext,
    packet: Packet,
}

impl FfmpegEncoder {
    pub fn new(
        settings: VaapiEncoderSettings,
        geometry: VaapiEncodeGeometry,
        device: &FfmpegEncodeDevice,
        width: u32,
        height: u32,
    ) -> Result<Self> {
        let codec_kind = CodecKind::try_from(settings.codec)?;
        let source_width = i32::try_from(width)?;
        let source_height = i32::try_from(height)?;
        let (coded_width, coded_height) = geometry.coded_extent(width, height)?;
        let coded_width = i32::try_from(coded_width)?;
        let coded_height = i32::try_from(coded_height)?;
        let frames_per_second = i32::try_from(settings.frames_per_second)?;
        let filter = FilterPipeline::new(
            device,
            source_width,
            source_height,
            coded_width,
            coded_height,
            frames_per_second,
        )?;
        let codec = CodecContext::new(
            CodecParameters {
                kind: codec_kind,
                width: coded_width,
                height: coded_height,
                frames_per_second,
                bitrate: i64::try_from(settings.bitrate_bits)?,
                keyframe_interval: i32::try_from(settings.keyframe_interval)?,
                low_power: geometry.entrypoint().is_low_power(),
            },
            filter.output_frames_context()?,
        )?;
        Ok(Self {
            filter,
            codec,
            packet: Packet::new()?,
        })
    }

    pub fn encode(&mut self, input: VaapiDmabuf, timestamp_micros: u64) -> Result<EncodedPacket> {
        let presentation_time = i64::try_from(timestamp_micros)?;
        let input = self.filter.input_frame(input, presentation_time)?;
        self.filter.push(input)?;
        let mut encoded = None;
        while let Some(frame) = self.filter.pull()? {
            ensure!(
                encoded.is_none(),
                "one source frame produced multiple filtered frames"
            );
            self.codec.send(Some(&frame))?;
            encoded = Some(self.receive_one()?);
        }
        encoded.context("FFmpeg filter emitted no frame")
    }

    fn receive_one(&mut self) -> Result<EncodedPacket> {
        ensure!(
            self.codec.receive(&mut self.packet)? == ReceiveStatus::Packet,
            "FFmpeg VA-API encoder did not emit one packet for the submitted frame"
        );
        let payload = self.packet.bytes()?.to_vec();
        let timestamp_micros = u64::try_from(self.packet.presentation_time()?)?;
        let kind = if self.packet.is_keyframe() {
            EncodedFrameKind::Keyframe
        } else {
            EncodedFrameKind::Delta
        };
        self.packet.clear();
        ensure!(
            self.codec.receive(&mut self.packet)? == ReceiveStatus::Again,
            "one source frame produced multiple encoded packets"
        );
        Ok(EncodedPacket {
            payload,
            timestamp_micros,
            kind,
        })
    }
}

pub struct DecodedPacket {
    pub timestamp_micros: u64,
    pub coded_width: u32,
    pub coded_height: u32,
    pub dmabuf: VaapiDmabuf,
    pub timing: DecodeConversionTiming,
}

/// Wall time inside the native finish path, not GPU execution counters.
#[derive(Clone, Copy, Debug)]
pub struct DecodeConversionTiming {
    pub decode_sync: Duration,
    pub conversion_setup: Duration,
    pub conversion_sync: Duration,
}

/// Owns an FFmpeg hardware frame until conversion finishes. It stays on the
/// decoder thread; dropping it releases FFmpeg's reference, not copied pixels.
pub struct PendingDecodedFrame {
    frame: Frame,
    timestamp_micros: u64,
}

impl PendingDecodedFrame {
    pub fn finish(
        self,
        visible_width: u32,
        visible_height: u32,
        xrgb_modifiers: &[u64],
        vpp: &VppConverter,
    ) -> Result<DecodedPacket> {
        let sync_started = Instant::now();
        self.frame.sync_vaapi()?;
        let decode_sync = sync_started.elapsed();
        let source = self.frame.export_vaapi_dmabuf()?;
        ensure!(
            visible_width <= source.width && visible_height <= source.height,
            "decoded extent is smaller than its transported visible extent"
        );
        // Keep the AVFrame and exported input alive through VPP completion.
        let (dmabuf, timing) = vpp.convert_scaled_timed(
            &source,
            visible_width,
            visible_height,
            visible_width,
            visible_height,
            VppOutput::Xrgb8888 {
                modifiers: xrgb_modifiers.to_vec(),
            },
        )?;
        Ok(DecodedPacket {
            timestamp_micros: self.timestamp_micros,
            coded_width: source.width,
            coded_height: source.height,
            dmabuf,
            timing: DecodeConversionTiming {
                decode_sync,
                conversion_setup: timing.setup,
                conversion_sync: timing.sync,
            },
        })
    }
}

pub struct FfmpegDecoder {
    context: *mut ffi::AVCodecContext,
    _vaapi_device: BufferRef,
    packet: Packet,
}

impl FfmpegDecoder {
    /// `depth` is the maximum caller-held hardware-frame count. Reserve it in
    /// FFmpeg's hardware pool before opening, independently of codec DPB needs.
    pub fn new(codec: VideoCodec, device: &FfmpegVaapiDevice, depth: usize) -> Result<Self> {
        ensure!(
            (1..=2).contains(&depth),
            "unsupported decoder pipeline depth"
        );
        let extra_hw_frames = i32::try_from(depth)?;
        let packet = Packet::new()?;
        let codec_kind = CodecKind::try_from(codec)?;
        let vaapi_device = device.0.try_clone()?;
        // SAFETY: decoder_name is a static null-terminated FFmpeg decoder name.
        let decoder =
            unsafe { ffi::avcodec_find_decoder_by_name(codec_kind.decoder_name().as_ptr()) };
        ensure!(
            !decoder.is_null(),
            "FFmpeg has no {} decoder",
            codec_kind.name()
        );
        // SAFETY: decoder points to FFmpeg-owned immutable codec metadata.
        let context = unsafe { ffi::avcodec_alloc_context3(decoder) };
        ensure!(
            !context.is_null(),
            "could not allocate FFmpeg decoder context"
        );
        let setup = (|| {
            // SAFETY: context uniquely owns an unopened decoder context.
            unsafe {
                (*context).time_base = ffi::AVRational {
                    num: 1,
                    den: 1_000_000,
                };
                (*context).pkt_timebase = ffi::AVRational {
                    num: 1,
                    den: 1_000_000,
                };
                (*context).thread_count = 1;
                (*context).extra_hw_frames = extra_hw_frames;
                (*context).get_format = Some(select_vaapi_format);
                (*context).hw_device_ctx = ffi::av_buffer_ref(vaapi_device.0);
            }
            // SAFETY: context remains live for this null check.
            ensure!(
                unsafe { !(*context).hw_device_ctx.is_null() },
                "could not retain FFmpeg VA-API device"
            );
            // SAFETY: context is initialized and decoder is the metadata used to allocate it.
            let result = unsafe { ffi::avcodec_open2(context, decoder, ptr::null_mut()) };
            check(result, "could not open FFmpeg VA-API decoder")
        })();
        if let Err(error) = setup {
            let mut context = context;
            // SAFETY: context is the unique allocation returned above.
            unsafe { ffi::avcodec_free_context(&mut context) };
            return Err(error);
        }
        Ok(Self {
            context,
            _vaapi_device: vaapi_device,
            packet,
        })
    }

    /// Submit one low-delay access unit and retain its hardware output without
    /// explicitly synchronizing it. Driver/API calls may still block. Keep at
    /// most the configured depth of outputs; later submissions may precede finish.
    pub fn submit(&mut self, payload: &[u8], timestamp_micros: u64) -> Result<PendingDecodedFrame> {
        self.packet.set_bytes(payload, timestamp_micros)?;
        // SAFETY: context is open and packet owns a padded FFmpeg allocation.
        let result = unsafe { ffi::avcodec_send_packet(self.context, self.packet.0) };
        self.packet.clear();
        check(result, "could not submit packet to FFmpeg decoder")?;

        let mut output = None;
        loop {
            let mut decoded = Frame::new()?;
            // SAFETY: context is open and decoded is an empty writable frame.
            let result = unsafe { ffi::avcodec_receive_frame(self.context, decoded.as_mut_ptr()) };
            match status(result)? {
                CallStatus::Ready => {
                    let decoded_timestamp = decoded.timestamp()?;
                    ensure!(
                        output.is_none(),
                        "low-delay decoder returned more than one frame"
                    );
                    ensure!(
                        decoded_timestamp == timestamp_micros,
                        "decoder returned an unexpected timestamp"
                    );
                    output = Some(PendingDecodedFrame {
                        frame: decoded,
                        timestamp_micros,
                    });
                }
                CallStatus::Again | CallStatus::End => break,
            }
        }
        output.context("low-delay decoder returned no frame")
    }
}

impl Drop for FfmpegDecoder {
    fn drop(&mut self) {
        // SAFETY: context is uniquely owned and may be nullified by FFmpeg.
        unsafe { ffi::avcodec_free_context(&mut self.context) };
    }
}

unsafe extern "C" fn select_vaapi_format(
    _context: *mut ffi::AVCodecContext,
    formats: *const ffi::AVPixelFormat,
) -> ffi::AVPixelFormat {
    if formats.is_null() {
        return ffi::AVPixelFormat::AV_PIX_FMT_NONE;
    }
    for index in 0..64 {
        // SAFETY: FFmpeg supplies a terminated pixel-format list to get_format.
        let format = unsafe { *formats.add(index) };
        if format == ffi::AVPixelFormat::AV_PIX_FMT_VAAPI {
            return format;
        }
        if format == ffi::AVPixelFormat::AV_PIX_FMT_NONE {
            return format;
        }
    }
    ffi::AVPixelFormat::AV_PIX_FMT_NONE
}

struct FilterPipeline {
    graph: *mut ffi::AVFilterGraph,
    source: *mut ffi::AVFilterContext,
    sink: *mut ffi::AVFilterContext,
    _drm_device: BufferRef,
    _vaapi_device: BufferRef,
    drm_frames: BufferRef,
}

impl FilterPipeline {
    fn new(
        device: &FfmpegEncodeDevice,
        source_width: i32,
        source_height: i32,
        coded_width: i32,
        coded_height: i32,
        fps: i32,
    ) -> Result<Self> {
        let drm_device = device.drm.try_clone()?;
        let vaapi_device = device.vaapi.try_clone()?;
        let drm_frames = BufferRef::drm_frames(&drm_device, source_width, source_height)?;
        // SAFETY: allocating an independent FFmpeg filter graph has no caller-side preconditions.
        let graph = unsafe { ffi::avfilter_graph_alloc() };
        ensure!(!graph.is_null(), "could not allocate FFmpeg filter graph");

        let setup = (|| {
            let source = allocate_filter(graph, c"buffer", c"weld-dmabuf-source")?;
            configure_source(source, &drm_frames, source_width, source_height, fps)?;
            // SAFETY: source has received all mandatory buffer parameters and is not initialized yet.
            let result = unsafe { ffi::avfilter_init_str(source, ptr::null()) };
            check(result, "could not initialize FFmpeg DMA-BUF source")?;

            let hwmap = allocate_filter(graph, c"hwmap", c"weld-vaapi-map")?;
            retain_filter_device(hwmap, &vaapi_device)?;
            initialize_filter(hwmap, "mode=read")?;
            let scale = allocate_filter(graph, c"scale_vaapi", c"weld-vaapi-scale")?;
            initialize_filter(scale, SCALE_DESCRIPTION)?;
            link_filters(source, hwmap)?;
            link_filters(hwmap, scale)?;

            let tail = if (source_width, source_height) == (coded_width, coded_height) {
                scale
            } else {
                let pad = allocate_filter(graph, c"pad_vaapi", c"weld-vaapi-pad")?;
                initialize_filter(
                    pad,
                    &format!("w={coded_width}:h={coded_height}:x=0:y=0:color=black"),
                )?;
                link_filters(scale, pad)?;
                pad
            };
            let sink = create_filter(graph, c"buffersink", c"weld-vaapi-sink")?;
            link_filters(tail, sink)?;
            // SAFETY: graph owns all linked filters and has not yet been configured.
            let result = unsafe { ffi::avfilter_graph_config(graph, ptr::null_mut()) };
            check(result, "could not configure FFmpeg hardware filter graph")?;
            // SAFETY: sink is a configured buffersink owned by graph.
            let format = unsafe { ffi::av_buffersink_get_format(sink) };
            ensure!(
                format == ffi::AVPixelFormat::AV_PIX_FMT_VAAPI as i32,
                "FFmpeg filter output is not a VA-API hardware frame"
            );
            Ok((source, sink))
        })();

        match setup {
            Ok((source, sink)) => Ok(Self {
                graph,
                source,
                sink,
                _drm_device: drm_device,
                _vaapi_device: vaapi_device,
                drm_frames,
            }),
            Err(error) => {
                let mut graph = graph;
                // SAFETY: graph is the unique allocation returned above and is not retained.
                unsafe { ffi::avfilter_graph_free(&mut graph) };
                Err(error)
            }
        }
    }

    fn output_frames_context(&self) -> Result<*mut ffi::AVBufferRef> {
        // SAFETY: sink is configured and remains owned by self.graph.
        let frames = unsafe { ffi::av_buffersink_get_hw_frames_ctx(self.sink) };
        ensure!(
            !frames.is_null(),
            "FFmpeg filter produced no hardware frames context"
        );
        Ok(frames)
    }

    fn input_frame(&self, input: VaapiDmabuf, pts: i64) -> Result<Frame> {
        Frame::from_dmabuf(input, &self.drm_frames, pts)
    }

    fn push(&mut self, mut frame: Frame) -> Result<()> {
        // SAFETY: source is configured and frame is a valid reference-counted DRM frame.
        let result = unsafe { ffi::av_buffersrc_add_frame(self.source, frame.as_mut_ptr()) };
        check(result, "could not submit DMA-BUF to FFmpeg filter graph")
    }

    fn pull(&mut self) -> Result<Option<Frame>> {
        let mut frame = Frame::new()?;
        // SAFETY: sink is configured and frame is an empty writable AVFrame.
        let result = unsafe { ffi::av_buffersink_get_frame(self.sink, frame.as_mut_ptr()) };
        match status(result)? {
            CallStatus::Ready => Ok(Some(frame)),
            CallStatus::Again | CallStatus::End => Ok(None),
        }
    }
}

impl Drop for FilterPipeline {
    fn drop(&mut self) {
        // SAFETY: graph is uniquely owned and freeing it invalidates source and sink together.
        unsafe { ffi::avfilter_graph_free(&mut self.graph) };
    }
}

struct CodecContext(*mut ffi::AVCodecContext);

struct CodecParameters {
    kind: CodecKind,
    width: i32,
    height: i32,
    frames_per_second: i32,
    bitrate: i64,
    keyframe_interval: i32,
    low_power: bool,
}

impl CodecContext {
    fn new(parameters: CodecParameters, frames: *mut ffi::AVBufferRef) -> Result<Self> {
        // SAFETY: the static C string is terminated and remains valid for the call.
        let encoder_name = parameters.kind.encoder_name();
        let codec = unsafe { ffi::avcodec_find_encoder_by_name(encoder_name.as_ptr()) };
        ensure!(
            !codec.is_null(),
            "FFmpeg has no {} encoder",
            encoder_name.to_string_lossy()
        );
        // SAFETY: codec points to FFmpeg-owned immutable encoder metadata.
        let context = unsafe { ffi::avcodec_alloc_context3(codec) };
        ensure!(
            !context.is_null(),
            "could not allocate FFmpeg encoder context"
        );
        let owned = Self(context);

        // SAFETY: owned uniquely owns an unopened AVCodecContext and assigned values satisfy encoder ranges.
        unsafe {
            (*context).width = parameters.width;
            (*context).height = parameters.height;
            (*context).time_base = ffi::AVRational {
                num: 1,
                den: 1_000_000,
            };
            (*context).framerate = ffi::AVRational {
                num: parameters.frames_per_second,
                den: 1,
            };
            (*context).sample_aspect_ratio = ffi::AVRational { num: 1, den: 1 };
            (*context).pix_fmt = ffi::AVPixelFormat::AV_PIX_FMT_VAAPI;
            (*context).profile = parameters.kind.profile();
            (*context).bit_rate = parameters.bitrate;
            (*context).rc_min_rate = parameters.bitrate;
            (*context).rc_max_rate = parameters.bitrate;
            // The one-second CBR reservoir is rate-control accounting, not a
            // frame queue. Pipeline depth remains bounded independently.
            (*context).rc_buffer_size = i32::try_from(parameters.bitrate)?;
            (*context).gop_size = parameters.keyframe_interval;
            (*context).max_b_frames = 0;
            (*context).color_range = ffi::AVColorRange::AVCOL_RANGE_MPEG;
            (*context).colorspace = ffi::AVColorSpace::AVCOL_SPC_BT709;
            (*context).color_primaries = ffi::AVColorPrimaries::AVCOL_PRI_BT709;
            (*context).color_trc = ffi::AVColorTransferCharacteristic::AVCOL_TRC_IEC61966_2_1;
            (*context).hw_frames_ctx = ffi::av_buffer_ref(frames);
        }
        // SAFETY: context is valid for the lifetime of owned.
        ensure!(
            unsafe { !(*context).hw_frames_ctx.is_null() },
            "could not retain FFmpeg output frames context"
        );
        // SAFETY: context is valid and owns its private encoder options.
        let private_options = unsafe { (*context).priv_data };
        set_option(private_options, c"rc_mode", c"CBR")?;
        set_option(private_options, c"async_depth", c"1")?;
        set_option(
            private_options,
            c"low_power",
            if parameters.low_power { c"1" } else { c"0" },
        )?;
        if parameters.kind == CodecKind::H264 {
            set_option(private_options, c"coder", c"cavlc")?;
        }
        // SAFETY: context is initialized and codec is the same encoder used to allocate it.
        let result = unsafe { ffi::avcodec_open2(context, codec, ptr::null_mut()) };
        check(result, "could not open FFmpeg VA-API encoder")?;
        Ok(owned)
    }

    fn send(&mut self, frame: Option<&Frame>) -> Result<()> {
        let pointer = frame.map_or(ptr::null(), Frame::as_ptr);
        // SAFETY: context is open and pointer is null for flush or a valid frame for the call duration.
        let result = unsafe { ffi::avcodec_send_frame(self.0, pointer) };
        check(result, "could not submit frame to FFmpeg encoder")
    }

    fn receive(&mut self, packet: &mut Packet) -> Result<ReceiveStatus> {
        // SAFETY: context is open and packet is a valid reusable AVPacket.
        status(unsafe { ffi::avcodec_receive_packet(self.0, packet.0) }).map(
            |status| match status {
                CallStatus::Ready => ReceiveStatus::Packet,
                CallStatus::Again => ReceiveStatus::Again,
                CallStatus::End => ReceiveStatus::End,
            },
        )
    }
}

impl Drop for CodecContext {
    fn drop(&mut self) {
        // SAFETY: self.0 is uniquely owned and may be nullified by FFmpeg.
        unsafe { ffi::avcodec_free_context(&mut self.0) };
    }
}

struct Packet(*mut ffi::AVPacket);

impl Packet {
    fn new() -> Result<Self> {
        // SAFETY: packet allocation has no caller-side preconditions.
        let packet = unsafe { ffi::av_packet_alloc() };
        ensure!(!packet.is_null(), "could not allocate FFmpeg packet");
        Ok(Self(packet))
    }

    fn set_bytes(&mut self, payload: &[u8], timestamp_micros: u64) -> Result<()> {
        self.clear();
        let length = i32::try_from(payload.len())?;
        // SAFETY: packet is empty and av_new_packet allocates payload plus the
        // padding required by FFmpeg bitstream readers.
        let result = unsafe { ffi::av_new_packet(self.0, length) };
        check(result, "could not allocate FFmpeg decoder packet")?;
        // SAFETY: av_new_packet allocated at least payload.len() writable bytes.
        unsafe {
            ptr::copy_nonoverlapping(payload.as_ptr(), (*self.0).data, payload.len());
            let timestamp = i64::try_from(timestamp_micros)?;
            (*self.0).pts = timestamp;
            (*self.0).dts = timestamp;
        }
        Ok(())
    }

    fn bytes(&self) -> Result<&[u8]> {
        // SAFETY: self.0 is a live packet allocation.
        let packet = unsafe { &*self.0 };
        ensure!(packet.size >= 0, "FFmpeg returned a negative packet size");
        let length = usize::try_from(packet.size)?;
        ensure!(
            !packet.data.is_null() || length == 0,
            "FFmpeg packet data is null"
        );
        // SAFETY: FFmpeg owns packet.data for exactly packet.size bytes until av_packet_unref.
        Ok(unsafe { std::slice::from_raw_parts(packet.data, length) })
    }

    fn presentation_time(&self) -> Result<i64> {
        // SAFETY: self.0 is a live packet allocation.
        let packet = unsafe { &*self.0 };
        ensure!(
            packet.pts >= 0,
            "FFmpeg returned a negative packet timestamp"
        );
        Ok(packet.pts)
    }

    fn is_keyframe(&self) -> bool {
        // SAFETY: self.0 is a live packet allocation.
        unsafe { (*self.0).flags & ffi::AV_PKT_FLAG_KEY != 0 }
    }

    fn clear(&mut self) {
        // SAFETY: self.0 is a live AVPacket and remains reusable after unref.
        unsafe { ffi::av_packet_unref(self.0) };
    }
}

impl Drop for Packet {
    fn drop(&mut self) {
        // SAFETY: self.0 is uniquely owned and may be nullified by FFmpeg.
        unsafe { ffi::av_packet_free(&mut self.0) };
    }
}

struct Frame(*mut ffi::AVFrame);

impl Frame {
    fn new() -> Result<Self> {
        // SAFETY: frame allocation has no caller-side preconditions.
        let frame = unsafe { ffi::av_frame_alloc() };
        ensure!(!frame.is_null(), "could not allocate FFmpeg frame");
        Ok(Self(frame))
    }

    fn from_dmabuf(input: VaapiDmabuf, frames: &BufferRef, pts: i64) -> Result<Self> {
        ensure!(
            input.objects.len() <= 4 && input.planes.len() <= 4,
            "DMA-BUF exceeds FFmpeg DRM descriptor limits"
        );
        let width = i32::try_from(input.width)?;
        let height = i32::try_from(input.height)?;
        let frame = Self::new()?;
        let owner = Box::new(DrmDescriptorOwner::new(
            input.objects,
            input.planes,
            input.fourcc,
        )?);
        let owner = Box::into_raw(owner);
        // SAFETY: owner is a unique Box allocation retained by the callback on success.
        let buffer = unsafe {
            ffi::av_buffer_create(
                owner.cast::<u8>(),
                mem::size_of::<DrmDescriptorOwner>(),
                Some(free_drm_descriptor),
                ptr::null_mut(),
                0,
            )
        };
        if buffer.is_null() {
            // SAFETY: FFmpeg did not accept owner, so this reconstructs its unique Box.
            unsafe { drop(Box::from_raw(owner)) };
            bail!("could not create FFmpeg-owned DRM descriptor");
        }

        // SAFETY: frame.0 is uniquely owned and initialized by av_frame_alloc.
        let frame_ref = unsafe { &mut *frame.0 };
        frame_ref.format = ffi::AVPixelFormat::AV_PIX_FMT_DRM_PRIME as i32;
        frame_ref.width = width;
        frame_ref.height = height;
        frame_ref.pts = pts;
        frame_ref.time_base = ffi::AVRational {
            num: 1,
            den: 1_000_000,
        };
        frame_ref.sample_aspect_ratio = ffi::AVRational { num: 1, den: 1 };
        frame_ref.color_range = ffi::AVColorRange::AVCOL_RANGE_JPEG;
        frame_ref.colorspace = ffi::AVColorSpace::AVCOL_SPC_RGB;
        frame_ref.color_primaries = ffi::AVColorPrimaries::AVCOL_PRI_BT709;
        frame_ref.color_trc = ffi::AVColorTransferCharacteristic::AVCOL_TRC_IEC61966_2_1;
        frame_ref.buf[0] = buffer;
        frame_ref.data[0] = owner.cast::<u8>();
        frame_ref.hw_frames_ctx = frames.try_clone()?.into_raw();
        Ok(frame)
    }

    fn as_ptr(&self) -> *const ffi::AVFrame {
        self.0
    }

    fn as_mut_ptr(&mut self) -> *mut ffi::AVFrame {
        self.0
    }

    fn timestamp(&self) -> Result<u64> {
        // SAFETY: self.0 is a live AVFrame.
        let frame = unsafe { &*self.0 };
        let timestamp = if frame.pts != i64::MIN {
            frame.pts
        } else {
            frame.best_effort_timestamp
        };
        ensure!(timestamp != i64::MIN, "decoded frame has no timestamp");
        Ok(u64::try_from(timestamp)?)
    }

    fn vaapi_surface(&self) -> Result<(cros_libva::VADisplay, cros_libva::VASurfaceID)> {
        // SAFETY: self.0 is a live decoded frame while this method runs.
        let frame = unsafe { &*self.0 };
        ensure!(
            frame.format == ffi::AVPixelFormat::AV_PIX_FMT_VAAPI as i32,
            "FFmpeg decoder did not produce a VA-API frame (format={})",
            frame.format,
        );
        ensure!(
            !frame.hw_frames_ctx.is_null(),
            "VA-API frame has no frames context"
        );
        // SAFETY: the non-null AVBufferRef belongs to this live frame.
        let frames_data = unsafe { (*frame.hw_frames_ctx).data };
        ensure!(!frames_data.is_null(), "VA-API frame context has no data");
        // SAFETY: hw_frames_ctx.data is AVHWFramesContext for hardware frames.
        let frames = unsafe { &*(frames_data.cast::<ffi::AVHWFramesContext>()) };
        ensure!(
            !frames.device_ctx.is_null(),
            "VA-API frame has no device context"
        );
        // SAFETY: the non-null device context belongs to the live frames context.
        let device = unsafe { &*frames.device_ctx };
        ensure!(
            device.type_ == ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
            "decoded frame device is not VA-API"
        );
        ensure!(
            !device.hwctx.is_null(),
            "VA-API device has no native context"
        );
        // SAFETY: VA-API AVHWDeviceContext.hwctx is AVVAAPIDeviceContext.
        let vaapi = unsafe { &*(device.hwctx.cast::<ffi::AVVAAPIDeviceContext>()) };
        ensure!(!vaapi.display.is_null(), "FFmpeg VA-API display is null");
        ensure!(!frame.data[3].is_null(), "VA-API frame has no surface ID");
        // FFmpeg stores the VASurfaceID value itself in the data[3] pointer slot.
        let surface = cros_libva::VASurfaceID::try_from(frame.data[3] as usize)?;
        Ok((vaapi.display, surface))
    }

    fn sync_vaapi(&self) -> Result<()> {
        let (display, surface) = self.vaapi_surface()?;
        // SAFETY: display and surface belong to this live decoded frame.
        let status = unsafe { cros_libva::vaSyncSurface(display, surface) };
        ensure!(
            status as u32 == cros_libva::VA_STATUS_SUCCESS,
            "could not synchronize decoded VA surface"
        );
        Ok(())
    }

    // Only called after sync_vaapi, while this AVFrame still owns the surface.
    fn export_vaapi_dmabuf(&self) -> Result<VaapiDmabuf> {
        let (display, surface) = self.vaapi_surface()?;
        let mut descriptor = cros_libva::VADRMPRIMESurfaceDescriptor::default();
        // SAFETY: descriptor is writable, display and surface are live, and
        // libva transfers ownership of exported file descriptors on success.
        let status = unsafe {
            cros_libva::vaExportSurfaceHandle(
                display,
                surface,
                cros_libva::VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2,
                cros_libva::VA_EXPORT_SURFACE_READ_ONLY
                    | cros_libva::VA_EXPORT_SURFACE_COMPOSED_LAYERS,
                (&mut descriptor as *mut cros_libva::VADRMPRIMESurfaceDescriptor).cast(),
            )
        };
        ensure!(
            status as u32 == cros_libva::VA_STATUS_SUCCESS,
            "could not export decoded VA surface"
        );
        prime_descriptor_from_libva(descriptor)
    }
}

impl Drop for Frame {
    fn drop(&mut self) {
        // SAFETY: self.0 is uniquely owned and may be nullified by FFmpeg.
        unsafe { ffi::av_frame_free(&mut self.0) };
    }
}

fn prime_descriptor_from_libva(
    descriptor: cros_libva::VADRMPRIMESurfaceDescriptor,
) -> Result<VaapiDmabuf> {
    let object_count = usize::try_from(descriptor.num_objects)?;
    ensure!(
        (1..=4).contains(&object_count),
        "libva exported an invalid object count"
    );
    let objects = descriptor.objects[..object_count]
        .iter()
        .map(|object| {
            ensure!(
                object.fd >= 0,
                "libva exported an invalid DMA-BUF descriptor"
            );
            Ok(VaapiDmabufObject {
                // SAFETY: successful vaExportSurfaceHandle transfers ownership
                // of each returned descriptor to the caller exactly once.
                file_descriptor: unsafe { OwnedFd::from_raw_fd(object.fd) },
                size: object.size,
                modifier: object.drm_format_modifier,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    ensure!(
        descriptor.num_layers == 1,
        "libva did not export composed layers"
    );
    let layer = &descriptor.layers[0];
    let plane_count = usize::try_from(layer.num_planes)?;
    ensure!(
        (1..=4).contains(&plane_count),
        "libva exported an invalid plane count"
    );
    let planes = (0..plane_count)
        .map(|index| {
            Ok(VaapiDmabufPlane {
                object_index: u8::try_from(layer.object_index[index])?,
                offset: layer.offset[index],
                stride: layer.pitch[index],
            })
        })
        .collect::<Result<Vec<_>>>()?;
    VaapiDmabuf::try_new(
        descriptor.width,
        descriptor.height,
        layer.drm_format,
        objects,
        planes,
    )
}

#[repr(C)]
struct DrmDescriptorOwner {
    descriptor: ffi::AVDRMFrameDescriptor,
    file_descriptors: Vec<OwnedFd>,
}

impl DrmDescriptorOwner {
    fn new(
        objects: Vec<VaapiDmabufObject>,
        planes: Vec<VaapiDmabufPlane>,
        fourcc: u32,
    ) -> Result<Self> {
        let mut descriptor = empty_drm_descriptor();
        descriptor.nb_objects = i32::try_from(objects.len())?;
        descriptor.nb_layers = 1;
        descriptor.layers[0].format = fourcc;
        descriptor.layers[0].nb_planes = i32::try_from(planes.len())?;
        let mut file_descriptors = Vec::with_capacity(objects.len());
        for (index, object) in objects.into_iter().enumerate() {
            descriptor.objects[index] = ffi::AVDRMObjectDescriptor {
                fd: object.file_descriptor.as_raw_fd(),
                size: usize::try_from(object.size)?,
                format_modifier: object.modifier,
            };
            file_descriptors.push(object.file_descriptor);
        }
        for (index, plane) in planes.into_iter().enumerate() {
            descriptor.layers[0].planes[index] = ffi::AVDRMPlaneDescriptor {
                object_index: i32::from(plane.object_index),
                offset: isize::try_from(plane.offset)?,
                pitch: isize::try_from(plane.stride)?,
            };
        }
        Ok(Self {
            descriptor,
            file_descriptors,
        })
    }
}

unsafe extern "C" fn free_drm_descriptor(_opaque: *mut c_void, data: *mut u8) {
    if !data.is_null() {
        // SAFETY: data was created by Box::into_raw in Frame::from_dmabuf and FFmpeg calls this once.
        unsafe { drop(Box::from_raw(data.cast::<DrmDescriptorOwner>())) };
    }
}

struct BufferRef(*mut ffi::AVBufferRef);

impl BufferRef {
    fn hardware_device(kind: ffi::AVHWDeviceType, path: &CStr) -> Result<Self> {
        let mut pointer = ptr::null_mut();
        // SAFETY: pointer is writable and path is a terminated device path for the call duration.
        let result = unsafe {
            ffi::av_hwdevice_ctx_create(&mut pointer, kind, path.as_ptr(), ptr::null_mut(), 0)
        };
        check(result, "could not create FFmpeg DRM device")?;
        ensure!(!pointer.is_null(), "FFmpeg returned a null DRM device");
        Ok(Self(pointer))
    }

    fn derived_hardware_device(kind: ffi::AVHWDeviceType, source: &Self) -> Result<Self> {
        let mut pointer = ptr::null_mut();
        // SAFETY: source is a live FFmpeg hardware device and pointer is
        // writable for the derived reference returned by FFmpeg.
        let result =
            unsafe { ffi::av_hwdevice_ctx_create_derived(&mut pointer, kind, source.0, 0) };
        check(result, "could not derive FFmpeg hardware device")?;
        ensure!(
            !pointer.is_null(),
            "FFmpeg returned a null derived hardware device"
        );
        Ok(Self(pointer))
    }

    fn drm_frames(device: &Self, width: i32, height: i32) -> Result<Self> {
        // SAFETY: device is a live FFmpeg hardware device reference.
        let pointer = unsafe { ffi::av_hwframe_ctx_alloc(device.0) };
        ensure!(
            !pointer.is_null(),
            "could not allocate FFmpeg DRM frames context"
        );
        let owned = Self(pointer);
        // SAFETY: AVBufferRef.data is an AVHWFramesContext for av_hwframe_ctx_alloc results.
        let context = unsafe { &mut *((*pointer).data.cast::<ffi::AVHWFramesContext>()) };
        context.format = ffi::AVPixelFormat::AV_PIX_FMT_DRM_PRIME;
        context.sw_format = ffi::AVPixelFormat::AV_PIX_FMT_BGR0;
        context.width = width;
        context.height = height;
        // SAFETY: the context is uniquely initialized with valid DRM frame parameters.
        let result = unsafe { ffi::av_hwframe_ctx_init(pointer) };
        check(result, "could not initialize FFmpeg DRM frames context")?;
        Ok(owned)
    }

    fn try_clone(&self) -> Result<Self> {
        // SAFETY: self.0 is a live AVBufferRef.
        let pointer = unsafe { ffi::av_buffer_ref(self.0) };
        ensure!(
            !pointer.is_null(),
            "could not retain FFmpeg buffer reference"
        );
        Ok(Self(pointer))
    }

    fn into_raw(mut self) -> *mut ffi::AVBufferRef {
        let pointer = self.0;
        self.0 = ptr::null_mut();
        pointer
    }
}

impl Drop for BufferRef {
    fn drop(&mut self) {
        // SAFETY: self.0 is an owned reference or null after into_raw.
        unsafe { ffi::av_buffer_unref(&mut self.0) };
    }
}

fn create_filter(
    graph: *mut ffi::AVFilterGraph,
    factory: &CStr,
    name: &CStr,
) -> Result<*mut ffi::AVFilterContext> {
    // SAFETY: factory is a terminated static filter name.
    let filter = unsafe { ffi::avfilter_get_by_name(factory.as_ptr()) };
    ensure!(
        !filter.is_null(),
        "FFmpeg filter {} is unavailable",
        factory.to_string_lossy()
    );
    let mut context = ptr::null_mut();
    // SAFETY: graph is live, filter is FFmpeg-owned metadata, and strings remain valid for the call.
    let result = unsafe {
        ffi::avfilter_graph_create_filter(
            &mut context,
            filter,
            name.as_ptr(),
            ptr::null(),
            ptr::null_mut(),
            graph,
        )
    };
    check(result, "could not create FFmpeg filter")?;
    Ok(context)
}

fn allocate_filter(
    graph: *mut ffi::AVFilterGraph,
    factory: &CStr,
    name: &CStr,
) -> Result<*mut ffi::AVFilterContext> {
    // SAFETY: factory is a terminated static filter name.
    let filter = unsafe { ffi::avfilter_get_by_name(factory.as_ptr()) };
    ensure!(
        !filter.is_null(),
        "FFmpeg filter {} is unavailable",
        factory.to_string_lossy()
    );
    // SAFETY: graph is live, filter is FFmpeg-owned metadata, and name remains valid for the call.
    let context = unsafe { ffi::avfilter_graph_alloc_filter(graph, filter, name.as_ptr()) };
    ensure!(!context.is_null(), "could not allocate FFmpeg filter");
    Ok(context)
}

fn configure_source(
    source: *mut ffi::AVFilterContext,
    frames: &BufferRef,
    width: i32,
    height: i32,
    fps: i32,
) -> Result<()> {
    // SAFETY: source parameter allocation has no caller-side preconditions.
    let parameters = unsafe { ffi::av_buffersrc_parameters_alloc() };
    ensure!(
        !parameters.is_null(),
        "could not allocate FFmpeg source parameters"
    );
    // SAFETY: parameters is uniquely owned and initialized by FFmpeg.
    unsafe {
        (*parameters).format = ffi::AVPixelFormat::AV_PIX_FMT_DRM_PRIME as i32;
        (*parameters).time_base = ffi::AVRational {
            num: 1,
            den: 1_000_000,
        };
        (*parameters).width = width;
        (*parameters).height = height;
        (*parameters).sample_aspect_ratio = ffi::AVRational { num: 1, den: 1 };
        (*parameters).frame_rate = ffi::AVRational { num: fps, den: 1 };
        (*parameters).hw_frames_ctx = frames.0;
        (*parameters).color_space = ffi::AVColorSpace::AVCOL_SPC_RGB;
        (*parameters).color_range = ffi::AVColorRange::AVCOL_RANGE_JPEG;
    }
    // SAFETY: source is a buffer filter and parameters stays live for the call.
    let result = unsafe { ffi::av_buffersrc_parameters_set(source, parameters) };
    // SAFETY: parameters is the allocation returned above and is no longer used.
    unsafe { ffi::av_free(parameters.cast()) };
    check(result, "could not configure FFmpeg DMA-BUF source")
}

fn retain_filter_device(context: *mut ffi::AVFilterContext, device: &BufferRef) -> Result<()> {
    // SAFETY: device is a live FFmpeg hardware device reference.
    let retained = unsafe { ffi::av_buffer_ref(device.0) };
    ensure!(
        !retained.is_null(),
        "could not retain FFmpeg filter hardware device"
    );
    // SAFETY: context is a newly allocated, uninitialized filter with no
    // hardware device. Its graph releases this retained reference.
    unsafe {
        (*context).hw_device_ctx = retained;
    }
    Ok(())
}

fn initialize_filter(context: *mut ffi::AVFilterContext, options: &str) -> Result<()> {
    let options = CString::new(options)?;
    // SAFETY: context is an uninitialized graph-owned filter and options is
    // a terminated option string valid for this call.
    let result = unsafe { ffi::avfilter_init_str(context, options.as_ptr()) };
    check(result, "could not initialize FFmpeg hardware filter")
}

fn link_filters(
    source: *mut ffi::AVFilterContext,
    destination: *mut ffi::AVFilterContext,
) -> Result<()> {
    // SAFETY: both contexts are initialized filters in the same live graph;
    // this tracer uses their single video input and output pads.
    let result = unsafe { ffi::avfilter_link(source, 0, destination, 0) };
    check(result, "could not link FFmpeg hardware filters")
}

fn set_option(target: *mut c_void, name: &CStr, value: &CStr) -> Result<()> {
    ensure!(!target.is_null(), "FFmpeg encoder has no private options");
    // SAFETY: target is an AVOption-bearing encoder private context and strings are terminated.
    let result = unsafe { ffi::av_opt_set(target, name.as_ptr(), value.as_ptr(), 0) };
    check(result, "could not set FFmpeg encoder option")
}

fn empty_drm_descriptor() -> ffi::AVDRMFrameDescriptor {
    let object = ffi::AVDRMObjectDescriptor {
        fd: -1,
        size: 0,
        format_modifier: 0,
    };
    let plane = ffi::AVDRMPlaneDescriptor {
        object_index: 0,
        offset: 0,
        pitch: 0,
    };
    let layer = ffi::AVDRMLayerDescriptor {
        format: 0,
        nb_planes: 0,
        planes: [plane; 4],
    };
    ffi::AVDRMFrameDescriptor {
        nb_objects: 0,
        objects: [object; 4],
        nb_layers: 0,
        layers: [layer; 4],
    }
}

enum CallStatus {
    Ready,
    Again,
    End,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReceiveStatus {
    Packet,
    Again,
    End,
}

fn status(code: i32) -> Result<CallStatus> {
    if code >= 0 {
        return Ok(CallStatus::Ready);
    }
    match FfmpegError::from(code) {
        FfmpegError::Other { errno } if errno == libc::EAGAIN => Ok(CallStatus::Again),
        FfmpegError::Eof => Ok(CallStatus::End),
        error => Err(error.into()),
    }
}

fn check(code: i32, operation: &'static str) -> Result<()> {
    if code >= 0 {
        Ok(())
    } else {
        Err(anyhow::Error::new(FfmpegError::from(code)).context(operation))
    }
}

#[cfg(test)]
mod settings_tests {
    use super::*;

    #[test]
    fn replacement_rate_preserves_other_settings_and_revalidates_limits() {
        let original =
            VaapiEncoderSettings::try_new(VideoCodec::Av1, 8_000_000, 30, 60).expect("settings");
        let reduced = original.with_bitrate(4_000_000).expect("lower rate");
        assert_eq!(reduced.codec, original.codec);
        assert_eq!(reduced.frames_per_second, original.frames_per_second);
        assert_eq!(reduced.keyframe_interval, original.keyframe_interval);
        assert_eq!(reduced.bitrate_bits, 4_000_000);
        assert_eq!(original.bitrate_bits, 8_000_000);
        assert!(original.with_bitrate(0).is_err());
        assert!(original.with_bitrate(8_000_001).is_err());
    }

    #[test]
    fn av1_settings_enforce_the_validated_driver_ceiling() {
        assert!(VaapiEncoderSettings::try_new(VideoCodec::Av1, 8_000_000, 60, 32).is_ok());
        assert!(VaapiEncoderSettings::try_new(VideoCodec::Av1, 8_000_001, 60, 32).is_err());
        assert!(VaapiEncoderSettings::try_new(VideoCodec::H264, 16_000_000, 60, 32).is_ok());
    }

    #[test]
    fn unsupported_or_degenerate_settings_fail_before_opening_hardware() {
        assert!(VaapiEncoderSettings::try_new(VideoCodec::Vp9, 8_000_000, 60, 32).is_err());
        assert!(VaapiEncoderSettings::try_new(VideoCodec::H264, 0, 60, 32).is_err());
        assert!(VaapiEncoderSettings::try_new(VideoCodec::H264, 8_000_000, 0, 32).is_err());
        assert!(VaapiEncoderSettings::try_new(VideoCodec::H264, 8_000_000, 60, 0).is_err());
    }
}
