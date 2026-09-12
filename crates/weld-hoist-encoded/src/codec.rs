//! Codec-independent hoist adapter contracts and optional VA-API binding.

use std::any::Any;

use anyhow::Result;
use weld_client::{ClientBufferId, ClientBufferLease, ClientBufferUseId, PresentationRate};
#[cfg(feature = "native")]
use weld_core::dmabuf::ExternalDmabuf;
use weld_media::{DecodeTiming, EncodedAccessUnit, MediaFrameId, MediaStreamId, StreamGeneration};

use crate::EncoderBitrateLimits;

/// Prepared encoder input. Native integrations may add representations.
#[non_exhaustive]
pub enum EncodeInput {
    #[cfg(feature = "native")]
    Dmabuf(ExternalDmabuf),
    PackedBgra {
        width: u32,
        height: u32,
        pixels: Vec<u8>,
    },
}

/// Backend-prepared input and any source consumer needed through completion.
/// Copied pixels need no retained lease; borrowed native storage does.
pub struct PreparedEncodeInput {
    pub input: EncodeInput,
    pub retained_lease: Option<ClientBufferLease>,
}

pub struct EncodeRequest {
    pub token: u64,
    pub frame: MediaFrameId,
    pub timestamp_micros: u64,
    pub input: EncodeInput,
    /// Frozen settings for this generation; None uses the backend's default.
    pub bitrate_bits_per_second: Option<u64>,
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

pub struct DecodedFrame<O> {
    pub frame: MediaFrameId,
    pub buffer: O,
}

pub struct DecodeCompletion<O> {
    pub token: u64,
    pub result: Result<Vec<DecodedFrame<O>>>,
    pub timing: Option<DecodeTiming>,
}

pub enum SubmitError<T> {
    Busy(T),
    Stopped(T),
    Rejected(anyhow::Error),
}

pub trait EncodeBackend {
    /// Optional configured operating ceiling, not necessarily a probed device
    /// maximum. Presenter preferences are independently enforced by the port.
    fn frame_rate_limit(&self) -> Option<PresentationRate> {
        None
    }
    /// Resolve this adapter's source access without exposing native types to
    /// scheduling. Retain a lease whenever asynchronous work borrows its storage.
    fn prepare_input(&self, lease: &ClientBufferLease) -> Result<PreparedEncodeInput>;

    /// Optional rate control through generation replacement, not hot retuning.
    fn bitrate_limits(&self) -> Option<EncoderBitrateLimits> {
        None
    }
    fn try_submit(&mut self, request: EncodeRequest) -> Result<(), SubmitError<EncodeRequest>>;
    fn drain(&mut self) -> Vec<EncodeCompletion>;
    fn retire(&mut self, stream: MediaStreamId, generation: StreamGeneration) -> Result<()>;
}

pub trait DecodeBackend {
    /// Owned output that stays valid until published or dropped, independently
    /// of subsequent submissions, generation retirement and backend destruction.
    /// Unpublished outputs may be dropped after this backend; releasing them must
    /// not require a live backend. Retain any necessary native owners in the output.
    type Output: 'static;

    /// Nonblocking, bounded admission. Busy returns the exact owned request and
    /// must arrange a host wake when capacity becomes available. Callers submit
    /// in stream order; independent streams may complete in any order.
    fn try_submit(&mut self, request: DecodeRequest) -> Result<(), SubmitError<DecodeRequest>>;
    /// Return all completed work alongside any terminal failure. Each successful
    /// output owns stable storage independent of the codec context, so retiring
    /// that context cannot invalidate or overwrite a returned image.
    fn drain(&mut self) -> (Vec<DecodeCompletion<Self::Output>>, Option<anyhow::Error>);
    /// Idempotent retirement; completion releases context capacity and wakes the
    /// host even if there are no subsequent frames. Never retire an active job.
    fn retire(&mut self, stream: MediaStreamId, generation: StreamGeneration) -> Result<()>;
}

/// Converts a completed native output into a client lease at atomic publication,
/// not at decode completion. Dropping an unpublished [`Self::Buffer`] must release it.
/// This is distinct from the app-side client importer that consumes the lease.
pub trait DecodedFramePublisher: 'static {
    /// Backend-owned decoded allocation, retained until publication or cancellation.
    type Buffer: 'static;
    /// App-side importer matching the access payload placed in published leases.
    type ClientImporter: Any;
    /// Supply the app-side intake matching this publisher's lease payloads.
    fn client_importer(&self) -> Self::ClientImporter;
    /// IDs belong to the destination adapter. Returned leases and release
    /// closures own all required state and must outlive this publisher/port.
    /// Failure may consume IDs and is terminal through the destination relay.
    /// On failure, release any native resources allocated by this call.
    fn publish(
        &mut self,
        buffer: Self::Buffer,
        id: ClientBufferId,
        use_id: ClientBufferUseId,
    ) -> Result<ClientBufferLease>;
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

    use weld_client::{ClientBufferLease, PresentationRate};

    use super::{
        DecodeBackend, DecodeCompletion, DecodeRequest, DecodedFrame, EncodeBackend,
        EncodeCompletion, EncodeInput, EncodeRequest, SubmitError,
    };
    use crate::{EncoderBitrateLimits, PreparedEncodeInput, native::prepare_input};

    const H264_BITRATE: u64 = 16_000_000;
    const AV1_BITRATE: u64 = 8_000_000;
    /// Provisional control floor, not a probed device limit or quality guarantee.
    /// Avoid tiny area-weighted targets and correspondingly tiny CBR reservoirs.
    const MINIMUM_CONTROL_BITRATE: u64 = 128_000;
    const CONFIGURED_FRAME_RATE: PresentationRate = PresentationRate::HZ_60;
    const DEFAULT_FRAMES_PER_SECOND: u32 = CONFIGURED_FRAME_RATE.millihertz() / 1000;
    const DEFAULT_KEYFRAME_INTERVAL: u32 = 32;
    const DRM_FORMAT_XRGB8888: u32 = u32::from_le_bytes(*b"XR24");

    pub fn encode_backend(
        render_node: PathBuf,
        codec: VideoCodec,
        dump_directory: Option<PathBuf>,
        notify: impl Fn() + Send + Sync + 'static,
    ) -> Result<Box<dyn EncodeBackend>> {
        let settings = encoder_settings(codec)?;
        // This control surface permits lowering and restoring the validated
        // startup rate, not discovering a device's maximum operating rate.
        let limits = encoder_bitrate_limits(settings)?;
        Ok(Box::new(VaapiEncoder {
            worker: VaapiEncodeWorker::spawn(render_node, dump_directory, notify)?,
            settings,
            limits,
        }))
    }

    pub fn decode_backend(
        capabilities: &ExternalDmabufCapabilities,
        codec: VideoCodec,
        notify: impl Fn() + Send + Sync + 'static,
    ) -> Result<Box<dyn DecodeBackend<Output = ExternalDmabuf>>> {
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
        limits: EncoderBitrateLimits,
    }

    impl EncodeBackend for VaapiEncoder {
        fn frame_rate_limit(&self) -> Option<PresentationRate> {
            // Matches DEFAULT_FRAMES_PER_SECOND and the FFmpeg CBR configuration.
            Some(CONFIGURED_FRAME_RATE)
        }
        fn prepare_input(&self, lease: &ClientBufferLease) -> Result<PreparedEncodeInput> {
            prepare_input(lease)
        }

        fn bitrate_limits(&self) -> Option<EncoderBitrateLimits> {
            Some(self.limits)
        }

        fn try_submit(&mut self, request: EncodeRequest) -> Result<(), SubmitError<EncodeRequest>> {
            let EncodeRequest {
                token,
                frame,
                timestamp_micros,
                input,
                bitrate_bits_per_second,
            } = request;
            let settings = match bitrate_bits_per_second {
                Some(bitrate) => {
                    self.limits
                        .validate(bitrate)
                        .map_err(SubmitError::Rejected)?;
                    self.settings
                        .with_bitrate(bitrate)
                        .map_err(SubmitError::Rejected)?
                }
                None => self.settings,
            };
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
                settings,
                input,
            };
            match self.worker.try_encode(request) {
                Ok(()) => Ok(()),
                Err(VaapiWorkerSubmitError::Busy(request)) => {
                    let request = from_vaapi_encode_request(*request, bitrate_bits_per_second)
                        .map_err(SubmitError::Rejected)?;
                    Err(SubmitError::Busy(request))
                }
                Err(VaapiWorkerSubmitError::Stopped(request)) => {
                    let request = from_vaapi_encode_request(*request, bitrate_bits_per_second)
                        .map_err(SubmitError::Rejected)?;
                    Err(SubmitError::Stopped(request))
                }
                Err(VaapiWorkerSubmitError::Rejected(error)) => Err(SubmitError::Rejected(error)),
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

    fn from_vaapi_encode_request(
        request: VaapiEncodeRequest,
        bitrate_bits_per_second: Option<u64>,
    ) -> Result<EncodeRequest> {
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
            bitrate_bits_per_second,
        })
    }

    fn encoder_bitrate_limits(settings: VaapiEncoderSettings) -> Result<EncoderBitrateLimits> {
        EncoderBitrateLimits::try_new(
            MINIMUM_CONTROL_BITRATE,
            settings.bitrate_bits(),
            settings.bitrate_bits(),
        )
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
        type Output = ExternalDmabuf;

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
                VaapiWorkerSubmitError::Rejected(error) => SubmitError::Rejected(error),
            })
        }

        fn drain(&mut self) -> (Vec<DecodeCompletion<ExternalDmabuf>>, Option<anyhow::Error>) {
            let (completions, failure) = self.worker.drain();
            let completions = completions
                .into_iter()
                .map(|completion| DecodeCompletion {
                    token: completion.token,
                    timing: completion.timing,
                    result: completion.result.and_then(|frames| {
                        frames
                            .into_iter()
                            .map(|frame| {
                                Ok(DecodedFrame {
                                    frame: frame.frame,
                                    buffer: from_vaapi_dmabuf(frame.dmabuf)?,
                                })
                            })
                            .collect()
                    }),
                })
                .collect();
            (completions, failure)
        }

        fn retire(&mut self, stream: MediaStreamId, generation: StreamGeneration) -> Result<()> {
            self.worker.retire(stream, generation)
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
    #[cfg(test)]
    mod tests {
        use super::*;
        use weld_media::MediaFrameId;

        #[test]
        fn adapter_control_floor_does_not_restrict_general_encoder_settings() {
            for (codec, startup) in [(VideoCodec::Av1, 8_000_000), (VideoCodec::H264, 16_000_000)] {
                let settings = encoder_settings(codec).expect("settings");
                let limits = encoder_bitrate_limits(settings).expect("limits");
                assert_eq!(
                    (limits.minimum(), limits.initial(), limits.maximum()),
                    (128_000, startup, startup)
                );
                assert!(settings.with_bitrate(64_000).is_ok());
            }
        }

        #[test]
        fn rejected_worker_request_conversion_preserves_the_original_override() {
            for override_rate in [None, Some(4_000_000)] {
                let settings = encoder_settings(VideoCodec::Av1).expect("settings");
                let request = VaapiEncodeRequest {
                    token: 3,
                    frame: MediaFrameId::new(MediaStreamId::new(1), StreamGeneration::new(2), 0),
                    timestamp_micros: 4,
                    settings: settings
                        .with_bitrate(override_rate.unwrap_or(settings.bitrate_bits()))
                        .expect("rate"),
                    input: VaapiEncodeInput::PackedBgra {
                        width: 1,
                        height: 1,
                        pixels: vec![1, 2, 3, 4],
                    },
                };
                let converted = from_vaapi_encode_request(request, override_rate)
                    .expect("convert without hardware");
                assert_eq!(converted.bitrate_bits_per_second, override_rate);
                assert_eq!(converted.token, 3);
                assert_eq!(converted.timestamp_micros, 4);
                assert_eq!(converted.frame.generation.raw(), 2);
                assert!(
                    matches!(converted.input, EncodeInput::PackedBgra { width: 1, height: 1, pixels } if pixels == [1, 2, 3, 4])
                );
            }
        }
    }
}

#[cfg(feature = "vaapi")]
pub use vaapi::{decode_backend, encode_backend};
