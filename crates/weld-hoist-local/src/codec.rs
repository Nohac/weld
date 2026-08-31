//! Codec-independent local adapter contracts and optional VA-API binding.

use anyhow::Result;
use weld_core::dmabuf::ExternalDmabuf;
use weld_media::{EncodedAccessUnit, MediaFrameId, MediaStreamId, StreamGeneration};

pub enum LocalEncodeInput {
    Dmabuf(ExternalDmabuf),
    PackedBgra {
        width: u32,
        height: u32,
        pixels: Vec<u8>,
    },
}

pub struct LocalEncodeRequest {
    pub token: u64,
    pub frame: MediaFrameId,
    pub timestamp_micros: u64,
    pub input: LocalEncodeInput,
}

pub struct LocalEncodeCompletion {
    pub token: u64,
    pub result: Result<EncodedAccessUnit>,
}

pub struct LocalDecodeRequest {
    pub token: u64,
    pub access_unit: EncodedAccessUnit,
    pub visible_width: u32,
    pub visible_height: u32,
}

pub struct LocalDecodedFrame {
    pub frame: MediaFrameId,
    pub dmabuf: ExternalDmabuf,
}

pub struct LocalDecodeCompletion {
    pub token: u64,
    pub result: Result<Vec<LocalDecodedFrame>>,
}

pub enum LocalSubmitError<T> {
    Busy(T),
    Stopped(T),
    Rejected(anyhow::Error),
}

pub trait LocalEncodeBackend {
    fn try_submit(
        &mut self,
        request: LocalEncodeRequest,
    ) -> Result<(), LocalSubmitError<LocalEncodeRequest>>;
    fn drain(&mut self) -> Vec<LocalEncodeCompletion>;
    fn retire(&mut self, stream: MediaStreamId, generation: StreamGeneration) -> Result<()>;
}

pub trait LocalDecodeBackend {
    fn try_submit(
        &mut self,
        request: LocalDecodeRequest,
    ) -> Result<(), LocalSubmitError<LocalDecodeRequest>>;
    fn drain(&mut self) -> Vec<LocalDecodeCompletion>;
    fn retire(&mut self, stream: MediaStreamId, generation: StreamGeneration) -> Result<()>;
}

#[cfg(feature = "encoded-vaapi")]
mod vaapi {
    use std::{num::NonZeroU16, path::PathBuf};

    use anyhow::{Context, Result, ensure};
    use weld_core::dmabuf::{ExternalDmabuf, ExternalDmabufCapabilities, ExternalDmabufPlane};
    use weld_media::{MediaStreamId, StreamGeneration};
    use weld_media_vaapi::{
        VaapiDecodeRequest, VaapiDecodeWorker, VaapiDmabuf, VaapiDmabufObject, VaapiDmabufPlane,
        VaapiEncodeInput, VaapiEncodeRequest, VaapiEncodeWorker, VaapiWorkerSubmitError,
    };

    use super::{
        LocalDecodeBackend, LocalDecodeCompletion, LocalDecodeRequest, LocalDecodedFrame,
        LocalEncodeBackend, LocalEncodeCompletion, LocalEncodeInput, LocalEncodeRequest,
        LocalSubmitError,
    };

    const DEFAULT_BITRATE: u64 = 16_000_000;
    const DEFAULT_FRAMES_PER_SECOND: u32 = 60;
    const DEFAULT_INTRA_PERIOD: u16 = 32;
    const DRM_FORMAT_XRGB8888: u32 = u32::from_le_bytes(*b"XR24");

    pub(crate) fn encode_backend(
        render_node: PathBuf,
        notify: impl Fn() + Send + Sync + 'static,
    ) -> Result<Box<dyn LocalEncodeBackend>> {
        let intra_period =
            NonZeroU16::new(DEFAULT_INTRA_PERIOD).context("H.264 intra period must be non-zero")?;
        Ok(Box::new(VaapiLocalEncoder {
            worker: VaapiEncodeWorker::spawn(render_node, notify)?,
            intra_period,
        }))
    }

    pub(crate) fn decode_backend(
        capabilities: &ExternalDmabufCapabilities,
        notify: impl Fn() + Send + Sync + 'static,
    ) -> Result<Box<dyn LocalDecodeBackend>> {
        let xrgb_modifiers = capabilities
            .formats
            .iter()
            .filter_map(|format| (format.fourcc == DRM_FORMAT_XRGB8888).then_some(format.modifier))
            .collect::<Vec<_>>();
        ensure!(
            !xrgb_modifiers.is_empty(),
            "Weld exposes no XRGB DMA-BUF modifier for decoded video"
        );
        Ok(Box::new(VaapiLocalDecoder {
            worker: VaapiDecodeWorker::spawn(capabilities.render_node.clone(), notify)?,
            xrgb_modifiers,
        }))
    }

    struct VaapiLocalEncoder {
        worker: VaapiEncodeWorker,
        intra_period: NonZeroU16,
    }

    impl LocalEncodeBackend for VaapiLocalEncoder {
        fn try_submit(
            &mut self,
            request: LocalEncodeRequest,
        ) -> Result<(), LocalSubmitError<LocalEncodeRequest>> {
            let LocalEncodeRequest {
                token,
                frame,
                timestamp_micros,
                input,
            } = request;
            let input = match input {
                LocalEncodeInput::Dmabuf(dmabuf) => VaapiEncodeInput::Dmabuf(
                    to_vaapi_dmabuf(dmabuf).map_err(LocalSubmitError::Rejected)?,
                ),
                LocalEncodeInput::PackedBgra {
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
                bitrate: DEFAULT_BITRATE,
                frames_per_second: DEFAULT_FRAMES_PER_SECOND,
                intra_period: self.intra_period,
                input,
            };
            match self.worker.try_encode(request) {
                Ok(()) => Ok(()),
                Err(VaapiWorkerSubmitError::Busy(request)) => {
                    let request =
                        from_vaapi_encode_request(request).map_err(LocalSubmitError::Rejected)?;
                    Err(LocalSubmitError::Busy(request))
                }
                Err(VaapiWorkerSubmitError::Stopped(request)) => {
                    let request =
                        from_vaapi_encode_request(request).map_err(LocalSubmitError::Rejected)?;
                    Err(LocalSubmitError::Stopped(request))
                }
            }
        }

        fn drain(&mut self) -> Vec<LocalEncodeCompletion> {
            self.worker
                .drain()
                .map(|completion| LocalEncodeCompletion {
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

    fn from_vaapi_encode_request(request: VaapiEncodeRequest) -> Result<LocalEncodeRequest> {
        let input = match request.input {
            VaapiEncodeInput::Dmabuf(dmabuf) => {
                LocalEncodeInput::Dmabuf(from_vaapi_dmabuf(dmabuf)?)
            }
            VaapiEncodeInput::PackedBgra {
                width,
                height,
                pixels,
            } => LocalEncodeInput::PackedBgra {
                width,
                height,
                pixels,
            },
        };
        Ok(LocalEncodeRequest {
            token: request.token,
            frame: request.frame,
            timestamp_micros: request.timestamp_micros,
            input,
        })
    }

    struct VaapiLocalDecoder {
        worker: VaapiDecodeWorker,
        xrgb_modifiers: Vec<u64>,
    }

    impl LocalDecodeBackend for VaapiLocalDecoder {
        fn try_submit(
            &mut self,
            request: LocalDecodeRequest,
        ) -> Result<(), LocalSubmitError<LocalDecodeRequest>> {
            let vaapi = VaapiDecodeRequest {
                token: request.token,
                access_unit: request.access_unit,
                visible_width: request.visible_width,
                visible_height: request.visible_height,
                xrgb_modifiers: self.xrgb_modifiers.clone(),
            };
            self.worker.try_decode(vaapi).map_err(|error| match error {
                VaapiWorkerSubmitError::Busy(request) => {
                    LocalSubmitError::Busy(LocalDecodeRequest {
                        token: request.token,
                        access_unit: request.access_unit,
                        visible_width: request.visible_width,
                        visible_height: request.visible_height,
                    })
                }
                VaapiWorkerSubmitError::Stopped(request) => {
                    LocalSubmitError::Stopped(LocalDecodeRequest {
                        token: request.token,
                        access_unit: request.access_unit,
                        visible_width: request.visible_width,
                        visible_height: request.visible_height,
                    })
                }
            })
        }

        fn drain(&mut self) -> Vec<LocalDecodeCompletion> {
            self.worker
                .drain()
                .map(|completion| LocalDecodeCompletion {
                    token: completion.token,
                    result: completion.result.and_then(|frames| {
                        frames
                            .into_iter()
                            .map(|frame| {
                                Ok(LocalDecodedFrame {
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

#[cfg(feature = "encoded-vaapi")]
pub(crate) use vaapi::{decode_backend, encode_backend};
