//! Codec-independent hoist adapter contracts and optional VA-API binding.

use anyhow::Result;
use weld_core::dmabuf::ExternalDmabuf;
use weld_media::{EncodedAccessUnit, MediaFrameId, MediaStreamId, StreamGeneration};

pub enum EncodeInput {
    Dmabuf(ExternalDmabuf),
    PackedBgra {
        width: u32,
        height: u32,
        pixels: Vec<u8>,
    },
}

pub struct EncodeRequest {
    pub token: u64,
    pub frame: MediaFrameId,
    pub timestamp_micros: u64,
    pub input: EncodeInput,
}

pub struct EncodeCompletion {
    pub token: u64,
    pub result: Result<EncodedAccessUnit>,
}

pub struct DecodeRequest {
    pub token: u64,
    pub access_unit: EncodedAccessUnit,
    pub visible_width: u32,
    pub visible_height: u32,
}

pub struct DecodedFrame {
    pub frame: MediaFrameId,
    pub dmabuf: ExternalDmabuf,
}

pub struct DecodeCompletion {
    pub token: u64,
    pub result: Result<Vec<DecodedFrame>>,
}

pub enum SubmitError<T> {
    Busy(T),
    Stopped(T),
    Rejected(anyhow::Error),
}

pub trait EncodeBackend {
    fn try_submit(&mut self, request: EncodeRequest) -> Result<(), SubmitError<EncodeRequest>>;
    fn drain(&mut self) -> Vec<EncodeCompletion>;
    fn retire(&mut self, stream: MediaStreamId, generation: StreamGeneration) -> Result<()>;
}

pub trait DecodeBackend {
    fn try_submit(&mut self, request: DecodeRequest) -> Result<(), SubmitError<DecodeRequest>>;
    fn drain(&mut self) -> Vec<DecodeCompletion>;
    fn retire(&mut self, stream: MediaStreamId, generation: StreamGeneration) -> Result<()>;
}

#[cfg(feature = "vaapi")]
mod vaapi {
    use std::path::PathBuf;

    use anyhow::{Context, Result, ensure};
    use weld_core::dmabuf::{ExternalDmabuf, ExternalDmabufCapabilities, ExternalDmabufPlane};
    use weld_media::{MediaStreamId, StreamGeneration, VideoCodec};
    use weld_media_vaapi::{
        VaapiDecodeRequest, VaapiDecodeWorker, VaapiDmabuf, VaapiDmabufObject, VaapiDmabufPlane,
        VaapiEncodeInput, VaapiEncodeRequest, VaapiEncodeWorker, VaapiEncoderSettings,
        VaapiWorkerSubmitError,
    };

    use super::{
        DecodeBackend, DecodeCompletion, DecodeRequest, DecodedFrame, EncodeBackend,
        EncodeCompletion, EncodeInput, EncodeRequest, SubmitError,
    };

    const H264_BITRATE: u64 = 16_000_000;
    const AV1_BITRATE: u64 = 8_000_000;
    const DEFAULT_FRAMES_PER_SECOND: u32 = 60;
    const DEFAULT_KEYFRAME_INTERVAL: u32 = 32;
    const DRM_FORMAT_XRGB8888: u32 = u32::from_le_bytes(*b"XR24");

    pub fn encode_backend(
        render_node: PathBuf,
        codec: VideoCodec,
        dump_directory: Option<PathBuf>,
        notify: impl Fn() + Send + Sync + 'static,
    ) -> Result<Box<dyn EncodeBackend>> {
        Ok(Box::new(VaapiEncoder {
            worker: VaapiEncodeWorker::spawn(render_node, dump_directory, notify)?,
            settings: encoder_settings(codec)?,
        }))
    }

    pub fn decode_backend(
        capabilities: &ExternalDmabufCapabilities,
        codec: VideoCodec,
        notify: impl Fn() + Send + Sync + 'static,
    ) -> Result<Box<dyn DecodeBackend>> {
        let xrgb_modifiers = capabilities
            .formats
            .iter()
            .filter_map(|format| (format.fourcc == DRM_FORMAT_XRGB8888).then_some(format.modifier))
            .collect::<Vec<_>>();
        ensure!(
            !xrgb_modifiers.is_empty(),
            "Weld exposes no XRGB DMA-BUF modifier for decoded video"
        );
        Ok(Box::new(VaapiDecoder {
            worker: VaapiDecodeWorker::spawn(capabilities.render_node.clone(), notify)?,
            codec,
            xrgb_modifiers,
        }))
    }

    struct VaapiEncoder {
        worker: VaapiEncodeWorker,
        settings: VaapiEncoderSettings,
    }

    impl EncodeBackend for VaapiEncoder {
        fn try_submit(&mut self, request: EncodeRequest) -> Result<(), SubmitError<EncodeRequest>> {
            let EncodeRequest {
                token,
                frame,
                timestamp_micros,
                input,
            } = request;
            let input = match input {
                EncodeInput::Dmabuf(dmabuf) => VaapiEncodeInput::Dmabuf(
                    to_vaapi_dmabuf(dmabuf).map_err(SubmitError::Rejected)?,
                ),
                EncodeInput::PackedBgra {
                    width,
                    height,
                    pixels,
                } => VaapiEncodeInput::PackedBgra {
                    width,
                    height,
                    pixels,
                },
            };
            let request = VaapiEncodeRequest {
                token,
                frame,
                timestamp_micros,
                settings: self.settings,
                input,
            };
            match self.worker.try_encode(request) {
                Ok(()) => Ok(()),
                Err(VaapiWorkerSubmitError::Busy(request)) => {
                    let request =
                        from_vaapi_encode_request(*request).map_err(SubmitError::Rejected)?;
                    Err(SubmitError::Busy(request))
                }
                Err(VaapiWorkerSubmitError::Stopped(request)) => {
                    let request =
                        from_vaapi_encode_request(*request).map_err(SubmitError::Rejected)?;
                    Err(SubmitError::Stopped(request))
                }
            }
        }

        fn drain(&mut self) -> Vec<EncodeCompletion> {
            self.worker
                .drain()
                .map(|completion| EncodeCompletion {
                    token: completion.token,
                    result: completion.result,
                })
                .collect()
        }

        fn retire(&mut self, stream: MediaStreamId, generation: StreamGeneration) -> Result<()> {
            ensure!(
                self.worker.try_retire(stream, generation),
                "VA-API encoder worker stopped before retirement"
            );
            Ok(())
        }
    }

    fn from_vaapi_encode_request(request: VaapiEncodeRequest) -> Result<EncodeRequest> {
        let input = match request.input {
            VaapiEncodeInput::Dmabuf(dmabuf) => EncodeInput::Dmabuf(from_vaapi_dmabuf(dmabuf)?),
            VaapiEncodeInput::PackedBgra {
                width,
                height,
                pixels,
            } => EncodeInput::PackedBgra {
                width,
                height,
                pixels,
            },
        };
        Ok(EncodeRequest {
            token: request.token,
            frame: request.frame,
            timestamp_micros: request.timestamp_micros,
            input,
        })
    }

    fn encoder_settings(codec: VideoCodec) -> Result<VaapiEncoderSettings> {
        let bitrate = match codec {
            VideoCodec::H264 => H264_BITRATE,
            VideoCodec::Av1 => AV1_BITRATE,
            VideoCodec::Vp9 => anyhow::bail!("VP9 encoded hoisting is not implemented"),
        };
        VaapiEncoderSettings::try_new(
            codec,
            bitrate,
            DEFAULT_FRAMES_PER_SECOND,
            DEFAULT_KEYFRAME_INTERVAL,
        )
    }

    struct VaapiDecoder {
        worker: VaapiDecodeWorker,
        codec: VideoCodec,
        xrgb_modifiers: Vec<u64>,
    }

    impl DecodeBackend for VaapiDecoder {
        fn try_submit(&mut self, request: DecodeRequest) -> Result<(), SubmitError<DecodeRequest>> {
            if request.access_unit.codec != self.codec {
                return Err(SubmitError::Rejected(anyhow::anyhow!(
                    "received {:?} in a negotiated {:?} stream",
                    request.access_unit.codec,
                    self.codec,
                )));
            }
            let vaapi = VaapiDecodeRequest {
                token: request.token,
                access_unit: request.access_unit,
                visible_width: request.visible_width,
                visible_height: request.visible_height,
                xrgb_modifiers: self.xrgb_modifiers.clone(),
            };
            self.worker.try_decode(vaapi).map_err(|error| match error {
                VaapiWorkerSubmitError::Busy(request) => SubmitError::Busy(DecodeRequest {
                    token: request.token,
                    access_unit: request.access_unit,
                    visible_width: request.visible_width,
                    visible_height: request.visible_height,
                }),
                VaapiWorkerSubmitError::Stopped(request) => SubmitError::Stopped(DecodeRequest {
                    token: request.token,
                    access_unit: request.access_unit,
                    visible_width: request.visible_width,
                    visible_height: request.visible_height,
                }),
            })
        }

        fn drain(&mut self) -> Vec<DecodeCompletion> {
            self.worker
                .drain()
                .map(|completion| DecodeCompletion {
                    token: completion.token,
                    result: completion.result.and_then(|frames| {
                        frames
                            .into_iter()
                            .map(|frame| {
                                Ok(DecodedFrame {
                                    frame: frame.frame,
                                    dmabuf: from_vaapi_dmabuf(frame.dmabuf)?,
                                })
                            })
                            .collect()
                    }),
                })
                .collect()
        }

        fn retire(&mut self, stream: MediaStreamId, generation: StreamGeneration) -> Result<()> {
            ensure!(
                self.worker.try_retire(stream, generation),
                "VA-API decoder worker stopped before retirement"
            );
            Ok(())
        }
    }

    fn to_vaapi_dmabuf(dmabuf: ExternalDmabuf) -> Result<VaapiDmabuf> {
        let height = dmabuf.extent.height;
        let modifier = dmabuf.modifier;
        let mut objects = Vec::with_capacity(dmabuf.planes.len());
        let mut planes = Vec::with_capacity(dmabuf.planes.len());
        for (index, plane) in dmabuf.planes.into_iter().enumerate() {
            let stat = rustix::fs::fstat(&plane.file_descriptor)?;
            let fallback = u64::from(plane.offset)
                .checked_add(u64::from(plane.stride).saturating_mul(u64::from(height)))
                .context("DMA-BUF fallback size overflow")?;
            let stat_size = u64::try_from(stat.st_size).unwrap_or_default();
            let object_size = if stat_size == 0 { fallback } else { stat_size };
            let size =
                u32::try_from(object_size).context("DMA-BUF object exceeds VA-API size range")?;
            objects.push(VaapiDmabufObject {
                file_descriptor: plane.file_descriptor,
                size,
                modifier,
            });
            planes.push(VaapiDmabufPlane {
                object_index: u8::try_from(index).context("DMA-BUF has too many objects")?,
                offset: plane.offset,
                stride: plane.stride,
            });
        }
        VaapiDmabuf::try_new(
            dmabuf.extent.width,
            dmabuf.extent.height,
            dmabuf.format,
            objects,
            planes,
        )
    }

    fn from_vaapi_dmabuf(dmabuf: VaapiDmabuf) -> Result<ExternalDmabuf> {
        let modifier = dmabuf.primary_modifier()?;
        let planes = dmabuf
            .planes
            .iter()
            .map(|plane| {
                let object = dmabuf
                    .objects
                    .get(usize::from(plane.object_index))
                    .context("VA-API plane references an absent object")?;
                Ok(ExternalDmabufPlane {
                    file_descriptor: object.file_descriptor.try_clone()?,
                    offset: plane.offset,
                    stride: plane.stride,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(ExternalDmabuf {
            extent: weld_client::Extent::new(dmabuf.width, dmabuf.height),
            format: dmabuf.fourcc,
            modifier,
            flags: 0,
            planes,
        })
    }
}

#[cfg(feature = "vaapi")]
pub use vaapi::{decode_backend, encode_backend};
