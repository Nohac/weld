use std::{cell::RefCell, fs::File, num::NonZeroU16, rc::Rc};

use anyhow::{Context, Result, bail, ensure};
use cros_codecs::{
    BlockingMode, Fourcc, FrameLayout, PlaneLayout, Resolution,
    backend::vaapi::{decoder::VaapiBackend as VaapiDecoderBackend, encoder::VaapiBackend},
    decoder::{
        DecodedHandle, DecoderEvent, StreamInfo,
        stateless::{
            DecodeError, StatelessDecoder, StatelessVideoDecoder, h264::H264 as H264DecoderCodec,
        },
    },
    encoder::{
        FrameMetadata, PredictionStructure, RateControl, Tunings, VideoEncoder,
        h264::EncoderConfig, stateless::h264::StatelessEncoder,
    },
    libva::{
        Display, Surface, UsageHint, VA_FOURCC_NV12, VA_RT_FORMAT_YUV420, VAEntrypoint, VAProfile,
    },
    video_frame::{
        ReadMapping, VideoFrame, WriteMapping, generic_dma_video_frame::GenericDmaVideoFrame,
    },
};
use weld_media::{EncodedAccessUnit, EncodedFrameKind, MediaFrameId, VideoCodec as WeldVideoCodec};

use crate::dmabuf::{PrimeImportDescriptor, VaapiDmabuf};

type CrosH264Encoder = StatelessEncoder<
    EncodedInputFrame,
    VaapiBackend<PrimeImportDescriptor, Surface<PrimeImportDescriptor>>,
>;
type CrosH264Decoder =
    StatelessDecoder<H264DecoderCodec, VaapiDecoderBackend<GenericDmaVideoFrame>>;

const H264_MACROBLOCK_SIZE: u32 = 16;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum H264ReferenceMode {
    LowDelay,
    IndependentIdr,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct H264EncoderSettings {
    bitrate: u64,
    frames_per_second: u32,
    intra_period: u16,
    min_qp: u32,
    max_qp: u32,
    reference_mode: H264ReferenceMode,
}

impl H264EncoderSettings {
    pub fn try_new(
        bitrate: u64,
        frames_per_second: u32,
        intra_period: u16,
        min_qp: u32,
        max_qp: u32,
        reference_mode: H264ReferenceMode,
    ) -> Result<Self> {
        ensure!(bitrate > 0, "H.264 bitrate must be positive");
        ensure!(frames_per_second > 0, "H.264 frame rate must be positive");
        ensure!(
            intra_period >= 16 && intra_period.is_power_of_two(),
            "H.264 intra period must be a power of two of at least 16"
        );
        ensure!(
            (1..=51).contains(&min_qp) && (1..=51).contains(&max_qp) && min_qp <= max_qp,
            "H.264 QP range must be ordered within 1..=51"
        );
        Ok(Self {
            bitrate,
            frames_per_second,
            intra_period,
            min_qp,
            max_qp,
            reference_mode,
        })
    }

    pub const fn bitrate(self) -> u64 {
        self.bitrate
    }

    pub const fn frames_per_second(self) -> u32 {
        self.frames_per_second
    }

    pub const fn intra_period(self) -> u16 {
        self.intra_period
    }

    pub const fn min_qp(self) -> u32 {
        self.min_qp
    }

    pub const fn max_qp(self) -> u32 {
        self.max_qp
    }

    pub const fn reference_mode(self) -> H264ReferenceMode {
        self.reference_mode
    }
}

#[derive(Debug)]
struct EncodedInputFrame {
    frame: VaapiDmabuf,
}

impl VideoFrame for EncodedInputFrame {
    type MemDescriptor = PrimeImportDescriptor;
    type NativeHandle = Surface<PrimeImportDescriptor>;

    fn fourcc(&self) -> Fourcc {
        Fourcc::from(b"NV12")
    }

    fn resolution(&self) -> Resolution {
        Resolution {
            width: self.frame.width,
            height: self.frame.height,
        }
    }

    fn get_plane_size(&self) -> Vec<usize> {
        let height = usize::try_from(self.frame.height).unwrap_or(usize::MAX);
        self.get_plane_pitch()
            .into_iter()
            .enumerate()
            .map(|(index, pitch)| {
                let plane_height = if index == 0 {
                    height
                } else {
                    height.div_ceil(2)
                };
                pitch.saturating_mul(plane_height)
            })
            .collect()
    }

    fn get_plane_pitch(&self) -> Vec<usize> {
        self.frame
            .planes
            .iter()
            .map(|plane| usize::try_from(plane.stride).unwrap_or(usize::MAX))
            .collect()
    }

    fn map<'a>(&'a self) -> Result<Box<dyn ReadMapping<'a> + 'a>, String> {
        Err("DMA-BUF encoder input is not CPU-mappable".to_owned())
    }

    fn map_mut<'a>(&'a mut self) -> Result<Box<dyn WriteMapping<'a> + 'a>, String> {
        Err("DMA-BUF encoder input is not CPU-mappable".to_owned())
    }

    fn to_native_handle(&self, display: &Rc<Display>) -> Result<Self::NativeHandle, String> {
        let descriptor = self
            .frame
            .import_descriptor()
            .map_err(|error| error.to_string())?;
        display
            .create_surfaces(
                VA_RT_FORMAT_YUV420,
                Some(VA_FOURCC_NV12),
                self.frame.width,
                self.frame.height,
                Some(UsageHint::USAGE_HINT_ENCODER),
                vec![descriptor],
            )
            .map_err(|error| error.to_string())?
            .pop()
            .ok_or_else(|| "VA-API did not import the encoder input surface".to_owned())
    }
}

pub struct H264Encoder {
    encoder: CrosH264Encoder,
    repair_radeonsi_slice_header: bool,
    coded_resolution: Resolution,
    cadence: EncoderCadence,
}

struct EncoderCadence {
    intra_period: NonZeroU16,
    emitted_frames: u16,
    /// True while one submission is in flight and permanently after it fails.
    poisoned: bool,
}

impl EncoderCadence {
    fn new(intra_period: NonZeroU16) -> Result<Self> {
        let period = intra_period.get();
        ensure!(
            period >= 16 && period.is_power_of_two(),
            "H.264 intra period must be a power of two of at least 16"
        );
        Ok(Self {
            intra_period,
            emitted_frames: 0,
            poisoned: false,
        })
    }

    fn begin(&mut self) -> Result<EncodedFrameKind> {
        ensure!(!self.poisoned, "H.264 encoder session requires recreation");
        let kind = frame_kind(self.emitted_frames, self.intra_period);
        self.poisoned = true;
        Ok(kind)
    }

    fn complete(&mut self) {
        self.emitted_frames = (self.emitted_frames + 1) % self.intra_period.get();
        self.poisoned = false;
    }
}

impl H264Encoder {
    pub(crate) fn new(
        display: Rc<Display>,
        width: u32,
        height: u32,
        settings: H264EncoderSettings,
    ) -> Result<Self> {
        let intra_period = NonZeroU16::new(settings.intra_period())
            .context("H.264 intra period must be non-zero")?;
        let cadence = EncoderCadence::new(intra_period)?;
        let entrypoints = display
            .query_config_entrypoints(VAProfile::VAProfileH264ConstrainedBaseline)
            .context("could not query H.264 encoder entrypoints")?;
        let low_power = if entrypoints.contains(&VAEntrypoint::VAEntrypointEncSliceLP) {
            true
        } else if entrypoints.contains(&VAEntrypoint::VAEntrypointEncSlice) {
            false
        } else {
            bail!("VA-API device exposes no H.264 encode entrypoint");
        };
        let vendor = display
            .query_vendor_string()
            .map_err(anyhow::Error::msg)
            .context("could not identify the VA-API encoder driver")?;
        let repair_radeonsi_slice_header =
            vendor.contains("Mesa Gallium") && vendor.contains("radeonsi");
        let visible_resolution = Resolution { width, height };
        let coded_resolution = h264_coded_resolution(width, height)?;
        let config = EncoderConfig {
            resolution: visible_resolution,
            initial_tunings: Tunings {
                rate_control: RateControl::ConstantBitrate(settings.bitrate()),
                framerate: settings.frames_per_second(),
                min_quality: settings.min_qp(),
                max_quality: settings.max_qp(),
            },
            pred_structure: PredictionStructure::LowDelay {
                limit: intra_period.get(),
            },
            ..Default::default()
        };
        let encoder = CrosH264Encoder::new_vaapi(
            display,
            config,
            Fourcc::from(b"NV12"),
            coded_resolution,
            low_power,
            BlockingMode::Blocking,
        )
        .map_err(|error| anyhow::anyhow!("could not create VA-API H.264 encoder: {error}"))?;
        Ok(Self {
            encoder,
            repair_radeonsi_slice_header,
            coded_resolution,
            cadence,
        })
    }

    pub const fn coded_size(&self) -> (u32, u32) {
        (self.coded_resolution.width, self.coded_resolution.height)
    }

    pub fn encode(
        &mut self,
        frame: MediaFrameId,
        timestamp_micros: u64,
        input: &VaapiDmabuf,
    ) -> Result<EncodedAccessUnit> {
        if input.fourcc != u32::from_le_bytes(*b"NV12") {
            bail!("H.264 encoder input is not NV12");
        }
        ensure!(
            input.width == self.coded_resolution.width
                && input.height == self.coded_resolution.height,
            "H.264 encoder input extent differs from its stream generation"
        );
        let layout = frame_layout(input)?;
        let input = EncodedInputFrame {
            frame: input.try_clone()?,
        };
        let kind = self.cadence.begin()?;
        self.encoder
            .encode(
                FrameMetadata {
                    timestamp: timestamp_micros,
                    layout,
                    force_keyframe: false,
                },
                input,
            )
            .map_err(|error| anyhow::anyhow!("could not submit H.264 frame: {error}"))?;
        self.encoder
            .drain()
            .map_err(|error| anyhow::anyhow!("could not drain H.264 encoder: {error}"))?;
        let output = self
            .encoder
            .poll()
            .map_err(|error| anyhow::anyhow!("could not poll H.264 encoder: {error}"))?
            .context("H.264 encoder produced no access unit after draining")?;
        ensure!(
            output.metadata.timestamp == timestamp_micros,
            "H.264 encoder returned an unexpected frame timestamp"
        );
        ensure!(
            self.encoder
                .poll()
                .map_err(|error| anyhow::anyhow!("could not poll H.264 encoder: {error}"))?
                .is_none(),
            "H.264 encoder produced more than one access unit for one input"
        );
        let mut payload = normalize_annex_b(&output.bitstream);
        validate_and_repair_slice_headers(&mut payload, kind, self.repair_radeonsi_slice_header)?;
        if payload.is_empty() {
            bail!("H.264 encoder produced no access unit");
        }
        let access_unit = EncodedAccessUnit {
            frame,
            codec: WeldVideoCodec::H264,
            kind,
            timestamp_micros,
            payload,
        };
        self.cadence.complete();
        Ok(access_unit)
    }
}

pub(crate) fn h264_coded_resolution(width: u32, height: u32) -> Result<Resolution> {
    Ok(Resolution {
        width: align_h264_dimension(width)?,
        height: align_h264_dimension(height)?,
    })
}

fn align_h264_dimension(value: u32) -> Result<u32> {
    ensure!(value > 0, "H.264 frame has zero extent");
    value
        .checked_add(H264_MACROBLOCK_SIZE - 1)
        .map(|rounded| rounded / H264_MACROBLOCK_SIZE * H264_MACROBLOCK_SIZE)
        .context("H.264 coded extent overflow")
}

const fn frame_kind(emitted_frames: u16, intra_period: NonZeroU16) -> EncodedFrameKind {
    if emitted_frames.is_multiple_of(intra_period.get()) {
        EncodedFrameKind::Keyframe
    } else {
        EncodedFrameKind::Delta
    }
}

fn validate_and_repair_slice_headers(
    bitstream: &mut [u8],
    kind: EncodedFrameKind,
    repair_radeonsi: bool,
) -> Result<()> {
    // cros-codecs currently lets the VA driver synthesize the slice header.
    // Mesa radeonsi emits the correct slice bits but leaves the NAL type at
    // the reserved value zero. Repair only that invalid header; already-valid
    // driver output remains byte-for-byte unchanged.
    let repaired_header = match kind {
        EncodedFrameKind::Keyframe => 0x65,
        EncodedFrameKind::Delta => 0x41,
    };
    let mut index = 0;
    let mut saw_sps = false;
    let mut saw_pps = false;
    let mut slice_count = 0;
    while index + 4 < bitstream.len() {
        if bitstream[index..].starts_with(&[0, 0, 0, 1]) {
            let nal_header = index + 4;
            let nal_type = bitstream[nal_header] & 0x1f;
            match nal_type {
                7 => saw_sps = true,
                8 => saw_pps = true,
                0 | 1 | 5 => {
                    let end =
                        next_annex_b_start(bitstream, nal_header + 1).unwrap_or(bitstream.len());
                    let slice_type = parse_slice_type(&bitstream[nal_header + 1..end])?;
                    ensure!(
                        slice_matches_kind(slice_type, kind),
                        "H.264 slice type does not match the expected frame kind"
                    );
                    slice_count += 1;
                    ensure!(slice_count == 1, "H.264 access unit has multiple slices");
                    if nal_type != 0 {
                        ensure!(
                            matches!(
                                (kind, nal_type),
                                (EncodedFrameKind::Keyframe, 5) | (EncodedFrameKind::Delta, 1)
                            ),
                            "H.264 NAL type does not match the expected frame kind"
                        );
                        index = nal_header + 1;
                        continue;
                    }
                    ensure!(
                        repair_radeonsi,
                        "H.264 encoder emitted a reserved NAL type on an unknown driver"
                    );
                    ensure!(
                        kind != EncodedFrameKind::Keyframe || (saw_sps && saw_pps),
                        "reserved keyframe slice does not follow SPS and PPS"
                    );
                    bitstream[nal_header] = repaired_header;
                }
                _ => {}
            }
            index = nal_header + 1;
            continue;
        }
        index += 1;
    }
    ensure!(slice_count == 1, "H.264 access unit has no slice");
    Ok(())
}

fn next_annex_b_start(bitstream: &[u8], from: usize) -> Option<usize> {
    // `normalize_annex_b` has already canonicalized every prefix to four bytes.
    bitstream[from..]
        .windows(3)
        .position(|window| window == [0, 0, 1])
        .map(|offset| from + offset.saturating_sub(1))
}

fn parse_slice_type(escaped_rbsp: &[u8]) -> Result<u32> {
    let mut rbsp = Vec::with_capacity(escaped_rbsp.len());
    let mut zeroes = 0_u8;
    for byte in escaped_rbsp.iter().copied() {
        if zeroes >= 2 && byte == 3 {
            zeroes = 0;
            continue;
        }
        rbsp.push(byte);
        zeroes = if byte == 0 {
            zeroes.saturating_add(1)
        } else {
            0
        };
    }
    let mut reader = ExpGolombReader::new(&rbsp);
    let _first_macroblock = reader.read_unsigned()?;
    let slice_type = reader.read_unsigned()?;
    ensure!(
        slice_type <= 9,
        "H.264 slice type is outside its defined range"
    );
    Ok(slice_type)
}

const fn slice_matches_kind(slice_type: u32, kind: EncodedFrameKind) -> bool {
    match kind {
        EncodedFrameKind::Keyframe => slice_type % 5 == 2,
        EncodedFrameKind::Delta => slice_type.is_multiple_of(5),
    }
}

struct ExpGolombReader<'a> {
    bytes: &'a [u8],
    bit: usize,
}

impl<'a> ExpGolombReader<'a> {
    const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, bit: 0 }
    }

    fn read_unsigned(&mut self) -> Result<u32> {
        let mut leading_zeroes = 0_u32;
        while !self.read_bit()? {
            leading_zeroes = leading_zeroes
                .checked_add(1)
                .context("H.264 Exp-Golomb prefix overflow")?;
            ensure!(leading_zeroes < 32, "H.264 Exp-Golomb value is too large");
        }
        let mut suffix = 0_u32;
        for _ in 0..leading_zeroes {
            suffix = (suffix << 1) | u32::from(self.read_bit()?);
        }
        Ok(((1_u32 << leading_zeroes) - 1) + suffix)
    }

    fn read_bit(&mut self) -> Result<bool> {
        let byte = self
            .bytes
            .get(self.bit / 8)
            .context("H.264 slice header ended unexpectedly")?;
        let shift = 7 - (self.bit % 8);
        self.bit += 1;
        Ok((byte >> shift) & 1 != 0)
    }
}

fn normalize_annex_b(bitstream: &[u8]) -> Vec<u8> {
    let mut starts = Vec::new();
    let mut index = 0;
    while index + 3 <= bitstream.len() {
        if bitstream[index..].starts_with(&[0, 0, 1]) {
            let start = if index > 0 && bitstream[index - 1] == 0 {
                index - 1
            } else {
                index
            };
            if starts.last().copied() != Some(start) {
                starts.push(start);
            }
            index += 3;
        } else {
            index += 1;
        }
    }
    let mut normalized = Vec::new();
    for (position, start) in starts.iter().copied().enumerate() {
        let prefix = if bitstream[start..].starts_with(&[0, 0, 0, 1]) {
            4
        } else {
            3
        };
        let payload_start = start + prefix;
        let mut payload_end = starts.get(position + 1).copied().unwrap_or(bitstream.len());
        while payload_end > payload_start && bitstream[payload_end - 1] == 0 {
            payload_end -= 1;
        }
        if payload_end > payload_start {
            normalized.extend_from_slice(&[0, 0, 0, 1]);
            normalized.extend_from_slice(&bitstream[payload_start..payload_end]);
        }
    }
    normalized
}

fn frame_layout(frame: &VaapiDmabuf) -> Result<FrameLayout> {
    let modifier = frame.primary_modifier()?;
    let planes = frame
        .planes
        .iter()
        .map(|plane| {
            Ok(PlaneLayout {
                buffer_index: usize::from(plane.object_index),
                offset: usize::try_from(plane.offset)?,
                stride: usize::try_from(plane.stride)?,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(FrameLayout {
        format: (Fourcc::from(b"NV12"), modifier),
        size: Resolution {
            width: frame.width,
            height: frame.height,
        },
        planes,
    })
}

pub struct H264Decoder {
    display: Rc<Display>,
    decoder: CrosH264Decoder,
    stream_info: Option<StreamInfo>,
    allocation_count: u64,
}

pub struct DecodedH264Frame {
    pub timestamp_micros: u64,
    /// Valid display region within the macroblock-aligned decoded frame.
    pub display_width: u32,
    pub display_height: u32,
    pub frame: VaapiDmabuf,
}

impl H264Decoder {
    pub(crate) fn new(display: Rc<Display>) -> Result<Self> {
        let decoder = CrosH264Decoder::new_vaapi(display.clone(), BlockingMode::Blocking)
            .map_err(|error| anyhow::anyhow!("could not create VA-API H.264 decoder: {error:?}"))?;
        Ok(Self {
            display,
            decoder,
            stream_info: None,
            allocation_count: 0,
        })
    }

    pub const fn allocation_count(&self) -> u64 {
        self.allocation_count
    }

    pub fn decode(&mut self, access_unit: &EncodedAccessUnit) -> Result<Vec<DecodedH264Frame>> {
        if access_unit.codec != WeldVideoCodec::H264 {
            bail!("VA-API H.264 decoder received another codec");
        }
        let mut remaining = access_unit.payload.as_slice();
        let mut decoded = Vec::new();
        while !remaining.is_empty() {
            let mut made_progress = false;
            let allocation_error = RefCell::new(None);
            let allocation_info = self.stream_info.clone();
            let display = self.display.clone();
            let decode_result = {
                let allocation_count = &mut self.allocation_count;
                let mut allocate = || {
                    let info = allocation_info.as_ref()?;
                    match allocate_decoder_frame(&display, info) {
                        Ok(frame) => {
                            *allocation_count = allocation_count.saturating_add(1);
                            Some(frame)
                        }
                        Err(error) => {
                            *allocation_error.borrow_mut() = Some(error);
                            None
                        }
                    }
                };
                self.decoder
                    .decode(access_unit.timestamp_micros, remaining, &mut allocate)
            };
            match decode_result {
                Ok(consumed) => {
                    made_progress = consumed > 0;
                    remaining = &remaining[consumed..];
                }
                Err(DecodeError::CheckEvents) => {}
                Err(DecodeError::NotEnoughOutputBuffers(_)) => {
                    if let Some(error) = allocation_error.into_inner() {
                        return Err(error).context("could not allocate decoder output");
                    }
                }
                Err(error) => bail!("could not decode H.264 access unit: {error}"),
            }
            let (events, event_progress) = self.collect_events()?;
            made_progress |= event_progress;
            decoded.extend(events);
            ensure!(made_progress, "H.264 decoder made no progress");
        }
        self.decoder
            .finish_access_unit()
            .context("could not finish H.264 access unit")?;
        let (events, _) = self.collect_events()?;
        decoded.extend(events);
        Ok(decoded)
    }

    pub fn drain(&mut self) -> Result<Vec<DecodedH264Frame>> {
        self.decoder
            .flush()
            .context("could not flush the H.264 decoder")?;
        self.collect_events().map(|(frames, _)| frames)
    }

    fn collect_events(&mut self) -> Result<(Vec<DecodedH264Frame>, bool)> {
        let mut decoded = Vec::new();
        let mut made_progress = false;
        while let Some(event) = self.decoder.next_event() {
            made_progress = true;
            match event {
                DecoderEvent::FormatChanged => {
                    self.stream_info = self.decoder.stream_info().cloned();
                }
                DecoderEvent::FrameReady(handle) => {
                    handle
                        .sync()
                        .context("could not synchronize decoded frame")?;
                    let timestamp_micros = handle.timestamp();
                    let frame = handle.video_frame();
                    let surface = frame
                        .to_native_handle(&self.display)
                        .map_err(anyhow::Error::msg)
                        .context("could not import decoded frame for export")?;
                    surface
                        .sync()
                        .context("could not synchronize decoded VA surface")?;
                    decoded.push(DecodedH264Frame {
                        timestamp_micros,
                        display_width: self
                            .stream_info
                            .as_ref()
                            .context("decoder produced a frame before stream information")?
                            .display_resolution
                            .width,
                        display_height: self
                            .stream_info
                            .as_ref()
                            .context("decoder produced a frame before stream information")?
                            .display_resolution
                            .height,
                        frame: VaapiDmabuf::from_prime(
                            surface
                                .export_prime()
                                .context("could not export decoded NV12 DMA-BUF")?,
                        )?,
                    });
                }
            }
        }
        Ok((decoded, made_progress))
    }
}

fn allocate_decoder_frame(
    display: &Rc<Display>,
    info: &StreamInfo,
) -> Result<GenericDmaVideoFrame> {
    let mut surfaces = display
        .create_surfaces(
            VA_RT_FORMAT_YUV420,
            Some(VA_FOURCC_NV12),
            info.coded_resolution.width,
            info.coded_resolution.height,
            Some(UsageHint::USAGE_HINT_DECODER | UsageHint::USAGE_HINT_EXPORT),
            vec![()],
        )
        .context("could not allocate decoder VA surface")?;
    let surface = surfaces
        .pop()
        .context("VA-API did not create a decoder surface")?;
    let descriptor = surface
        .export_prime()
        .context("could not export decoder surface")?;
    let layer = descriptor
        .layers
        .first()
        .context("decoder surface export has no layer")?;
    let plane_count = usize::try_from(layer.num_planes)?;
    let object_indices = layer.object_index;
    let offsets = layer.offset;
    let pitches = layer.pitch;
    let modifier = descriptor
        .objects
        .first()
        .map(|object| object.drm_format_modifier)
        .context("decoder surface export has no object")?;
    let fourcc = descriptor.fourcc;
    let files = descriptor
        .objects
        .into_iter()
        .map(|object| File::from(object.fd))
        .collect::<Vec<_>>();
    let planes = (0..plane_count)
        .map(|index| -> Result<PlaneLayout> {
            Ok(PlaneLayout {
                buffer_index: usize::from(object_indices[index]),
                offset: usize::try_from(offsets[index])?,
                stride: usize::try_from(pitches[index])?,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    GenericDmaVideoFrame::new(
        files,
        FrameLayout {
            format: (Fourcc::from(fourcc), modifier),
            size: info.coded_resolution,
            planes,
        },
    )
    .map_err(anyhow::Error::msg)
}

#[cfg(test)]
mod tests {
    use weld_media::EncodedFrameKind;

    use super::{
        EncoderCadence, ExpGolombReader, H264EncoderSettings, H264ReferenceMode, frame_kind,
        h264_coded_resolution, normalize_annex_b, parse_slice_type, slice_matches_kind,
        validate_and_repair_slice_headers,
    };
    use std::num::NonZeroU16;

    #[test]
    fn canonicalizes_start_codes_and_removes_inter_unit_padding() {
        let encoded = [0, 0, 1, 0x67, 0x11, 0, 0, 0, 0, 1, 0x68, 0x22, 0, 0];
        assert_eq!(
            normalize_annex_b(&encoded),
            [0, 0, 0, 1, 0x67, 0x11, 0, 0, 0, 1, 0x68, 0x22]
        );
    }

    #[test]
    fn repairs_a_reserved_keyframe_slice_header() {
        let encoded = [
            0, 0, 0, 1, 0x67, 0x11, 0, 0, 0, 1, 0x68, 0x22, 0, 0, 0, 1, 0, 0x88, 0x82,
        ];
        let mut encoded = encoded.to_vec();
        validate_and_repair_slice_headers(&mut encoded, EncodedFrameKind::Keyframe, true)
            .expect("known radeonsi keyframe repair");
        assert_eq!(
            encoded,
            [
                0, 0, 0, 1, 0x67, 0x11, 0, 0, 0, 1, 0x68, 0x22, 0, 0, 0, 1, 0x65, 0x88, 0x82
            ]
        );
    }

    #[test]
    fn rejects_a_reserved_slice_header_from_an_unknown_driver() {
        let mut encoded = [0, 0, 0, 1, 0];
        assert!(
            validate_and_repair_slice_headers(&mut encoded, EncodedFrameKind::Keyframe, false)
                .is_err()
        );
    }

    #[test]
    fn generic_frame_kind_policy_wraps_at_any_nonzero_period() {
        let kinds = (0..10)
            .map(|frame| frame_kind(frame, NonZeroU16::new(4).expect("nonzero period")))
            .collect::<Vec<_>>();

        assert_eq!(
            kinds,
            [
                EncodedFrameKind::Keyframe,
                EncodedFrameKind::Delta,
                EncodedFrameKind::Delta,
                EncodedFrameKind::Delta,
                EncodedFrameKind::Keyframe,
                EncodedFrameKind::Delta,
                EncodedFrameKind::Delta,
                EncodedFrameKind::Delta,
                EncodedFrameKind::Keyframe,
                EncodedFrameKind::Delta,
            ]
        );
    }

    #[test]
    fn encoder_cadence_validates_configuration_and_poisoning() {
        for period in [4, 8, 24] {
            assert!(EncoderCadence::new(NonZeroU16::new(period).expect("nonzero period")).is_err());
        }
        assert!(EncoderCadence::new(NonZeroU16::new(16).expect("nonzero period")).is_ok());
        assert!(EncoderCadence::new(NonZeroU16::new(32).expect("nonzero period")).is_ok());

        let mut cadence = EncoderCadence::new(NonZeroU16::new(16).expect("nonzero period"))
            .expect("valid cadence");
        assert_eq!(
            cadence.begin().expect("fresh cadence"),
            EncodedFrameKind::Keyframe
        );
        assert!(cadence.begin().is_err());
        cadence.complete();
        assert_eq!(
            cadence.begin().expect("completed cadence"),
            EncodedFrameKind::Delta
        );
        let recreated = EncoderCadence::new(NonZeroU16::new(16).expect("nonzero period"))
            .expect("valid cadence");
        assert_eq!(
            frame_kind(recreated.emitted_frames, recreated.intra_period),
            EncodedFrameKind::Keyframe
        );
    }

    #[test]
    fn encoder_settings_reject_values_the_hardware_policy_would_rewrite() {
        let settings = |bitrate, fps, period, min_qp, max_qp| {
            H264EncoderSettings::try_new(
                bitrate,
                fps,
                period,
                min_qp,
                max_qp,
                H264ReferenceMode::LowDelay,
            )
        };

        assert!(settings(0, 60, 32, 18, 36).is_err());
        assert!(settings(16_000_000, 0, 32, 18, 36).is_err());
        assert!(settings(16_000_000, 60, 24, 18, 36).is_err());
        assert!(settings(16_000_000, 60, 32, 0, 36).is_err());
        assert!(settings(16_000_000, 60, 32, 18, 52).is_err());
        assert!(settings(16_000_000, 60, 32, 36, 18).is_err());
        assert!(settings(16_000_000, 60, 32, 18, 36).is_ok());
    }

    #[test]
    fn coded_resolution_preserves_visible_macroblock_crop() {
        let aligned = h264_coded_resolution(320, 192).expect("aligned extent");
        assert_eq!((aligned.width, aligned.height), (320, 192));

        let cropped = h264_coded_resolution(944, 484).expect("cropped extent");
        assert_eq!((cropped.width, cropped.height), (944, 496));
        assert!(h264_coded_resolution(0, 484).is_err());
        assert!(h264_coded_resolution(u32::MAX, 484).is_err());
    }

    #[test]
    fn parses_h264_slice_types_independently_of_the_nal_header() {
        assert_eq!(parse_slice_type(&[0xb0]).expect("I slice"), 2);
        assert_eq!(parse_slice_type(&[0xc0]).expect("P slice"), 0);
        assert!(slice_matches_kind(2, EncodedFrameKind::Keyframe));
        assert!(slice_matches_kind(0, EncodedFrameKind::Delta));
        assert!(!slice_matches_kind(0, EncodedFrameKind::Keyframe));
    }

    #[test]
    fn exp_golomb_reader_rejects_truncated_values() {
        assert!(ExpGolombReader::new(&[0]).read_unsigned().is_err());
    }
}
