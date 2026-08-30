use std::{
    cell::RefCell,
    fs::File,
    path::{Path, PathBuf},
    rc::Rc,
};

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
}

impl H264Encoder {
    pub fn open(
        render_node: impl AsRef<Path>,
        width: u32,
        height: u32,
        bitrate: u64,
        frames_per_second: u32,
    ) -> Result<Self> {
        let display = Display::open_drm_display(render_node.as_ref()).with_context(|| {
            format!(
                "could not open VA-API display {}",
                render_node.as_ref().display()
            )
        })?;
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
        let resolution = Resolution { width, height };
        let config = EncoderConfig {
            resolution,
            initial_tunings: Tunings {
                rate_control: RateControl::ConstantBitrate(bitrate),
                framerate: frames_per_second,
                min_quality: 18,
                max_quality: 36,
            },
            pred_structure: PredictionStructure::LowDelay { limit: 60 },
            ..Default::default()
        };
        let encoder = CrosH264Encoder::new_vaapi(
            display,
            config,
            Fourcc::from(b"NV12"),
            resolution,
            low_power,
            BlockingMode::Blocking,
        )
        .map_err(|error| anyhow::anyhow!("could not create VA-API H.264 encoder: {error}"))?;
        Ok(Self {
            encoder,
            repair_radeonsi_slice_header,
        })
    }

    pub fn encode_one(
        mut self,
        frame: MediaFrameId,
        timestamp_micros: u64,
        input: &VaapiDmabuf,
    ) -> Result<EncodedAccessUnit> {
        if input.fourcc != u32::from_le_bytes(*b"NV12") {
            bail!("H.264 encoder input is not NV12");
        }
        self.encoder
            .encode(
                FrameMetadata {
                    timestamp: timestamp_micros,
                    layout: frame_layout(input)?,
                    force_keyframe: true,
                },
                EncodedInputFrame {
                    frame: input.try_clone()?,
                },
            )
            .map_err(|error| anyhow::anyhow!("could not submit H.264 frame: {error}"))?;
        self.encoder
            .drain()
            .map_err(|error| anyhow::anyhow!("could not drain H.264 encoder: {error}"))?;
        let mut payload = Vec::new();
        while let Some(output) = self
            .encoder
            .poll()
            .map_err(|error| anyhow::anyhow!("could not poll H.264 encoder: {error}"))?
        {
            payload.extend(normalize_annex_b(&output.bitstream));
        }
        repair_reserved_slice_headers(
            &mut payload,
            EncodedFrameKind::Keyframe,
            self.repair_radeonsi_slice_header,
        )?;
        if payload.is_empty() {
            bail!("H.264 encoder produced no access unit");
        }
        Ok(EncodedAccessUnit {
            frame,
            codec: WeldVideoCodec::H264,
            kind: EncodedFrameKind::Keyframe,
            timestamp_micros,
            payload,
        })
    }
}

fn repair_reserved_slice_headers(
    bitstream: &mut [u8],
    kind: EncodedFrameKind,
    repair_radeonsi: bool,
) -> Result<()> {
    // cros-codecs currently lets the VA driver synthesize the slice header.
    // Mesa radeonsi emits the correct slice bits but leaves the NAL type at
    // the reserved value zero. Repair only that invalid header; already-valid
    // driver output remains byte-for-byte unchanged.
    let header = match kind {
        EncodedFrameKind::Keyframe => 0x65,
        EncodedFrameKind::Delta => 0x41,
    };
    let mut index = 0;
    let mut saw_sps = false;
    let mut saw_pps = false;
    let mut repaired = 0;
    while index + 4 < bitstream.len() {
        if bitstream[index..].starts_with(&[0, 0, 0, 1]) {
            let nal_header = index + 4;
            match bitstream[nal_header] & 0x1f {
                7 => saw_sps = true,
                8 => saw_pps = true,
                0 => {
                    ensure!(
                        repair_radeonsi,
                        "H.264 encoder emitted a reserved NAL type on an unknown driver"
                    );
                    ensure!(
                        kind != EncodedFrameKind::Keyframe || (saw_sps && saw_pps),
                        "reserved keyframe slice does not follow SPS and PPS"
                    );
                    repaired += 1;
                    ensure!(
                        repaired == 1,
                        "H.264 access unit has multiple reserved NAL types"
                    );
                    bitstream[nal_header] = header;
                }
                _ => {}
            }
            index = nal_header + 1;
            continue;
        }
        index += 1;
    }
    Ok(())
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

pub fn decode_h264_frame(
    render_node: impl AsRef<Path>,
    access_unit: &EncodedAccessUnit,
) -> Result<VaapiDmabuf> {
    if access_unit.codec != WeldVideoCodec::H264 {
        bail!("VA-API H.264 decoder received another codec");
    }
    let render_node = render_node.as_ref().to_path_buf();
    let display = Display::open_drm_display(&render_node)
        .with_context(|| format!("could not open VA-API display {}", render_node.display()))?;
    let mut decoder = CrosH264Decoder::new_vaapi(display.clone(), BlockingMode::Blocking)
        .map_err(|error| anyhow::anyhow!("could not create VA-API H.264 decoder: {error:?}"))?;
    let mut stream_info = None;
    let mut remaining = access_unit.payload.as_slice();
    let mut decoded = None;
    while !remaining.is_empty() || decoded.is_none() {
        let mut made_progress = false;
        let allocation_error = RefCell::new(None);
        let allocation_info = stream_info.clone();
        let allocation_node = render_node.clone();
        let mut allocate = || {
            let info = allocation_info.as_ref()?;
            match allocate_decoder_frame(&allocation_node, info) {
                Ok(frame) => Some(frame),
                Err(error) => {
                    *allocation_error.borrow_mut() = Some(error);
                    None
                }
            }
        };
        match decoder.decode(access_unit.timestamp_micros, remaining, &mut allocate) {
            Ok(consumed) => {
                if consumed == 0 && remaining.is_empty() {
                    break;
                }
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
        while let Some(event) = decoder.next_event() {
            made_progress = true;
            match event {
                DecoderEvent::FormatChanged => {
                    stream_info = decoder.stream_info().cloned();
                }
                DecoderEvent::FrameReady(handle) => {
                    handle
                        .sync()
                        .context("could not synchronize decoded frame")?;
                    decoded = Some(handle.video_frame());
                }
            }
        }
        if remaining.is_empty() {
            if decoded.is_none() {
                decoder
                    .flush()
                    .context("could not flush the H.264 decoder")?;
                while let Some(event) = decoder.next_event() {
                    if let DecoderEvent::FrameReady(handle) = event {
                        handle
                            .sync()
                            .context("could not synchronize decoded frame")?;
                        decoded = Some(handle.video_frame());
                    }
                }
            }
            break;
        }
        ensure!(made_progress, "H.264 decoder made no progress");
    }
    let frame = decoded.context("H.264 decoder produced no frame")?;
    let surface = frame
        .to_native_handle(&display)
        .map_err(anyhow::Error::msg)
        .context("could not import decoded frame for export")?;
    surface
        .sync()
        .context("could not synchronize decoded VA surface")?;
    VaapiDmabuf::from_prime(
        surface
            .export_prime()
            .context("could not export decoded NV12 DMA-BUF")?,
    )
}

fn allocate_decoder_frame(
    render_node: &PathBuf,
    info: &StreamInfo,
) -> Result<GenericDmaVideoFrame> {
    let display = Display::open_drm_display(render_node)
        .with_context(|| format!("could not open decoder allocator {}", render_node.display()))?;
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

    use super::{normalize_annex_b, repair_reserved_slice_headers};

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
        repair_reserved_slice_headers(&mut encoded, EncodedFrameKind::Keyframe, true)
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
            repair_reserved_slice_headers(&mut encoded, EncodedFrameKind::Keyframe, false).is_err()
        );
    }
}
