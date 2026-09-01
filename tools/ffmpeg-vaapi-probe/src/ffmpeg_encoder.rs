use std::{
    error::Error,
    ffi::{CStr, CString, c_void},
    fmt, mem,
    os::fd::{AsRawFd, OwnedFd},
    path::Path,
    ptr,
    str::FromStr,
};

use anyhow::{Context, Result, bail, ensure};
use ffmpeg_next::{Error as FfmpegError, ffi};
use weld_media_vaapi::{VaapiDmabuf, VaapiDmabufObject, VaapiDmabufPlane};

const FILTER_DESCRIPTION: &str = "hwmap=derive_device=vaapi:mode=read,scale_vaapi=format=nv12:out_color_matrix=bt709:out_range=tv";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum VideoCodec {
    Av1,
    H264,
}

impl VideoCodec {
    pub(crate) const fn name(self) -> &'static str {
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

    pub(crate) const fn output_file(self) -> &'static str {
        match self {
            Self::Av1 => "ffmpeg.ivf",
            Self::H264 => "ffmpeg.h264",
        }
    }

    const fn profile(self) -> i32 {
        match self {
            Self::Av1 => ffi::AV_PROFILE_AV1_MAIN,
            Self::H264 => ffi::AV_PROFILE_H264_CONSTRAINED_BASELINE,
        }
    }
}

#[derive(Debug)]
pub(crate) struct ParseVideoCodecError(String);

impl fmt::Display for ParseVideoCodecError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "unsupported probe codec {}; expected av1 or h264",
            self.0
        )
    }
}

impl Error for ParseVideoCodecError {}

impl FromStr for VideoCodec {
    type Err = ParseVideoCodecError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "av1" => Ok(Self::Av1),
            "h264" => Ok(Self::H264),
            other => Err(ParseVideoCodecError(other.to_owned())),
        }
    }
}

pub(crate) struct EncodedOutput {
    pub(crate) payload: Vec<u8>,
    pub(crate) packet_count: u64,
    pub(crate) filter_description: &'static str,
    pub(crate) encoder_name: &'static str,
}

pub(crate) struct FfmpegEncoder {
    codec_kind: VideoCodec,
    filter: FilterPipeline,
    codec: CodecContext,
    packet: Packet,
    payload: Vec<u8>,
    packet_count: u64,
}

impl FfmpegEncoder {
    pub(crate) fn new(
        codec_kind: VideoCodec,
        render_node: &Path,
        width: u32,
        height: u32,
        frames_per_second: u32,
        bitrate_bits: u64,
        keyframe_interval: u32,
    ) -> Result<Self> {
        let width = i32::try_from(width)?;
        let height = i32::try_from(height)?;
        let frames_per_second = i32::try_from(frames_per_second)?;
        let filter = FilterPipeline::new(render_node, width, height, frames_per_second)?;
        let codec = CodecContext::new(
            codec_kind,
            filter.output_frames_context()?,
            width,
            height,
            frames_per_second,
            i64::try_from(bitrate_bits)?,
            i32::try_from(keyframe_interval)?,
        )?;
        Ok(Self {
            codec_kind,
            filter,
            codec,
            packet: Packet::new()?,
            payload: codec_kind.stream_header(width, height, frames_per_second)?,
            packet_count: 0,
        })
    }

    pub(crate) fn push(&mut self, input: VaapiDmabuf, presentation_time: i64) -> Result<()> {
        let input = self.filter.input_frame(input, presentation_time)?;
        self.filter.push(input)?;
        while let Some(frame) = self.filter.pull()? {
            self.codec.send(Some(&frame))?;
            self.drain_packets(false)?;
        }
        Ok(())
    }

    pub(crate) fn finish(mut self) -> Result<EncodedOutput> {
        self.filter.flush()?;
        while let Some(frame) = self.filter.pull()? {
            self.codec.send(Some(&frame))?;
            self.drain_packets(false)?;
        }
        self.codec.send(None)?;
        self.drain_packets(true)?;
        ensure!(self.packet_count > 0, "FFmpeg emitted no encoded packets");
        ensure!(!self.payload.is_empty(), "FFmpeg encoded output is empty");
        self.codec_kind
            .finish_stream(&mut self.payload, self.packet_count)?;
        Ok(EncodedOutput {
            payload: mem::take(&mut self.payload),
            packet_count: self.packet_count,
            filter_description: FILTER_DESCRIPTION,
            encoder_name: self.codec_kind.encoder_name().to_str()?,
        })
    }

    fn drain_packets(&mut self, expect_end: bool) -> Result<()> {
        loop {
            match self.codec.receive(&mut self.packet)? {
                ReceiveStatus::Packet => {
                    self.codec_kind
                        .append_packet(&mut self.payload, &self.packet)?;
                    self.packet_count = self
                        .packet_count
                        .checked_add(1)
                        .context("FFmpeg packet count overflow")?;
                    self.packet.clear();
                }
                ReceiveStatus::Again if !expect_end => return Ok(()),
                ReceiveStatus::End if expect_end => return Ok(()),
                ReceiveStatus::Again => bail!("FFmpeg encoder requested input while flushing"),
                ReceiveStatus::End => bail!("FFmpeg encoder ended before flush"),
            }
        }
    }
}

impl VideoCodec {
    fn stream_header(self, width: i32, height: i32, fps: i32) -> Result<Vec<u8>> {
        if self == Self::H264 {
            return Ok(Vec::new());
        }
        let width = u16::try_from(width)?;
        let height = u16::try_from(height)?;
        let fps = u32::try_from(fps)?;
        let mut header = Vec::with_capacity(32);
        header.extend_from_slice(b"DKIF");
        header.extend_from_slice(&0_u16.to_le_bytes());
        header.extend_from_slice(&32_u16.to_le_bytes());
        header.extend_from_slice(b"AV01");
        header.extend_from_slice(&width.to_le_bytes());
        header.extend_from_slice(&height.to_le_bytes());
        header.extend_from_slice(&fps.to_le_bytes());
        header.extend_from_slice(&1_u32.to_le_bytes());
        header.extend_from_slice(&0_u32.to_le_bytes());
        header.extend_from_slice(&0_u32.to_le_bytes());
        Ok(header)
    }

    fn append_packet(self, stream: &mut Vec<u8>, packet: &Packet) -> Result<()> {
        let bytes = packet.bytes()?;
        if self == Self::Av1 {
            stream.extend_from_slice(&u32::try_from(bytes.len())?.to_le_bytes());
            stream.extend_from_slice(&u64::try_from(packet.presentation_time()?)?.to_le_bytes());
        }
        stream.extend_from_slice(bytes);
        Ok(())
    }

    fn finish_stream(self, stream: &mut [u8], packet_count: u64) -> Result<()> {
        if self == Self::Av1 {
            let frame_count = u32::try_from(packet_count)?.to_le_bytes();
            let destination = stream
                .get_mut(24..28)
                .context("AV1 IVF header is incomplete")?;
            destination.copy_from_slice(&frame_count);
        }
        Ok(())
    }
}

struct FilterPipeline {
    graph: *mut ffi::AVFilterGraph,
    source: *mut ffi::AVFilterContext,
    sink: *mut ffi::AVFilterContext,
    _drm_device: BufferRef,
    drm_frames: BufferRef,
}

impl FilterPipeline {
    fn new(render_node: &Path, width: i32, height: i32, fps: i32) -> Result<Self> {
        let render_node = CString::new(render_node.as_os_str().as_encoded_bytes())?;
        let drm_device =
            BufferRef::hardware_device(ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_DRM, &render_node)?;
        let drm_frames = BufferRef::drm_frames(&drm_device, width, height)?;
        // SAFETY: allocating an independent FFmpeg filter graph has no caller-side preconditions.
        let graph = unsafe { ffi::avfilter_graph_alloc() };
        ensure!(!graph.is_null(), "could not allocate FFmpeg filter graph");

        let setup = (|| {
            let source = allocate_filter(graph, c"buffer", c"weld-dmabuf-source")?;
            let sink = create_filter(graph, c"buffersink", c"weld-vaapi-sink")?;
            configure_source(source, &drm_frames, width, height, fps)?;
            // SAFETY: source has received all mandatory buffer parameters and is not initialized yet.
            let result = unsafe { ffi::avfilter_init_str(source, ptr::null()) };
            check(result, "could not initialize FFmpeg DMA-BUF source")?;
            parse_filter_graph(graph, source, sink, FILTER_DESCRIPTION)?;
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

    fn flush(&mut self) -> Result<()> {
        // SAFETY: a null frame is FFmpeg's documented end-of-stream marker for buffersrc.
        let result = unsafe { ffi::av_buffersrc_add_frame_flags(self.source, ptr::null_mut(), 0) };
        check(result, "could not flush FFmpeg filter source")
    }
}

impl Drop for FilterPipeline {
    fn drop(&mut self) {
        // SAFETY: graph is uniquely owned and freeing it invalidates source and sink together.
        unsafe { ffi::avfilter_graph_free(&mut self.graph) };
    }
}

struct CodecContext(*mut ffi::AVCodecContext);

impl CodecContext {
    fn new(
        codec_kind: VideoCodec,
        frames: *mut ffi::AVBufferRef,
        width: i32,
        height: i32,
        fps: i32,
        bitrate: i64,
        keyframe_interval: i32,
    ) -> Result<Self> {
        // SAFETY: the static C string is terminated and remains valid for the call.
        let encoder_name = codec_kind.encoder_name();
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
            (*context).width = width;
            (*context).height = height;
            (*context).time_base = ffi::AVRational { num: 1, den: fps };
            (*context).framerate = ffi::AVRational { num: fps, den: 1 };
            (*context).sample_aspect_ratio = ffi::AVRational { num: 1, den: 1 };
            (*context).pix_fmt = ffi::AVPixelFormat::AV_PIX_FMT_VAAPI;
            (*context).profile = codec_kind.profile();
            (*context).bit_rate = bitrate;
            (*context).rc_min_rate = bitrate;
            (*context).rc_max_rate = bitrate;
            (*context).rc_buffer_size = i32::try_from(bitrate)?;
            (*context).gop_size = keyframe_interval;
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
        if codec_kind == VideoCodec::H264 {
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
        frame_ref.time_base = ffi::AVRational { num: 1, den: 60 };
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
}

impl Drop for Frame {
    fn drop(&mut self) {
        // SAFETY: self.0 is uniquely owned and may be nullified by FFmpeg.
        unsafe { ffi::av_frame_free(&mut self.0) };
    }
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
        (*parameters).time_base = ffi::AVRational { num: 1, den: fps };
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

fn parse_filter_graph(
    graph: *mut ffi::AVFilterGraph,
    source: *mut ffi::AVFilterContext,
    sink: *mut ffi::AVFilterContext,
    description: &str,
) -> Result<()> {
    let description = CString::new(description)?;
    let mut inputs = filter_endpoint(c"out", sink)?;
    let mut outputs = filter_endpoint(c"in", source)?;
    // SAFETY: graph and endpoints are live and description is terminated for the call.
    let result = unsafe {
        ffi::avfilter_graph_parse_ptr(
            graph,
            description.as_ptr(),
            &mut inputs,
            &mut outputs,
            ptr::null_mut(),
        )
    };
    // SAFETY: parse either consumed or returned the endpoint lists; both pointers are valid for free.
    unsafe {
        ffi::avfilter_inout_free(&mut inputs);
        ffi::avfilter_inout_free(&mut outputs);
    }
    check(result, "could not parse FFmpeg hardware filter graph")
}

fn filter_endpoint(
    name: &CStr,
    context: *mut ffi::AVFilterContext,
) -> Result<*mut ffi::AVFilterInOut> {
    // SAFETY: endpoint allocation has no caller-side preconditions.
    let endpoint = unsafe { ffi::avfilter_inout_alloc() };
    ensure!(
        !endpoint.is_null(),
        "could not allocate FFmpeg filter endpoint"
    );
    // SAFETY: endpoint is uniquely owned and context remains graph-owned.
    unsafe {
        (*endpoint).name = ffi::av_strdup(name.as_ptr());
        (*endpoint).filter_ctx = context;
        (*endpoint).pad_idx = 0;
        (*endpoint).next = ptr::null_mut();
    }
    // SAFETY: endpoint remains live until graph parsing.
    ensure!(
        unsafe { !(*endpoint).name.is_null() },
        "could not copy FFmpeg filter endpoint name"
    );
    Ok(endpoint)
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
