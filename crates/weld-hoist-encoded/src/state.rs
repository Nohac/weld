//! Encoded hoist state machines, independent from transport and hardware backend.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    fs::{self, File, OpenOptions},
    io::{BufWriter, Write},
    path::{Path, PathBuf},
    time::Instant,
};

use anyhow::{Context, Result, bail, ensure};
use weld_client::{
    ClientBufferId, ClientBufferLease, ClientBufferMetadata, ClientBufferUseId,
    ClientCommitRevision, ClientRequest, ClientSourceDescriptor, ClientSurfaceEvent,
    ClientSurfaceEventKind, ClientSurfaceId, ClientSurfaceRequestKind, SurfaceAlphaMode,
    SurfaceBufferChange, SurfaceLayerId, WireClientSurfaceEvent, WireClientSurfaceEventKind,
    WireSurfaceBufferChange,
};
use weld_core::dmabuf::{DirectClientBufferAccess, DmabufContext, export_client_dmabuf};
use weld_hoist_core::{
    DestinationPortCommand, DestinationPortEvent, DestinationPortRecord, HoistDestinationPort,
    HoistPortError, HoistPortResult, HoistSessionId, HoistSourcePort, SourcePortCommand,
};
use weld_hoist_protocol::{
    DestinationEnvelope, DestinationMessage, EncodedBuffer, MAX_ENCODED_ACCESS_UNIT_BYTES,
    MediaEnvelope, SourceEnvelope, SourceMessage,
};
use weld_media::{
    EncodedAccessUnit, EncodedFrameKind, MediaFrameId, MediaStreamId, StreamGeneration, VideoCodec,
};

use crate::TransportSnapshot;
use crate::bitrate::{BitrateRequest, EncoderRateApplication, EncoderRateControl, EncoderRates};
use crate::codec::{
    DecodeBackend, DecodeRequest, EncodeBackend, EncodeInput, EncodeRequest, SubmitError,
};
use crate::destination_observations::{
    DestinationGauges, DestinationObservation, DestinationObservations,
};
use crate::observations::{SourceGauges, SourceObservation, SourceObservations};
use crate::output::SourceOutput;

/// One packet sent from an encoded source to its destination.
pub enum SourceTransportPacket {
    Control(SourceEnvelope<EncodedBuffer>),
    Media(MediaEnvelope<EncodedAccessUnit>),
}

/// Nonblocking transport half used by the encoded source port.
pub trait EncodedSourceTransport {
    /// Busy returns the exact unsent record; it is not a connection failure.
    fn try_send(
        &self,
        packet: SourceTransportPacket,
    ) -> HoistPortResult<SendStatus<SourceTransportPacket>>;
    /// Local transport room for another batch, not remote presentation feedback.
    fn media_headroom(&self) -> bool;
    fn drain(&self) -> HoistPortResult<Vec<DestinationEnvelope>>;
    fn disconnect(&self);
    /// Return locally measured, owned send state at `now`, or None when unavailable.
    /// Callers must not interpret unavailable observations as zero pressure.
    fn observations(&self, _now: Instant) -> Option<TransportSnapshot> {
        None
    }
}

/// Ownership-preserving nonblocking admission into a transport queue.
pub enum SendStatus<T> {
    Sent,
    Busy(T),
}

/// Nonblocking transport half used by the encoded destination port.
pub trait EncodedDestinationTransport {
    fn send(&self, packet: DestinationEnvelope) -> HoistPortResult<()>;
    fn drain(&self, budget: ReceiveBudget) -> HoistPortResult<Vec<SourceTransportPacket>>;
    /// Rearm buffered work after the consumer advances and recomputes its room.
    /// A zero budget must not repeatedly wake the compositor.
    fn wake_if_readable(&self, budget: ReceiveBudget) -> HoistPortResult<()>;
    fn disconnect(&self);
}

/// Independent local admission limits; these are not credits sent to a peer.
#[derive(Clone, Copy, Debug)]
pub struct ReceiveBudget {
    pub control_records: usize,
    pub media_records: usize,
    pub media_bytes: usize,
}

impl ReceiveBudget {
    /// Drain the binding's already bounded inboxes, useful outside a codec port.
    pub const ALL: Self = Self {
        control_records: usize::MAX,
        media_records: usize::MAX,
        media_bytes: usize::MAX,
    };
}

pub(super) const MAX_PENDING_SOURCE_EVENTS: usize = 128;
const MAX_DESTINATION_EVENTS: usize = 128;
pub(super) const MAX_REPLACEMENTS_PER_COMMIT: usize = 16;
const MAX_PENDING_MEDIA_FRAMES: usize = 128;
// Must admit at least one whole MAX_REPLACEMENTS_PER_COMMIT reference reservation.
const MAX_REFERENCED_FRAMES: usize = 128;
const MAX_PENDING_MEDIA_BYTES: usize = 64 * 1024 * 1024;
// This intentionally mirrors the current VA worker budget. Exceeding it while
// diagnostics are enabled indicates that stream retirement has fallen behind,
// so failing the requested diagnostic session is preferable to leaking files.
const MAX_OPEN_ACCESS_UNIT_DUMPS: usize = 16;

type EncodedGeneration = (MediaStreamId, StreamGeneration);

struct SourceStream {
    stream: MediaStreamId,
    generation: StreamGeneration,
    visible_extent: (u32, u32),
    next_sequence: Option<u64>,
    frozen_rate: Option<BitrateRequest>,
}

struct PreparedEncode {
    request: EncodeRequest,
    retained_dmabuf_lease: Option<ClientBufferLease>,
    rate: Option<BitrateRequest>,
}

struct ActiveEncode {
    token: u64,
    frame: MediaFrameId,
    retained_dmabuf_lease: Option<ClientBufferLease>,
    rate: Option<BitrateRequest>,
}

struct InFlightEncodeBatch {
    session: HoistSessionId,
    surface: ClientSurfaceId,
    event: WireClientSurfaceEvent<EncodedBuffer>,
    active: ActiveEncode,
    pending: VecDeque<PreparedEncode>,
    completed: Vec<weld_media::EncodedAccessUnit>,
    cancelled: bool,
    started_at: Instant,
}

struct AccessUnitDump {
    directory: PathBuf,
    codec: VideoCodec,
    streams: HashMap<(MediaStreamId, StreamGeneration), BufWriter<File>>,
}

impl AccessUnitDump {
    fn new(directory: PathBuf, codec: VideoCodec) -> Result<Self> {
        fs::create_dir_all(&directory).with_context(|| {
            format!(
                "could not create encoded dump directory {}",
                directory.display()
            )
        })?;
        Ok(Self {
            directory,
            codec,
            streams: HashMap::new(),
        })
    }

    fn write(&mut self, access_unit: &weld_media::EncodedAccessUnit) -> Result<()> {
        let key = (access_unit.frame.stream, access_unit.frame.generation);
        ensure!(
            access_unit.codec == self.codec,
            "encoded dump codec changed within one local session"
        );
        if !self.streams.contains_key(&key) {
            ensure!(
                self.streams.len() < MAX_OPEN_ACCESS_UNIT_DUMPS,
                "active encoded dump stream bound exceeded"
            );
            let path = dump_path(&self.directory, key, self.codec);
            let file = OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .open(&path)
                .with_context(|| format!("could not create encoded dump {}", path.display()))?;
            tracing::info!(path = %path.display(), codec = ?self.codec, "recording source encoded stream generation");
            self.streams.insert(key, BufWriter::new(file));
        }
        self.streams
            .get_mut(&key)
            .context("encoded dump stream disappeared")?
            .write_all(&access_unit.payload)
            .context("could not write encoded access unit")
    }

    fn flush(&mut self) -> Result<()> {
        for writer in self.streams.values_mut() {
            writer.flush().context("could not flush encoded dump")?;
        }
        Ok(())
    }

    fn retire(&mut self, stream: MediaStreamId, generation: StreamGeneration) -> Result<()> {
        if let Some(mut writer) = self.streams.remove(&(stream, generation)) {
            writer
                .flush()
                .context("could not flush retired encoded dump")?;
        }
        Ok(())
    }
}

fn dump_path(
    directory: &Path,
    (stream, generation): (MediaStreamId, StreamGeneration),
    codec: VideoCodec,
) -> PathBuf {
    let extension = match codec {
        VideoCodec::H264 => "h264",
        VideoCodec::Av1 => "obu",
        VideoCodec::Vp9 => "vp9",
    };
    directory.join(format!(
        "stream-{}-generation-{}.{}",
        stream.raw(),
        generation.raw(),
        extension,
    ))
}

struct EncodedSourceState {
    backend: Box<dyn EncodeBackend>,
    output: VecDeque<SourceTransportPacket>,
    transport_blocked: bool,
    retained_output_records: usize,
    pending: HashMap<ClientSurfaceId, VecDeque<(HoistSessionId, ClientSurfaceEvent)>>,
    pending_order: VecDeque<ClientSurfaceId>,
    resizing: HashSet<ClientSurfaceId>,
    streams: HashMap<(ClientSurfaceId, SurfaceLayerId), SourceStream>,
    in_flight: Option<InFlightEncodeBatch>,
    next_stream: Option<u64>,
    next_token: Option<u64>,
    started_at: Instant,
    last_timestamp_micros: u64,
    dump: Option<AccessUnitDump>,
    observations: SourceObservations,
    rates: Option<EncoderRates>,
}

impl EncodedSourceState {
    fn new(backend: Box<dyn EncodeBackend>) -> Self {
        let started_at = Instant::now();
        let rates = backend.bitrate_limits().map(EncoderRates::new);
        Self {
            backend,
            output: VecDeque::new(),
            transport_blocked: false,
            retained_output_records: 0,
            pending: HashMap::new(),
            pending_order: VecDeque::new(),
            resizing: HashSet::new(),
            streams: HashMap::new(),
            in_flight: None,
            next_stream: Some(1),
            next_token: Some(1),
            started_at,
            last_timestamp_micros: 0,
            dump: None,
            observations: SourceObservations::new(started_at),
            rates,
        }
    }

    fn with_access_unit_dump_directory(
        mut self,
        directory: PathBuf,
        codec: VideoCodec,
    ) -> Result<Self> {
        self.dump = Some(AccessUnitDump::new(directory, codec)?);
        Ok(self)
    }

    fn enqueue(&mut self, session: HoistSessionId, mut event: ClientSurfaceEvent) -> Result<()> {
        if let ClientSurfaceEventKind::Commit(commit) = &mut event.kind {
            self.observations.record(SourceObservation::CommitReceived);
            // The selected encoded path has no alpha, including on retained commits.
            commit.alpha_mode = SurfaceAlphaMode::Discarded;
        }
        let surface = event.surface;
        let surface_busy = self.pending.contains_key(&surface)
            || self
                .in_flight
                .as_ref()
                .is_some_and(|in_flight| in_flight.surface == surface);
        match &event.kind {
            ClientSurfaceEventKind::Commit(_) => {
                if self.resizing.contains(&surface)
                    || !self.pending.is_empty()
                    || self.transport_blocked
                    || !self.output.is_empty()
                    || self.in_flight.is_some()
                {
                    self.queue_event(session, event)?;
                } else {
                    self.submit_or_send(session, event)?;
                }
            }
            ClientSurfaceEventKind::Destroyed => {
                // Destruction cancels unpublished work but preserves published media.
                self.cancel_surface(surface)?;
                self.send_without_buffer(session, event)?;
            }
            ClientSurfaceEventKind::Role(_) | ClientSurfaceEventKind::Interaction(_) => {
                if surface_busy {
                    self.queue_event(session, event)?;
                } else {
                    self.send_without_buffer(session, event)?;
                }
            }
        }
        Ok(())
    }

    fn set_resizing(&mut self, surface: ClientSurfaceId, resizing: bool) -> Result<()> {
        if resizing {
            self.resizing.insert(surface);
        } else {
            self.resizing.remove(&surface);
            self.schedule()?;
        }
        Ok(())
    }

    fn drain(&mut self) -> Result<()> {
        for completion in self.backend.drain() {
            let mut batch = self
                .in_flight
                .take()
                .context("encoder completed without an in-flight source event")?;
            ensure!(
                completion.token == batch.active.token,
                "encoder completed an unexpected source event"
            );
            drop(batch.active.retained_dmabuf_lease.take());
            let valid_packet = completion.result.as_ref().is_ok_and(|unit| {
                unit.frame == batch.active.frame
                    && (unit.frame.sequence != 0 || unit.kind == EncodedFrameKind::Keyframe)
            });
            if let (Some(rates), Some(request)) = (&self.rates, batch.active.rate) {
                rates.finished(
                    EncoderRateApplication {
                        request,
                        frame: batch.active.frame,
                    },
                    !batch.cancelled && valid_packet,
                );
            }
            if completion.result.is_err() {
                self.observations.record(SourceObservation::CodecFailed);
            }
            if batch.cancelled {
                self.observations.record(SourceObservation::BatchCancelled);
                if let Err(error) = completion.result {
                    tracing::debug!(frame = ?batch.active.frame, error = %format_args!("{error:#}"),
                        "discarded cancelled encode failure");
                }
                self.retire_generation(batch.active.frame.stream, batch.active.frame.generation)?;
                drop(batch.pending);
                self.schedule()?;
                continue;
            }
            let access_unit = completion.result?;
            ensure!(
                !access_unit.payload.is_empty()
                    && access_unit.payload.len() <= MAX_ENCODED_ACCESS_UNIT_BYTES,
                "encoder returned an invalid access-unit length"
            );
            ensure!(
                valid_packet,
                "encoder returned an unexpected frame or a non-keyframe at generation start"
            );
            batch.completed.push(access_unit);
            if let Some(next) = batch.pending.pop_front() {
                batch.active = self.submit_prepared(next)?;
                self.in_flight = Some(batch);
                continue;
            }
            let revision = commit_revision(&batch.event)
                .context("encoded batch control event was not a commit")?;
            let batch_wall_time = batch.started_at.elapsed();
            let payload_bytes = batch.completed.iter().fold(0_u64, |total, unit| {
                total.saturating_add(u64::try_from(unit.payload.len()).unwrap_or(u64::MAX))
            });
            self.observations.record(SourceObservation::BatchCompleted {
                layer_frames: batch.completed.len(),
                payload_bytes,
                wall_time: batch_wall_time,
            });
            tracing::trace!(
                surface = ?batch.surface,
                ?revision,
                frames = batch.completed.len(),
                payload_bytes,
                encode_batch_micros = batch_wall_time.as_micros(),
                pending_events = self.pending.values().map(VecDeque::len).sum::<usize>(),
                "completed encoded source batch"
            );
            if let Some(dump) = &mut self.dump {
                for access_unit in &batch.completed {
                    dump.write(access_unit)?;
                }
                dump.flush()?;
            }
            for access_unit in batch.completed {
                self.output
                    .push_back(SourceTransportPacket::Media(MediaEnvelope {
                        session: batch.session,
                        access_unit,
                    }));
            }
            self.output
                .push_back(SourceTransportPacket::Control(SourceEnvelope {
                    session: batch.session,
                    message: SourceMessage::Surface(batch.event),
                }));
            self.schedule()?;
        }
        Ok(())
    }

    fn report_observations(
        &mut self,
        final_report: bool,
        transport: impl FnOnce(Instant) -> Option<TransportSnapshot>,
    ) {
        let now = Instant::now();
        if !self.observations.report_due(now, final_report) {
            return;
        }
        let gauges = SourceGauges {
            pending_events: self.pending.values().map(VecDeque::len).sum(),
            retained_output_records: self.retained_output_records,
            transport_blocked: self.transport_blocked,
            active_streams: self.streams.len(),
            encode_in_flight: self.in_flight.is_some(),
            active_batch_age: self
                .in_flight
                .as_ref()
                .map(|batch| now.saturating_duration_since(batch.started_at))
                .unwrap_or_default(),
        };
        let report = self.observations.take_report(now, gauges, final_report);
        if let Some(rates) = &self.rates {
            rates.report(report.is_some() || final_report);
        }
        if let Some(report) = report {
            report.emit();
            if let Some(snapshot) = transport(now) {
                snapshot.emit();
            }
        }
    }

    fn cancel_surface(&mut self, surface: ClientSurfaceId) -> Result<()> {
        self.pending.remove(&surface);
        self.pending_order.retain(|candidate| *candidate != surface);
        self.resizing.remove(&surface);
        self.observations
            .record(SourceObservation::SurfaceCancelled);
        if let Some(in_flight) = self.in_flight.as_mut()
            && in_flight.surface == surface
        {
            in_flight.cancelled = true;
            in_flight.pending.clear();
            in_flight.completed.clear();
        }
        self.retire_surface_streams(surface)
    }

    fn queue_event(
        &mut self,
        session: HoistSessionId,
        mut event: ClientSurfaceEvent,
    ) -> Result<()> {
        let surface = event.surface;
        let pending_events = self.pending.values().map(VecDeque::len).sum::<usize>();
        let first_for_surface = !self.pending.contains_key(&surface);
        let queue = self.pending.entry(surface).or_default();
        let mut replaced_previous_commit = false;
        if let Some((_, previous)) = queue.back_mut()
            && let (
                ClientSurfaceEventKind::Commit(current),
                ClientSurfaceEventKind::Commit(previous),
            ) = (&mut event.kind, &mut previous.kind)
            && current.mapped == previous.mapped
        {
            current.carry_unobserved_content_from(previous);
            queue.pop_back();
            self.observations.record(SourceObservation::CommitCoalesced);
            replaced_previous_commit = true;
            tracing::trace!(?surface, "coalesced an unobserved encoded source commit");
        }
        if !replaced_previous_commit {
            ensure!(
                pending_events < MAX_PENDING_SOURCE_EVENTS,
                "encoded source pending-event bound exceeded"
            );
        }
        if first_for_surface {
            self.pending_order.push_back(surface);
        }
        queue.push_back((session, event));
        Ok(())
    }

    fn schedule(&mut self) -> Result<()> {
        if self.in_flight.is_some() || self.transport_blocked || !self.output.is_empty() {
            return Ok(());
        }
        let mut blocked_surfaces = 0;
        while blocked_surfaces < self.pending_order.len() {
            let Some(surface) = self.pending_order.pop_front() else {
                break;
            };
            let Some(queue) = self.pending.get_mut(&surface) else {
                continue;
            };
            let Some((session, event)) = queue.front() else {
                self.pending.remove(&surface);
                continue;
            };
            if self.resizing.contains(&surface)
                && matches!(event.kind, ClientSurfaceEventKind::Commit(_))
            {
                self.pending_order.push_back(surface);
                blocked_surfaces += 1;
                continue;
            }
            blocked_surfaces = 0;
            let session = *session;
            let event = queue
                .pop_front()
                .context("encoded source event queue disappeared")?;
            let event = event.1;
            let queue_empty = queue.is_empty();
            if queue_empty {
                self.pending.remove(&surface);
            } else {
                self.pending_order.push_back(surface);
            }
            self.submit_or_send(session, event)?;
            if self.in_flight.is_some() {
                return Ok(());
            }
        }
        Ok(())
    }

    fn submit_or_send(&mut self, session: HoistSessionId, event: ClientSurfaceEvent) -> Result<()> {
        // Reconcile only when this snapshot is scheduled, not while a queued
        // snapshot may still be coalesced or an older encode is using its layers.
        self.reconcile_streams(&event)?;
        let replaced = replaced_buffer_count(&event);
        match replaced {
            0 => self.send_without_buffer(session, event),
            count if count <= MAX_REPLACEMENTS_PER_COMMIT => self.submit_batch(session, event),
            count => bail!(
                "encoded commit replaces {count} buffers, exceeding the {MAX_REPLACEMENTS_PER_COMMIT}-layer batch limit"
            ),
        }
    }

    fn send_without_buffer(
        &mut self,
        session: HoistSessionId,
        event: ClientSurfaceEvent,
    ) -> Result<()> {
        let event = WireClientSurfaceEvent::try_from_client(event, |_| {
            Err::<EncodedBuffer, _>(anyhow::anyhow!(
                "buffer replacement entered the structural encoded path"
            ))
        })?;
        self.output
            .push_back(SourceTransportPacket::Control(SourceEnvelope {
                session,
                message: SourceMessage::Surface(event),
            }));
        Ok(())
    }

    fn submit_batch(&mut self, session: HoistSessionId, event: ClientSurfaceEvent) -> Result<()> {
        let surface = event.surface;
        let mut prepared = VecDeque::new();
        let event = WireClientSurfaceEvent::try_from_client_with_layer(event, |layer, lease| {
            let (frame, rate) = self.allocate_frame(surface, layer, lease.metadata())?;
            let access = lease
                .access::<DirectClientBufferAccess>()
                .context("client-buffer lease does not contain direct native access")?;
            let (input, retained_dmabuf_lease) = match access {
                DirectClientBufferAccess::Dmabuf(_) => {
                    let dmabuf = export_client_dmabuf(&lease)?;
                    ensure!(
                        !dmabuf.is_y_inverted(),
                        "encoded tracer does not support y-inverted DMA-BUF input"
                    );
                    (EncodeInput::Dmabuf(dmabuf), Some(lease.clone()))
                }
                DirectClientBufferAccess::Shm(shm) => (
                    EncodeInput::PackedBgra {
                        width: lease.metadata().extent.width,
                        height: lease.metadata().extent.height,
                        pixels: shm.bgra_pixels.clone(),
                    },
                    None,
                ),
            };
            let token = take_counter(&mut self.next_token, "encoded source token")?;
            let timestamp_micros = self.next_timestamp()?;
            prepared.push_back(PreparedEncode {
                request: EncodeRequest {
                    token,
                    frame,
                    timestamp_micros,
                    input,
                    bitrate_bits_per_second: rate.map(|value| value.bits_per_second),
                },
                retained_dmabuf_lease,
                rate,
            });
            Ok::<_, anyhow::Error>(EncodedBuffer { frame })
        })?;
        let first = prepared
            .pop_front()
            .context("encoded commit did not expose a replacement")?;
        let active = self.submit_prepared(first)?;
        tracing::trace!(
            ?surface,
            revision = ?commit_revision(&event),
            replacements = prepared.len() + 1,
            pending_events = self.pending.values().map(VecDeque::len).sum::<usize>(),
            "submitted encoded source batch"
        );
        self.in_flight = Some(InFlightEncodeBatch {
            session,
            surface,
            event,
            active,
            pending: prepared,
            completed: Vec::new(),
            cancelled: false,
            started_at: Instant::now(),
        });
        Ok(())
    }

    fn next_timestamp(&mut self) -> Result<u64> {
        let minimum_timestamp = self
            .last_timestamp_micros
            .checked_add(1)
            .context("encoded timestamp space is exhausted")?;
        let elapsed_timestamp = u64::try_from(self.started_at.elapsed().as_micros())
            .context("encoded timestamp exceeds u64")?;
        let timestamp_micros = elapsed_timestamp.max(minimum_timestamp);
        self.last_timestamp_micros = timestamp_micros;
        Ok(timestamp_micros)
    }

    fn submit_prepared(&mut self, prepared: PreparedEncode) -> Result<ActiveEncode> {
        let PreparedEncode {
            request,
            retained_dmabuf_lease,
            rate,
        } = prepared;
        let token = request.token;
        let frame = request.frame;
        let application = rate.map(|request| EncoderRateApplication { request, frame });
        if let (Some(rates), Some(application)) = (&self.rates, application) {
            rates.submitted(application, Instant::now());
        }
        let result = self.backend.try_submit(request);
        if result.is_err()
            && let (Some(rates), Some(application)) = (&self.rates, application)
        {
            rates.finished(application, false);
        }
        match result {
            Ok(()) => Ok(ActiveEncode {
                token,
                frame,
                retained_dmabuf_lease,
                rate,
            }),
            Err(SubmitError::Busy(_)) => {
                bail!("encoder queue was busy without an in-flight source frame")
            }
            Err(SubmitError::Stopped(_)) => bail!("encoder worker stopped"),
            Err(SubmitError::Rejected(error)) => Err(error),
        }
    }

    fn allocate_frame(
        &mut self,
        surface: ClientSurfaceId,
        layer: SurfaceLayerId,
        metadata: ClientBufferMetadata,
    ) -> Result<(MediaFrameId, Option<BitrateRequest>)> {
        let visible_extent = (metadata.extent.width, metadata.extent.height);
        ensure!(
            visible_extent.0 > 0 && visible_extent.1 > 0,
            "encoded extent is zero"
        );
        let key = (surface, layer);
        if !self.streams.contains_key(&key) {
            let stream = take_counter(&mut self.next_stream, "encoded stream")?;
            let stream = MediaStreamId::new(stream);
            let frozen_rate = self
                .rates
                .as_ref()
                .and_then(|rates| rates.register(stream, surface, layer));
            self.streams.insert(
                key,
                SourceStream {
                    stream,
                    generation: StreamGeneration::new(1),
                    visible_extent,
                    next_sequence: Some(0),
                    frozen_rate,
                },
            );
        }
        let (frame, rate, retired) = {
            let stream = self
                .streams
                .get_mut(&key)
                .context("encoded stream disappeared")?;
            let rate = self
                .rates
                .as_ref()
                .and_then(|rates| rates.select(stream.stream, Instant::now()))
                .or(stream.frozen_rate);
            // Scheduling admits a new batch only after every old PreparedEncode
            // has finished. Never rotate by rewriting a job in that old batch.
            let retired = if stream.visible_extent != visible_extent
                || rate.map(|value| value.bits_per_second)
                    != stream.frozen_rate.map(|value| value.bits_per_second)
            {
                let retired = (stream.stream, stream.generation);
                stream.generation = StreamGeneration::new(
                    stream
                        .generation
                        .raw()
                        .checked_add(1)
                        .context("encoded stream generation exhausted")?,
                );
                stream.visible_extent = visible_extent;
                stream.next_sequence = Some(0);
                Some(retired)
            } else {
                None
            };
            stream.frozen_rate = rate;
            let sequence = take_counter(&mut stream.next_sequence, "encoded frame sequence")?;
            (
                MediaFrameId::new(stream.stream, stream.generation, sequence),
                rate,
                retired,
            )
        };
        if let Some((stream, generation)) = retired {
            self.retire_generation(stream, generation)?;
        }
        Ok((frame, rate))
    }

    fn reconcile_streams(&mut self, event: &ClientSurfaceEvent) -> Result<()> {
        let ClientSurfaceEventKind::Commit(commit) = &event.kind else {
            return Ok(());
        };
        let retired = self
            .streams
            .extract_if(|(surface, layer), _| {
                *surface == event.surface
                    && !commit.buffers.iter().any(|buffer| {
                        buffer.layer == *layer
                            && !matches!(buffer.change, SurfaceBufferChange::Removed)
                    })
            })
            .map(|(_, stream)| stream)
            .collect::<Vec<_>>();
        for stream in retired {
            if let Some(rates) = &self.rates {
                rates.remove(stream.stream);
            }
            self.retire_generation(stream.stream, stream.generation)?;
        }
        Ok(())
    }

    fn retire_surface_streams(&mut self, surface: ClientSurfaceId) -> Result<()> {
        let streams = self
            .streams
            .extract_if(|(candidate, _), _| *candidate == surface)
            .map(|(_, stream)| stream)
            .collect::<Vec<_>>();
        for stream in streams {
            if let Some(rates) = &self.rates {
                rates.remove(stream.stream);
            }
            self.retire_generation(stream.stream, stream.generation)?;
        }
        Ok(())
    }

    fn retire_generation(
        &mut self,
        stream: MediaStreamId,
        generation: StreamGeneration,
    ) -> Result<()> {
        // Retirement must not overtake a submitted job that can still create
        // this codec generation. The matching cancelled completion retires it.
        if self.in_flight.as_ref().is_some_and(|batch| {
            batch.cancelled
                && (batch.active.frame.stream, batch.active.frame.generation)
                    == (stream, generation)
        }) {
            return Ok(());
        }
        self.backend.retire(stream, generation)?;
        if let Some(dump) = &mut self.dump {
            dump.retire(stream, generation)?;
        }
        Ok(())
    }

    fn take_output(&mut self) -> VecDeque<SourceTransportPacket> {
        std::mem::take(&mut self.output)
    }
}

impl Drop for EncodedSourceState {
    fn drop(&mut self) {
        // Best effort before ordinary teardown; process abort/SIGKILL may skip it.
        self.report_observations(true, |_| None);
    }
}

/// Source relay port that schedules encoded commits over a binding-owned transport.
pub struct EncodedSourcePort<T> {
    transport: T,
    state: Option<EncodedSourceState>,
    output: SourceOutput,
}

impl<T: EncodedSourceTransport> EncodedSourcePort<T> {
    pub fn new(transport: T, backend: Box<dyn EncodeBackend>) -> Self {
        Self {
            transport,
            state: Some(EncodedSourceState::new(backend)),
            output: SourceOutput::default(),
        }
    }

    /// Retain this weak control handle before moving the port into an adapter.
    /// None means the backend does not advertise rate control.
    pub fn encoder_rate_control(&self) -> Option<EncoderRateControl> {
        self.state
            .as_ref()?
            .rates
            .as_ref()
            .map(EncoderRates::control)
    }

    pub fn with_access_unit_dump_directory(
        mut self,
        directory: PathBuf,
        codec: VideoCodec,
    ) -> Result<Self> {
        let state = self
            .state
            .take()
            .context("encoded source state disappeared")?;
        self.state = Some(state.with_access_unit_dump_directory(directory, codec)?);
        Ok(self)
    }

    fn flush(&mut self) -> HoistPortResult<()> {
        let state = self
            .state
            .as_mut()
            .ok_or_else(|| protocol_error("encoded source port is disconnected"))?;
        loop {
            for packet in state.take_output() {
                self.output.push(packet).map_err(protocol_error)?;
            }
            self.output.flush(&self.transport)?;
            state.retained_output_records = self.output.pending_records();
            state.transport_blocked = !self.output.is_empty() || !self.transport.media_headroom();
            state.schedule().map_err(protocol_error)?;
            if state.output.is_empty() {
                break;
            }
        }
        Ok(())
    }

    fn refresh_admission(&mut self) {
        if let Some(state) = &mut self.state {
            state.transport_blocked = !self.output.is_empty() || !self.transport.media_headroom();
        }
    }
}

impl<T: EncodedSourceTransport> HoistSourcePort for EncodedSourcePort<T> {
    fn submit(&mut self, command: SourcePortCommand) -> HoistPortResult<()> {
        self.refresh_admission();
        match command {
            SourcePortCommand::MapSurface { session, surface } => {
                self.output
                    .push(SourceTransportPacket::Control(SourceEnvelope {
                        session,
                        message: SourceMessage::Mapped { surface },
                    }))
                    .map_err(protocol_error)?;
                self.flush()
            }
            SourcePortCommand::Surface { session, event } => {
                self.state
                    .as_mut()
                    .ok_or_else(|| protocol_error("encoded source port is disconnected"))?
                    .enqueue(session, event)
                    .map_err(protocol_error)?;
                self.flush()
            }
            SourcePortCommand::WithdrawSurface { session, surface } => {
                self.output
                    .push(SourceTransportPacket::Control(SourceEnvelope {
                        session,
                        message: SourceMessage::Withdraw { surface },
                    }))
                    .map_err(protocol_error)?;
                self.state
                    .as_mut()
                    .ok_or_else(|| protocol_error("encoded source port is disconnected"))?
                    .cancel_surface(surface)
                    .map_err(protocol_error)?;
                self.flush()
            }
            SourcePortCommand::RetireUpstreamBuffer(_) => Ok(()),
            SourcePortCommand::Cursor {
                session,
                update,
                sequence,
            } => {
                self.output
                    .push(SourceTransportPacket::Control(SourceEnvelope {
                        session,
                        message: SourceMessage::Cursor { update, sequence },
                    }))
                    .map_err(protocol_error)?;
                self.flush()
            }
        }
    }

    fn poll(&mut self) -> HoistPortResult<Vec<DestinationEnvelope>> {
        self.refresh_admission();
        let state = self
            .state
            .as_mut()
            .ok_or_else(|| protocol_error("encoded source port is disconnected"))?;
        state.drain().map_err(protocol_error)?;
        state.report_observations(false, |now| self.transport.observations(now));
        self.flush()?;
        self.transport.drain()
    }

    fn accept_destination(&mut self, envelope: &DestinationEnvelope) -> HoistPortResult<()> {
        match &envelope.message {
            DestinationMessage::Request(ClientRequest::Surface(request))
                if matches!(request.kind, ClientSurfaceRequestKind::Configure { .. }) =>
            {
                if let ClientSurfaceRequestKind::Configure { resizing, .. } = request.kind {
                    self.state
                        .as_mut()
                        .ok_or_else(|| protocol_error("encoded source port is disconnected"))?
                        .set_resizing(request.surface, resizing)
                        .map_err(protocol_error)?;
                }
            }
            DestinationMessage::BufferReleased { .. } => {
                return Err(protocol_error(
                    "encoded peer sent a native buffer-release record",
                ));
            }
            DestinationMessage::Request(_)
            | DestinationMessage::Input(_)
            | DestinationMessage::Reclaim
            | DestinationMessage::CursorReceived { .. } => {}
        }
        self.flush()
    }

    fn effects_drained(&mut self) {}

    fn disconnect(&mut self) {
        self.transport.disconnect();
        self.state = None;
        self.output = SourceOutput::default();
    }
}

struct QueuedDestinationEvent {
    session: HoistSessionId,
    source_surface: ClientSurfaceId,
    event: WireClientSurfaceEvent<EncodedBuffer>,
    received_at: Instant,
}

struct PendingMedia {
    session: HoistSessionId,
    access_unit: EncodedAccessUnit,
    received_at: Instant,
}

struct EncodedDestinationEvent {
    session: HoistSessionId,
    event: ClientSurfaceEvent,
}

struct InFlightDecode {
    token: u64,
    frame: MediaFrameId,
    cancelled: bool,
    submitted_at: Instant,
}

struct EncodedDestinationState {
    backend: Box<dyn DecodeBackend>,
    descriptor: ClientSourceDescriptor,
    dmabuf: Option<DmabufContext>,
    queues: HashMap<ClientSurfaceId, VecDeque<QueuedDestinationEvent>>,
    media_frames: HashMap<MediaFrameId, PendingMedia>,
    decoded: HashMap<MediaFrameId, weld_core::dmabuf::ExternalDmabuf>,
    decode_in_flight: Option<InFlightDecode>,
    cancelled_frames: HashMap<MediaFrameId, HoistSessionId>,
    streams: HashMap<(ClientSurfaceId, SurfaceLayerId), EncodedGeneration>,
    pending_retirement: HashSet<EncodedGeneration>,
    next_token: Option<u64>,
    next_buffer: Option<u64>,
    next_use: Option<u64>,
    observations: DestinationObservations,
}

impl EncodedDestinationState {
    fn new(
        backend: Box<dyn DecodeBackend>,
        descriptor: ClientSourceDescriptor,
        dmabuf: DmabufContext,
    ) -> Self {
        Self::new_with_dmabuf(backend, descriptor, Some(dmabuf))
    }

    fn new_with_dmabuf(
        backend: Box<dyn DecodeBackend>,
        descriptor: ClientSourceDescriptor,
        dmabuf: Option<DmabufContext>,
    ) -> Self {
        Self {
            backend,
            descriptor,
            dmabuf,
            queues: HashMap::new(),
            media_frames: HashMap::new(),
            decoded: HashMap::new(),
            decode_in_flight: None,
            cancelled_frames: HashMap::new(),
            streams: HashMap::new(),
            pending_retirement: HashSet::new(),
            next_token: Some(1),
            next_buffer: Some(1),
            next_use: Some(1),
            observations: DestinationObservations::new(Instant::now()),
        }
    }

    #[cfg(test)]
    fn new_without_dmabuf(
        backend: Box<dyn DecodeBackend>,
        descriptor: ClientSourceDescriptor,
    ) -> Self {
        Self::new_with_dmabuf(backend, descriptor, None)
    }

    fn enqueue(
        &mut self,
        session: HoistSessionId,
        mut event: WireClientSurfaceEvent<EncodedBuffer>,
        output: &mut Vec<EncodedDestinationEvent>,
    ) -> Result<()> {
        let source_surface = event.surface;
        ensure!(
            encoded_frames(&event).len() <= MAX_REPLACEMENTS_PER_COMMIT,
            "encoded commit exceeds the replacement-frame limit"
        );
        if let WireClientSurfaceEventKind::Commit(commit) = &event.kind {
            ensure!(
                commit.alpha_mode == SurfaceAlphaMode::Discarded,
                "opaque encoded commit must declare discarded alpha"
            );
        }
        if matches!(event.kind, WireClientSurfaceEventKind::Destroyed) {
            self.cancel_surface(source_surface)?;
            output.push(EncodedDestinationEvent {
                session,
                event: event.try_into_client(|_, _| {
                    Err::<ClientBufferLease, _>(anyhow::anyhow!(
                        "destroy event unexpectedly carried a buffer"
                    ))
                })?,
            });
            return Ok(());
        }
        mark_encoded_buffers_opaque(&mut event);
        self.update_stream_generations(&event)?;
        tracing::trace!(
            ?source_surface,
            revision = ?commit_revision(&event),
            frames = encoded_frames(&event).len(),
            queued_events = self.queues.values().map(VecDeque::len).sum::<usize>(),
            "queued encoded destination control event"
        );
        let queued = self.queues.values().map(VecDeque::len).sum::<usize>();
        ensure!(
            queued < MAX_DESTINATION_EVENTS,
            "encoded destination event bound exceeded"
        );
        if matches!(event.kind, WireClientSurfaceEventKind::Commit(_)) {
            self.observations
                .record(DestinationObservation::CommitReceived);
        }
        self.queues
            .entry(source_surface)
            .or_default()
            .push_back(QueuedDestinationEvent {
                session,
                source_surface,
                event,
                received_at: Instant::now(),
            });
        self.advance(output)
    }

    fn receive_budget(&self) -> ReceiveBudget {
        let mut referenced = self.cancelled_frames.len();
        let mut events = 0_usize;
        for queued in self.queues.values().flatten() {
            events += 1;
            if let WireClientSurfaceEventKind::Commit(commit) = &queued.event.kind {
                referenced = referenced.saturating_add(
                    commit
                        .buffers
                        .iter()
                        .filter(|buffer| {
                            matches!(buffer.change, WireSurfaceBufferChange::Replaced { .. })
                        })
                        .count(),
                );
            }
        }
        let media_bytes = self.media_frames.values().fold(0_usize, |total, packet| {
            total.saturating_add(packet.access_unit.payload.len())
        });
        // Admit a prefix of control and media independently. Every control slot
        // reserves a whole maximum-sized commit, so no deferred record is needed.
        // Needed media precedes any media for unadmitted control; tombstones must
        // never subtract from MEDIA room, since their late media frees reference room.
        ReceiveBudget {
            control_records: (MAX_REFERENCED_FRAMES.saturating_sub(referenced)
                / MAX_REPLACEMENTS_PER_COMMIT)
                .min(MAX_DESTINATION_EVENTS.saturating_sub(events)),
            media_records: MAX_PENDING_MEDIA_FRAMES.saturating_sub(self.media_frames.len()),
            media_bytes: MAX_PENDING_MEDIA_BYTES.saturating_sub(media_bytes),
        }
    }

    fn enqueue_media(&mut self, packet: MediaEnvelope<EncodedAccessUnit>) -> Result<()> {
        let frame = packet.access_unit.frame;
        if let Some(session) = self.cancelled_frames.get(&frame) {
            ensure!(
                *session == packet.session,
                "cancelled encoded media crossed hoist sessions"
            );
            self.cancelled_frames.remove(&frame);
            self.observations
                .record(DestinationObservation::LateCancelledMedia);
            return Ok(());
        }
        ensure!(
            self.media_frames.len() < MAX_PENDING_MEDIA_FRAMES,
            "encoded destination media-frame bound exceeded"
        );
        ensure!(
            packet.access_unit.payload.len() <= self.receive_budget().media_bytes,
            "encoded destination payload-byte bound exceeded"
        );
        let payload_bytes = u64::try_from(packet.access_unit.payload.len()).unwrap_or(u64::MAX);
        ensure!(
            self.media_frames
                .insert(
                    frame,
                    PendingMedia {
                        session: packet.session,
                        access_unit: packet.access_unit,
                        received_at: Instant::now(),
                    }
                )
                .is_none(),
            "encoded media frame was delivered more than once"
        );
        self.observations
            .record(DestinationObservation::MediaReceived { payload_bytes });
        Ok(())
    }

    fn drain(&mut self, output: &mut Vec<EncodedDestinationEvent>) -> Result<()> {
        for completion in self.backend.drain() {
            let in_flight = self
                .decode_in_flight
                .take()
                .context("decoder completed without an in-flight frame")?;
            ensure!(
                completion.token == in_flight.token,
                "decoder completed an unexpected token"
            );
            if completion.result.is_err() {
                self.observations
                    .record(DestinationObservation::CodecFailed);
            }
            if in_flight.cancelled {
                self.observations
                    .record(DestinationObservation::DecodeCancelled);
                if let Err(error) = completion.result {
                    tracing::debug!(frame = ?in_flight.frame, error = %format_args!("{error:#}"),
                        "discarded cancelled decode failure");
                }
                self.pending_retirement
                    .insert((in_flight.frame.stream, in_flight.frame.generation));
                continue;
            }
            let frames = completion.result?;
            ensure!(
                frames.len() == 1,
                "low-delay decoder did not return exactly one frame"
            );
            let frame = frames
                .into_iter()
                .next()
                .context("low-delay decoder returned no frame")?;
            ensure!(
                frame.frame == in_flight.frame,
                "low-delay decoder returned another frame"
            );
            let wall_time = in_flight.submitted_at.elapsed();
            self.observations
                .record(DestinationObservation::DecodeCompleted { wall_time });
            tracing::trace!(
                frame = ?frame.frame,
                decode_micros = wall_time.as_micros(),
                "completed encoded destination decode"
            );
            self.decoded.insert(frame.frame, frame.dmabuf);
        }
        self.advance(output)
    }

    fn cancel_surface(&mut self, surface: ClientSurfaceId) -> Result<()> {
        if let Some(queue) = self.queues.remove(&surface) {
            for event in queue {
                let frames = encoded_frames(&event.event);
                for frame in &frames {
                    self.pending_retirement
                        .insert((frame.stream, frame.generation));
                    let media_was_pending = self.media_frames.remove(frame).is_some();
                    let was_decoded = self.decoded.remove(frame).is_some();
                    let in_flight = self
                        .decode_in_flight
                        .as_mut()
                        .filter(|submitted| submitted.frame == *frame);
                    if let Some(in_flight) = in_flight {
                        in_flight.cancelled = true;
                    } else if !media_was_pending && !was_decoded {
                        // Only media that has never arrived needs a tombstone.
                        ensure!(
                            self.cancelled_frames.len() < MAX_REFERENCED_FRAMES,
                            "encoded destination cancelled-frame bound exceeded"
                        );
                        self.cancelled_frames.insert(*frame, event.session);
                    }
                }
                if !frames.is_empty() {
                    self.observations
                        .record(DestinationObservation::CommitCancelled);
                }
            }
        }
        self.retire_surface_generations(surface)?;
        self.sweep_retirement()
    }

    fn update_stream_generations(
        &mut self,
        event: &WireClientSurfaceEvent<EncodedBuffer>,
    ) -> Result<()> {
        let WireClientSurfaceEventKind::Commit(commit) = &event.kind else {
            return Ok(());
        };
        // Inventories may advance ahead of decode. Retirement waits for all
        // references to the previous generation, not just the latest inventory.
        let retired = self
            .streams
            .extract_if(|(surface, layer), _| {
                *surface == event.surface
                    && !commit.buffers.iter().any(|buffer| {
                        buffer.layer == *layer
                            && !matches!(buffer.change, WireSurfaceBufferChange::Removed)
                    })
            })
            .map(|(_, generation)| generation)
            .collect::<Vec<_>>();
        for (stream, generation) in retired {
            self.pending_retirement.insert((stream, generation));
        }
        for update in &commit.buffers {
            let key = (event.surface, update.layer);
            let next = match &update.change {
                WireSurfaceBufferChange::Replaced { buffer, .. } => {
                    Some((buffer.frame.stream, buffer.frame.generation))
                }
                WireSurfaceBufferChange::Removed => None,
                WireSurfaceBufferChange::Retained { .. } => continue,
            };
            if let Some(previous) = self.streams.remove(&key)
                && Some(previous) != next
            {
                self.pending_retirement.insert(previous);
            }
            if let Some(next) = next {
                self.streams.insert(key, next);
            }
        }
        Ok(())
    }

    fn retire_surface_generations(&mut self, surface: ClientSurfaceId) -> Result<()> {
        let generations = self
            .streams
            .extract_if(|(candidate, _), _| *candidate == surface)
            .map(|(_, generation)| generation)
            .collect::<Vec<_>>();
        for (stream, generation) in generations {
            self.pending_retirement.insert((stream, generation));
        }
        Ok(())
    }

    fn sweep_retirement(&mut self) -> Result<()> {
        if self.pending_retirement.is_empty() {
            return Ok(());
        }
        let mut referenced = self.streams.values().copied().collect::<HashSet<_>>();
        referenced.extend(
            self.queues
                .values()
                .flatten()
                .flat_map(|queued| encoded_frames(&queued.event))
                .map(|frame| (frame.stream, frame.generation)),
        );
        referenced.extend(
            self.media_frames
                .keys()
                .chain(self.decoded.keys())
                .map(|frame| (frame.stream, frame.generation)),
        );
        if let Some(active) = &self.decode_in_flight {
            referenced.insert((active.frame.stream, active.frame.generation));
        }
        let ready = self
            .pending_retirement
            .difference(&referenced)
            .copied()
            .collect::<Vec<_>>();
        for (stream, generation) in ready {
            self.backend.retire(stream, generation)?;
            self.pending_retirement.remove(&(stream, generation));
        }
        Ok(())
    }

    fn advance(&mut self, output: &mut Vec<EncodedDestinationEvent>) -> Result<()> {
        self.sweep_retirement()?;
        loop {
            let surfaces = self.queues.keys().copied().collect::<Vec<_>>();
            let mut progressed = false;
            for surface in surfaces {
                let Some(front) = self.queues.get(&surface).and_then(|queue| queue.front()) else {
                    continue;
                };
                let frames = encoded_frames(&front.event);
                if frames.is_empty() {
                    let event = self.pop_event(surface)?;
                    output.push(EncodedDestinationEvent {
                        session: event.session,
                        event: event.event.try_into_client(|_, _| {
                            Err::<ClientBufferLease, _>(anyhow::anyhow!(
                                "encoded destination received native buffer content"
                            ))
                        })?,
                    });
                    progressed = true;
                    continue;
                }
                if frames.iter().all(|frame| self.decoded.contains_key(frame)) {
                    let queued = self.pop_event(surface)?;
                    let revision = commit_revision(&queued.event)
                        .context("decoded destination event was not a commit")?;
                    let event = queued.event.try_into_client(|buffer, _| {
                        let dmabuf = self
                            .decoded
                            .remove(&buffer.frame)
                            .context("decoded layer frame disappeared")?;
                        self.import_decoded(dmabuf)
                    })?;
                    output.push(EncodedDestinationEvent {
                        session: queued.session,
                        event,
                    });
                    self.observations
                        .record(DestinationObservation::CommitApplied {
                            wall_time: queued.received_at.elapsed(),
                        });
                    tracing::trace!(
                        source_surface = ?queued.source_surface,
                        ?revision,
                        frames = frames.len(),
                        queued_events = self.queues.values().map(VecDeque::len).sum::<usize>(),
                        "applied encoded destination commit"
                    );
                    progressed = true;
                    continue;
                }
                if let Some(frame) = frames
                    .into_iter()
                    .find(|frame| !self.decoded.contains_key(frame))
                {
                    self.schedule_decode(frame, front.session)?;
                }
            }
            self.queues.retain(|_, queue| !queue.is_empty());
            if !progressed {
                break;
            }
        }
        self.sweep_retirement()
    }

    fn schedule_decode(&mut self, frame: MediaFrameId, session: HoistSessionId) -> Result<()> {
        self.sweep_retirement()?;
        if self.decode_in_flight.is_some() {
            return Ok(());
        }
        let Some(media) = self.media_frames.remove(&frame) else {
            return Ok(());
        };
        ensure!(
            media.session == session,
            "encoded media crossed hoist sessions"
        );
        let metadata = self
            .queues
            .values()
            .flat_map(|queue| queue.iter())
            .find_map(|event| encoded_metadata(&event.event, frame))
            .context("encoded control metadata disappeared")?;
        let token = take_counter(&mut self.next_token, "encoded decode token")?;
        let request = DecodeRequest {
            token,
            access_unit: media.access_unit,
            visible_width: metadata.extent.width,
            visible_height: metadata.extent.height,
        };
        let submitted_at = Instant::now();
        match self.backend.try_submit(request) {
            Ok(()) => {
                self.observations
                    .record(DestinationObservation::DecodeSubmitted {
                        media_wait: submitted_at.saturating_duration_since(media.received_at),
                    });
                self.decode_in_flight = Some(InFlightDecode {
                    token,
                    frame,
                    cancelled: false,
                    submitted_at,
                });
                Ok(())
            }
            Err(SubmitError::Busy(_)) => {
                bail!("decoder queue was busy without an in-flight frame")
            }
            Err(SubmitError::Stopped(_)) => bail!("decoder worker stopped"),
            Err(SubmitError::Rejected(error)) => Err(error),
        }
    }

    fn pop_event(&mut self, surface: ClientSurfaceId) -> Result<QueuedDestinationEvent> {
        self.queues
            .get_mut(&surface)
            .and_then(VecDeque::pop_front)
            .context("encoded destination queue disappeared")
    }

    fn observation_gauges(&self, now: Instant) -> DestinationGauges {
        DestinationGauges {
            pending_events: self.queues.values().map(VecDeque::len).sum(),
            pending_media_frames: self.media_frames.len(),
            pending_media_bytes: self.media_frames.values().fold(0_u64, |total, media| {
                total.saturating_add(
                    u64::try_from(media.access_unit.payload.len()).unwrap_or(u64::MAX),
                )
            }),
            decoded_frames: self.decoded.len(),
            active_streams: self.streams.len(),
            decode_in_flight: self.decode_in_flight.is_some(),
            oldest_control_age: self
                .queues
                .values()
                .filter_map(|queue| queue.front())
                .map(|event| now.saturating_duration_since(event.received_at))
                .max()
                .unwrap_or_default(),
            oldest_media_age: self
                .media_frames
                .values()
                .map(|media| now.saturating_duration_since(media.received_at))
                .max()
                .unwrap_or_default(),
            active_decode_age: self
                .decode_in_flight
                .as_ref()
                .map(|active| now.saturating_duration_since(active.submitted_at))
                .unwrap_or_default(),
        }
    }

    fn report_observations(&mut self, final_report: bool) {
        let now = Instant::now();
        if !self.observations.report_due(now, final_report) {
            return;
        }
        let gauges = self.observation_gauges(now);
        if let Some(report) = self.observations.take_report(now, gauges, final_report) {
            report.emit();
        }
    }

    fn import_decoded(
        &mut self,
        dmabuf: weld_core::dmabuf::ExternalDmabuf,
    ) -> Result<ClientBufferLease> {
        let context = self
            .dmabuf
            .as_ref()
            .context("DMA-BUF import is unavailable in this encoded destination")?;
        let metadata = ClientBufferMetadata::new(dmabuf.extent, true);
        let access = context.import_external(dmabuf)?;
        let access_for_release = access.clone();
        let dmabuf_for_release = context.clone();
        let buffer = ClientBufferId::new(
            self.descriptor.id,
            take_counter(&mut self.next_buffer, "decoded buffer")?,
        );
        let use_id = ClientBufferUseId::new(
            self.descriptor.id,
            take_counter(&mut self.next_use, "decoded buffer use")?,
        );
        context.lease_external(buffer, use_id, metadata, access, move |_| {
            dmabuf_for_release.remove_external(&access_for_release)
        })
    }
}

impl Drop for EncodedDestinationState {
    fn drop(&mut self) {
        // Best effort before ordinary teardown; process abort/SIGKILL may skip it.
        self.report_observations(true);
    }
}

/// Destination relay port that reconstructs decoded client commits.
pub struct EncodedDestinationPort<T> {
    transport: T,
    state: Option<EncodedDestinationState>,
}

impl<T: EncodedDestinationTransport> EncodedDestinationPort<T> {
    pub fn new(
        transport: T,
        backend: Box<dyn DecodeBackend>,
        descriptor: ClientSourceDescriptor,
        dmabuf: DmabufContext,
    ) -> Self {
        Self {
            transport,
            state: Some(EncodedDestinationState::new(backend, descriptor, dmabuf)),
        }
    }

    #[cfg(test)]
    fn new_without_dmabuf(
        transport: T,
        backend: Box<dyn DecodeBackend>,
        descriptor: ClientSourceDescriptor,
    ) -> Self {
        Self {
            transport,
            state: Some(EncodedDestinationState::new_without_dmabuf(
                backend, descriptor,
            )),
        }
    }

    fn apply_source_packet(
        &mut self,
        packet: SourceTransportPacket,
        records: &mut Vec<DestinationPortRecord>,
    ) -> HoistPortResult<()> {
        match packet {
            SourceTransportPacket::Media(media) => {
                self.state
                    .as_mut()
                    .ok_or_else(|| protocol_error("encoded destination port is disconnected"))?
                    .enqueue_media(media)
                    .map_err(protocol_error)?;
            }
            SourceTransportPacket::Control(packet) => match packet.message {
                SourceMessage::Cursor { update, sequence } => records.push(DestinationPortRecord {
                    session: packet.session,
                    event: DestinationPortEvent::Cursor { update, sequence },
                }),
                SourceMessage::Mapped { surface } => records.push(DestinationPortRecord {
                    session: packet.session,
                    event: DestinationPortEvent::MappedSurface(surface),
                }),
                SourceMessage::Surface(event) => {
                    let mut decoded = Vec::new();
                    self.state
                        .as_mut()
                        .ok_or_else(|| protocol_error("encoded destination port is disconnected"))?
                        .enqueue(packet.session, event, &mut decoded)
                        .map_err(protocol_error)?;
                    extend_encoded_records(records, decoded);
                }
                SourceMessage::Withdraw { surface } => {
                    self.state
                        .as_mut()
                        .ok_or_else(|| protocol_error("encoded destination port is disconnected"))?
                        .cancel_surface(surface)
                        .map_err(protocol_error)?;
                    records.push(DestinationPortRecord {
                        session: packet.session,
                        event: DestinationPortEvent::WithdrawSurface(surface),
                    });
                }
                SourceMessage::Ended => records.push(DestinationPortRecord {
                    session: packet.session,
                    event: DestinationPortEvent::Ended,
                }),
                SourceMessage::BufferRetired { .. } => {
                    return Err(protocol_error(
                        "encoded peer sent a native buffer-retirement record",
                    ));
                }
            },
        }
        Ok(())
    }
}

impl<T: EncodedDestinationTransport> HoistDestinationPort for EncodedDestinationPort<T> {
    fn poll(&mut self) -> HoistPortResult<Vec<DestinationPortRecord>> {
        let budget = self
            .state
            .as_ref()
            .ok_or_else(|| protocol_error("encoded destination port is disconnected"))?
            .receive_budget();
        let mut records = Vec::new();
        for packet in self.transport.drain(budget)? {
            self.apply_source_packet(packet, &mut records)?;
        }
        let mut decoded = Vec::new();
        let state = self
            .state
            .as_mut()
            .ok_or_else(|| protocol_error("encoded destination port is disconnected"))?;
        let result = state.drain(&mut decoded);
        state.report_observations(result.is_err());
        result.map_err(protocol_error)?;
        extend_encoded_records(&mut records, decoded);
        self.transport.wake_if_readable(state.receive_budget())?;
        Ok(records)
    }

    fn submit(&mut self, command: DestinationPortCommand) -> HoistPortResult<()> {
        match command {
            DestinationPortCommand::Message(envelope) => self.transport.send(envelope),
            DestinationPortCommand::RouteMapped { .. }
            | DestinationPortCommand::RouteUnmapped { .. } => Ok(()),
        }
    }

    fn disconnect(&mut self) {
        self.transport.disconnect();
        self.state = None;
    }
}

fn extend_encoded_records(
    records: &mut Vec<DestinationPortRecord>,
    decoded: Vec<EncodedDestinationEvent>,
) {
    records.extend(decoded.into_iter().map(|decoded| DestinationPortRecord {
        session: decoded.session,
        event: DestinationPortEvent::Surface(decoded.event),
    }));
}

fn protocol_error(error: impl std::fmt::Display) -> HoistPortError {
    Box::new(EncodedPortError(format!("{error:#}")))
}

#[derive(Debug)]
struct EncodedPortError(String);

impl std::fmt::Display for EncodedPortError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for EncodedPortError {}

fn replaced_buffer_count(event: &ClientSurfaceEvent) -> usize {
    match &event.kind {
        ClientSurfaceEventKind::Commit(commit) => commit
            .buffers
            .iter()
            .filter(|update| matches!(update.change, SurfaceBufferChange::Replaced { .. }))
            .count(),
        _ => 0,
    }
}

fn commit_revision(event: &WireClientSurfaceEvent<EncodedBuffer>) -> Option<ClientCommitRevision> {
    match &event.kind {
        WireClientSurfaceEventKind::Commit(commit) => Some(commit.revision),
        WireClientSurfaceEventKind::Role(_)
        | WireClientSurfaceEventKind::Interaction(_)
        | WireClientSurfaceEventKind::Destroyed => None,
    }
}

fn encoded_frames(event: &WireClientSurfaceEvent<EncodedBuffer>) -> Vec<MediaFrameId> {
    let WireClientSurfaceEventKind::Commit(commit) = &event.kind else {
        return Vec::new();
    };
    commit
        .buffers
        .iter()
        .filter_map(|update| match &update.change {
            WireSurfaceBufferChange::Replaced { buffer, .. } => Some(buffer.frame),
            WireSurfaceBufferChange::Retained { .. } | WireSurfaceBufferChange::Removed => None,
        })
        .collect()
}

fn encoded_metadata(
    event: &WireClientSurfaceEvent<EncodedBuffer>,
    frame: MediaFrameId,
) -> Option<ClientBufferMetadata> {
    let WireClientSurfaceEventKind::Commit(commit) = &event.kind else {
        return None;
    };
    commit
        .buffers
        .iter()
        .find_map(|update| match &update.change {
            WireSurfaceBufferChange::Replaced { metadata, buffer } if buffer.frame == frame => {
                Some(*metadata)
            }
            _ => None,
        })
}

fn mark_encoded_buffers_opaque(event: &mut WireClientSurfaceEvent<EncodedBuffer>) {
    let WireClientSurfaceEventKind::Commit(commit) = &mut event.kind else {
        return;
    };
    for update in &mut commit.buffers {
        if let WireSurfaceBufferChange::Replaced { metadata, .. }
        | WireSurfaceBufferChange::Retained { metadata } = &mut update.change
        {
            metadata.opaque = true;
        }
    }
}

fn take_counter(counter: &mut Option<u64>, name: &str) -> Result<u64> {
    let value = counter.context(format!("{name} space is exhausted"))?;
    *counter = value.checked_add(1);
    Ok(value)
}

#[cfg(test)]
mod tests {
    mod bitrate_tests;
    use std::{
        cell::{Cell, RefCell},
        rc::Rc,
        time::Duration,
    };

    use weld_client::{
        ClientAdapter, ClientAdapterCommandEnvelope, ClientBufferId, ClientBufferUseId,
        ClientEventQueue, ClientId, ClientSourceId, ClientSurfaceCommit, Extent,
        SurfaceBufferUpdate, ToplevelInteractionRequestKind,
    };
    use weld_hoist_core::{HoistEndpointCommand, SourceRelayAdapter};
    use weld_media::EncodedFrameKind;

    use super::*;
    use crate::codec::{DecodeCompletion, DecodedFrame, EncodeCompletion};

    #[derive(Default)]
    struct FakeSourceTransportState {
        sent: Vec<SourceTransportPacket>,
        incoming: VecDeque<DestinationEnvelope>,
        disconnected: bool,
        block_control: bool,
        block_media: bool,
    }

    #[derive(Clone)]
    struct FakeSourceTransport(Rc<RefCell<FakeSourceTransportState>>);

    impl EncodedSourceTransport for FakeSourceTransport {
        fn try_send(
            &self,
            packet: SourceTransportPacket,
        ) -> HoistPortResult<SendStatus<SourceTransportPacket>> {
            let state = self.0.borrow();
            if match &packet {
                SourceTransportPacket::Control(_) => state.block_control,
                SourceTransportPacket::Media(_) => state.block_media,
            } {
                return Ok(SendStatus::Busy(packet));
            }
            drop(state);
            self.0.borrow_mut().sent.push(packet);
            Ok(SendStatus::Sent)
        }

        fn media_headroom(&self) -> bool {
            !self.0.borrow().block_media
        }

        fn drain(&self) -> HoistPortResult<Vec<DestinationEnvelope>> {
            Ok(self.0.borrow_mut().incoming.drain(..).collect())
        }

        fn disconnect(&self) {
            self.0.borrow_mut().disconnected = true;
        }
    }

    #[derive(Default)]
    struct FakeDestinationTransportState {
        sent: Vec<DestinationEnvelope>,
        incoming: VecDeque<SourceTransportPacket>,
        disconnected: bool,
    }

    #[derive(Clone)]
    struct FakeDestinationTransport(Rc<RefCell<FakeDestinationTransportState>>);

    impl EncodedDestinationTransport for FakeDestinationTransport {
        fn send(&self, packet: DestinationEnvelope) -> HoistPortResult<()> {
            self.0.borrow_mut().sent.push(packet);
            Ok(())
        }

        fn drain(&self, mut budget: ReceiveBudget) -> HoistPortResult<Vec<SourceTransportPacket>> {
            let mut state = self.0.borrow_mut();
            let mut packets = Vec::new();
            let mut retained = VecDeque::new();
            let mut media_blocked = false;
            for packet in state.incoming.drain(..) {
                match &packet {
                    SourceTransportPacket::Control(_) if budget.control_records > 0 => {
                        budget.control_records -= 1;
                        packets.push(packet);
                    }
                    SourceTransportPacket::Media(media)
                        if !media_blocked
                            && budget.media_records > 0
                            && media.access_unit.payload.len() <= budget.media_bytes =>
                    {
                        budget.media_records -= 1;
                        budget.media_bytes -= media.access_unit.payload.len();
                        packets.push(packet);
                    }
                    SourceTransportPacket::Media(_) => {
                        media_blocked = true;
                        retained.push_back(packet);
                    }
                    _ => retained.push_back(packet),
                }
            }
            state.incoming = retained;
            Ok(packets)
        }

        fn wake_if_readable(&self, _budget: ReceiveBudget) -> HoistPortResult<()> {
            Ok(())
        }

        fn disconnect(&self) {
            self.0.borrow_mut().disconnected = true;
        }
    }

    #[derive(Default)]
    struct FakeEncoderState {
        submitted: Vec<(u64, MediaFrameId, Vec<u8>)>,
        completions: Vec<EncodeCompletion>,
        retirements: Vec<(MediaStreamId, StreamGeneration)>,
        generations: HashSet<EncodedGeneration>,
        generation_limit: Option<usize>,
        bitrate_limits: Option<crate::EncoderBitrateLimits>,
        submitted_bitrates: Vec<Option<u64>>,
        generation_bitrates: HashMap<EncodedGeneration, Option<u64>>,
    }

    struct FakeEncoder(Rc<RefCell<FakeEncoderState>>);

    impl EncodeBackend for FakeEncoder {
        fn bitrate_limits(&self) -> Option<crate::EncoderBitrateLimits> {
            self.0.borrow().bitrate_limits
        }
        fn try_submit(&mut self, request: EncodeRequest) -> Result<(), SubmitError<EncodeRequest>> {
            let EncodeInput::PackedBgra { pixels, .. } = request.input else {
                return Err(SubmitError::Rejected(anyhow::anyhow!(
                    "test expected packed BGRA"
                )));
            };
            let mut state = self.0.borrow_mut();
            let generation = (request.frame.stream, request.frame.generation);
            if state
                .generation_bitrates
                .get(&generation)
                .is_some_and(|rate| *rate != request.bitrate_bits_per_second)
            {
                return Err(SubmitError::Rejected(anyhow::anyhow!(
                    "fake encoder settings changed within generation"
                )));
            }
            if !state.generations.contains(&generation)
                && state
                    .generation_limit
                    .is_some_and(|limit| state.generations.len() >= limit)
            {
                return Err(SubmitError::Rejected(anyhow::anyhow!(
                    "fake encoder stream budget exhausted"
                )));
            }
            state.generations.insert(generation);
            state
                .generation_bitrates
                .insert(generation, request.bitrate_bits_per_second);
            state
                .submitted_bitrates
                .push(request.bitrate_bits_per_second);
            state.submitted.push((request.token, request.frame, pixels));
            Ok(())
        }

        fn drain(&mut self) -> Vec<EncodeCompletion> {
            std::mem::take(&mut self.0.borrow_mut().completions)
        }

        fn retire(&mut self, stream: MediaStreamId, generation: StreamGeneration) -> Result<()> {
            self.0
                .borrow_mut()
                .generation_bitrates
                .remove(&(stream, generation));
            self.0
                .borrow_mut()
                .generations
                .remove(&(stream, generation));
            self.0.borrow_mut().retirements.push((stream, generation));
            Ok(())
        }
    }

    #[derive(Default)]
    struct FakeDecoderState {
        submitted: Vec<MediaFrameId>,
        tokens: Vec<u64>,
        completions: Vec<DecodeCompletion>,
        retirements: Vec<(MediaStreamId, StreamGeneration)>,
    }

    struct FakeDecoder(Rc<RefCell<FakeDecoderState>>);

    impl DecodeBackend for FakeDecoder {
        fn try_submit(&mut self, request: DecodeRequest) -> Result<(), SubmitError<DecodeRequest>> {
            self.0.borrow_mut().tokens.push(request.token);
            self.0
                .borrow_mut()
                .submitted
                .push(request.access_unit.frame);
            Ok(())
        }

        fn drain(&mut self) -> Vec<DecodeCompletion> {
            std::mem::take(&mut self.0.borrow_mut().completions)
        }

        fn retire(&mut self, stream: MediaStreamId, generation: StreamGeneration) -> Result<()> {
            self.0.borrow_mut().retirements.push((stream, generation));
            Ok(())
        }
    }

    fn source() -> (EncodedSourceState, Rc<RefCell<FakeEncoderState>>) {
        let state = Rc::new(RefCell::new(FakeEncoderState::default()));
        (
            EncodedSourceState::new(Box::new(FakeEncoder(state.clone()))),
            state,
        )
    }

    fn source_port() -> (
        EncodedSourcePort<FakeSourceTransport>,
        Rc<RefCell<FakeSourceTransportState>>,
        Rc<RefCell<FakeEncoderState>>,
    ) {
        let transport = Rc::new(RefCell::new(FakeSourceTransportState::default()));
        let encoder = Rc::new(RefCell::new(FakeEncoderState::default()));
        (
            EncodedSourcePort::new(
                FakeSourceTransport(transport.clone()),
                Box::new(FakeEncoder(encoder.clone())),
            ),
            transport,
            encoder,
        )
    }

    #[test]
    fn cursor_control_bypasses_an_unfinished_encode_on_both_ports() {
        let (mut source, transport, _) = source_port();
        let (mut destination, destination_transport, _) = destination_port();
        let source_id = ClientSourceId::new(1);
        let surface = surface(source_id, 1, 1);
        let session = HoistSessionId::new(1);
        let metadata = ClientBufferMetadata::new(Extent::new(1, 1), true);
        source
            .submit(SourcePortCommand::Surface {
                session,
                event: one_buffer_commit(surface, 1, 1, shm_lease(source_id, 1, 10, metadata)),
            })
            .expect("encode");
        source
            .submit(SourcePortCommand::Cursor {
                session,
                update: weld_client::ClientCursorUpdate {
                    surface,
                    cursor: weld_client::ClientCursor::Hidden,
                },
                sequence: 7,
            })
            .expect("cursor bypass");
        assert!(source.state.as_ref().expect("state").in_flight.is_some());
        assert_eq!(transport.borrow().sent.len(), 1);
        destination_transport
            .borrow_mut()
            .incoming
            .extend(transport.borrow_mut().sent.drain(..));
        let records = destination.poll().expect("cursor before any decoded frame");
        assert!(matches!(
            records.as_slice(),
            [DestinationPortRecord {
                event: DestinationPortEvent::Cursor { sequence: 7, .. },
                ..
            }]
        ));
    }

    #[test]
    fn cursor_overtaking_resize_blocked_unmap_remap_keeps_its_newer_preference() {
        let (mut source, transport, encoder) = source_port();
        let (destination, incoming, _) = destination_port();
        let source_id = ClientSourceId::new(1);
        let destination_id = ClientSourceId::new(9);
        let surface = surface(source_id, 1, 1);
        let session = HoistSessionId::new(1);
        source
            .submit(SourcePortCommand::MapSurface { session, surface })
            .expect("map");
        let metadata = ClientBufferMetadata::new(Extent::new(1, 1), true);
        source
            .submit(SourcePortCommand::Surface {
                session,
                event: one_buffer_commit(surface, 1, 1, shm_lease(source_id, 1, 10, metadata)),
            })
            .expect("first frame");
        let (token, frame, _) = encoder.borrow().submitted[0].clone();
        complete(&encoder, token, frame, 10);
        source.poll().expect("publish first frame");
        source
            .state
            .as_mut()
            .expect("state")
            .set_resizing(surface, true)
            .expect("resize");

        // Model an already-displayed first frame while resizing blocks commits.
        // Strip only its GPU payload: this test exercises cursor/lifecycle order,
        // not decoding or native buffer import.
        for packet in transport.borrow_mut().sent.drain(..) {
            if let SourceTransportPacket::Control(mut envelope) = packet {
                if let SourceMessage::Surface(WireClientSurfaceEvent {
                    kind: WireClientSurfaceEventKind::Commit(commit),
                    ..
                }) = &mut envelope.message
                {
                    commit.buffers.clear();
                }
                incoming
                    .borrow_mut()
                    .incoming
                    .push_back(SourceTransportPacket::Control(envelope));
            }
        }
        let descriptor =
            ClientSourceDescriptor::new(destination_id, weld_client::ClientProvenance::Relocated);
        let relay =
            weld_hoist_core::DestinationRelayAdapter::new(source_id, descriptor, destination);
        let mut runtime = weld_client::ClientRuntime::default();
        runtime
            .register(weld_client::ClientRuntimeAdapter::new(descriptor, relay))
            .expect("register receiver");
        runtime.drain_events(&mut ClientEventQueue::default(), &mut Vec::new());
        let relocated = weld_hoist_core::relocated_surface(destination_id, surface);
        runtime.set_pointer_route(Some(weld_client::ClientPointerRoute {
            surface: relocated,
            layer: SurfaceLayerId::new(1),
            transform: weld_client::InputTransform::IDENTITY,
        }));

        let mut unmap = commit(surface, 2, Vec::new());
        if let ClientSurfaceEventKind::Commit(commit) = &mut unmap.kind {
            commit.mapped = false;
        }
        for event in [unmap, commit(surface, 3, Vec::new())] {
            source
                .submit(SourcePortCommand::Surface { session, event })
                .expect("locally blocked lifecycle");
        }
        assert!(transport.borrow().sent.is_empty());
        let cursor = weld_client::ClientCursor::Named(weld_client::CursorIcon::Text);
        source
            .submit(SourcePortCommand::Cursor {
                session,
                update: weld_client::ClientCursorUpdate {
                    surface,
                    cursor: cursor.clone(),
                },
                sequence: 1,
            })
            .expect("newer cursor bypass");
        incoming
            .borrow_mut()
            .incoming
            .extend(transport.borrow_mut().sent.drain(..));
        runtime.drain_events(&mut ClientEventQueue::default(), &mut Vec::new());
        assert_eq!(runtime.pointer_cursor(), Some((relocated, cursor.clone())));

        source
            .state
            .as_mut()
            .expect("state")
            .set_resizing(surface, false)
            .expect("end resize");
        source.flush().expect("flush lifecycle");
        let delayed = std::mem::take(&mut transport.borrow_mut().sent);
        assert_eq!(delayed.len(), 2);
        for (index, packet) in delayed.into_iter().enumerate() {
            incoming.borrow_mut().incoming.push_back(packet);
            runtime.drain_events(&mut ClientEventQueue::default(), &mut Vec::new());
            assert_eq!(
                runtime.pointer_cursor(),
                (index == 1).then(|| (relocated, cursor.clone()))
            );
        }
    }

    fn destination_port() -> (
        EncodedDestinationPort<FakeDestinationTransport>,
        Rc<RefCell<FakeDestinationTransportState>>,
        Rc<RefCell<FakeDecoderState>>,
    ) {
        let transport = Rc::new(RefCell::new(FakeDestinationTransportState::default()));
        let decoder = Rc::new(RefCell::new(FakeDecoderState::default()));
        let descriptor = ClientSourceDescriptor::new(
            ClientSourceId::new(9),
            weld_client::ClientProvenance::Relocated,
        );
        (
            EncodedDestinationPort::new_without_dmabuf(
                FakeDestinationTransport(transport.clone()),
                Box::new(FakeDecoder(decoder.clone())),
                descriptor,
            ),
            transport,
            decoder,
        )
    }

    fn surface(source: ClientSourceId, client: u64, local: u64) -> ClientSurfaceId {
        ClientSurfaceId::new(ClientId::new(source, client), local)
    }

    fn shm_lease(
        source: ClientSourceId,
        local: u64,
        pixel: u8,
        metadata: ClientBufferMetadata,
    ) -> ClientBufferLease {
        ClientBufferLease::new(
            ClientBufferId::new(source, local),
            ClientBufferUseId::new(source, local),
            metadata,
            Rc::new(DirectClientBufferAccess::Shm(
                weld_core::dmabuf::WaylandShmBuffer {
                    bgra_pixels: vec![pixel, pixel, pixel, 255],
                },
            )),
            |_| {},
        )
        .expect("matching source")
    }

    fn commit(
        surface: ClientSurfaceId,
        revision: u64,
        buffers: Vec<SurfaceBufferUpdate>,
    ) -> ClientSurfaceEvent {
        ClientSurfaceEvent {
            surface,
            kind: ClientSurfaceEventKind::Commit(ClientSurfaceCommit {
                revision: ClientCommitRevision::new(revision),
                alpha_mode: Default::default(),
                mapped: true,
                root: None,
                window_geometry: None,
                overlays: Vec::new(),
                inputs: Vec::new(),
                buffers,
            }),
        }
    }

    fn one_buffer_commit(
        surface: ClientSurfaceId,
        revision: u64,
        layer: u64,
        lease: ClientBufferLease,
    ) -> ClientSurfaceEvent {
        let metadata = lease.metadata();
        commit(
            surface,
            revision,
            vec![SurfaceBufferUpdate {
                layer: SurfaceLayerId::new(layer),
                change: SurfaceBufferChange::Replaced {
                    metadata,
                    buffer: lease,
                },
            }],
        )
    }

    fn encoded_commit(
        surface: ClientSurfaceId,
        revision: u64,
        frame: MediaFrameId,
    ) -> WireClientSurfaceEvent<EncodedBuffer> {
        WireClientSurfaceEvent {
            surface,
            kind: WireClientSurfaceEventKind::Commit(weld_client::WireClientSurfaceCommit {
                revision: ClientCommitRevision::new(revision),
                alpha_mode: SurfaceAlphaMode::Discarded,
                mapped: true,
                root: None,
                window_geometry: None,
                overlays: Vec::new(),
                inputs: Vec::new(),
                buffers: vec![weld_client::WireSurfaceBufferUpdate {
                    layer: SurfaceLayerId::new(1),
                    change: WireSurfaceBufferChange::Replaced {
                        metadata: ClientBufferMetadata::new(Extent::new(1, 1), true),
                        buffer: EncodedBuffer { frame },
                    },
                }],
            }),
        }
    }

    fn encoded_media(
        session: HoistSessionId,
        frame: MediaFrameId,
    ) -> MediaEnvelope<EncodedAccessUnit> {
        MediaEnvelope {
            session,
            access_unit: EncodedAccessUnit {
                frame,
                codec: VideoCodec::H264,
                kind: EncodedFrameKind::Keyframe,
                timestamp_micros: 1,
                payload: vec![1],
            },
        }
    }

    fn complete(state: &Rc<RefCell<FakeEncoderState>>, token: u64, frame: MediaFrameId, byte: u8) {
        state.borrow_mut().completions.push(EncodeCompletion {
            token,
            result: Ok(EncodedAccessUnit {
                frame,
                codec: VideoCodec::H264,
                kind: EncodedFrameKind::Keyframe,
                timestamp_micros: 7,
                payload: vec![byte],
            }),
        });
    }

    fn output_kinds(output: &VecDeque<SourceTransportPacket>) -> Vec<&'static str> {
        output
            .iter()
            .map(|packet| match packet {
                SourceTransportPacket::Control(_) => "control",
                SourceTransportPacket::Media(_) => "media",
            })
            .collect()
    }

    #[test]
    fn destroyed_relay_surfaces_do_not_leak_streams() {
        let (port, transport, encoder) = source_port();
        let source_id = ClientSourceId::new(1);
        let session = HoistSessionId::new(1);
        let metadata = ClientBufferMetadata::new(Extent::new(1, 1), true);
        let mut relay = SourceRelayAdapter::new(source_id, port);
        for local in 1..=160 {
            let surface = surface(source_id, 1, local);
            relay.apply_command(ClientAdapterCommandEnvelope::new(
                source_id,
                HoistEndpointCommand::Map {
                    session,
                    source: surface,
                },
            ));
            relay.observe_event(&one_buffer_commit(
                surface,
                1,
                1,
                shm_lease(source_id, local, 10, metadata),
            ));
            let (token, frame, _) = encoder
                .borrow()
                .submitted
                .last()
                .expect("submitted")
                .clone();
            complete(&encoder, token, frame, 10);
            relay.drain_events(&mut ClientEventQueue::default());
            relay.observe_event(&ClientSurfaceEvent {
                surface,
                kind: ClientSurfaceEventKind::Destroyed,
            });
            assert!(transport.borrow().sent.iter().any(|packet| matches!(packet,
                SourceTransportPacket::Control(SourceEnvelope { message: SourceMessage::Surface(event), .. })
                    if event.surface == surface && matches!(event.kind, WireClientSurfaceEventKind::Destroyed)
            )), "destruction must remain deliverable");
            relay.drain_events(&mut ClientEventQueue::default());
            assert!(!transport.borrow().disconnected);
            assert_eq!(encoder.borrow().retirements.len(), local as usize);
        }
    }

    #[test]
    fn destruction_during_encode_defers_retirement_and_lease_release_until_completion() {
        let (mut source, encoder) = source();
        let source_id = ClientSourceId::new(1);
        let first = surface(source_id, 1, 1);
        let second = surface(source_id, 1, 2);
        let session = HoistSessionId::new(1);
        let metadata = ClientBufferMetadata::new(Extent::new(1, 1), true);
        source
            .enqueue(
                session,
                one_buffer_commit(first, 1, 1, shm_lease(source_id, 1, 10, metadata)),
            )
            .expect("submit");
        let released = Rc::new(Cell::new(0));
        let on_release = released.clone();
        // Exercise the retained GPU lease policy without submitting GPU work.
        source
            .in_flight
            .as_mut()
            .expect("active")
            .active
            .retained_dmabuf_lease = Some(
            ClientBufferLease::new(
                ClientBufferId::new(source_id, 2),
                ClientBufferUseId::new(source_id, 2),
                metadata,
                Rc::new(()),
                move |_| on_release.set(on_release.get() + 1),
            )
            .expect("lease"),
        );
        source
            .enqueue(
                session,
                one_buffer_commit(second, 1, 1, shm_lease(source_id, 3, 20, metadata)),
            )
            .expect("queue unrelated surface");
        source
            .enqueue(
                session,
                ClientSurfaceEvent {
                    surface: first,
                    kind: ClientSurfaceEventKind::Destroyed,
                },
            )
            .expect("destroy");
        assert_eq!(released.get(), 0);
        assert!(encoder.borrow().retirements.is_empty());
        let (token, frame, _) = encoder.borrow().submitted[0].clone();
        encoder.borrow_mut().completions.push(EncodeCompletion {
            token,
            result: Err(anyhow::anyhow!("obsolete encode failed")),
        });
        source
            .drain()
            .expect("cancelled error must not fail the session");
        assert_eq!(released.get(), 1);
        assert_eq!(
            encoder.borrow().retirements,
            vec![(frame.stream, frame.generation)]
        );
        assert_eq!(output_kinds(&source.output), vec!["control"]);
        source.take_output();
        source.schedule().expect("destruction output delivered");
        assert_eq!(encoder.borrow().submitted.len(), 2);
    }

    #[test]
    fn cancelled_decodes_ignore_results_and_retire_only_after_completion() {
        for result in [
            Err(anyhow::anyhow!("obsolete decode failed")),
            Ok(Vec::new()),
        ] {
            let failed = result.is_err();
            let (mut port, _, decoder) = destination_port();
            let state = port.state.as_mut().expect("state");
            let source_id = ClientSourceId::new(1);
            let first = surface(source_id, 1, 1);
            let second = surface(source_id, 1, 2);
            let session = HoistSessionId::new(1);
            let frame = MediaFrameId::new(MediaStreamId::new(1), StreamGeneration::new(1), 0);
            let next = MediaFrameId::new(MediaStreamId::new(2), StreamGeneration::new(1), 0);
            let mut output = Vec::new();
            state
                .enqueue_media(encoded_media(session, frame))
                .expect("media");
            state
                .enqueue(session, encoded_commit(first, 1, frame), &mut output)
                .expect("commit");
            state
                .enqueue_media(encoded_media(session, next))
                .expect("next media");
            state
                .enqueue(session, encoded_commit(second, 1, next), &mut output)
                .expect("next commit");
            state.cancel_surface(first).expect("cancel");
            assert!(decoder.borrow().retirements.is_empty());
            let token = decoder.borrow().tokens[0];
            decoder
                .borrow_mut()
                .completions
                .push(DecodeCompletion { token, result });
            state.drain(&mut output).expect("discard obsolete result");
            assert!(output.is_empty());
            assert!(state.cancelled_frames.is_empty());
            assert_eq!(
                decoder.borrow().retirements,
                vec![(frame.stream, frame.generation)]
            );
            assert_eq!(decoder.borrow().submitted, vec![frame, next]);
            let report = state
                .observations
                .take_report(Instant::now(), DestinationGauges::default(), true)
                .expect("observations");
            assert_eq!(report.counters.media_wait.samples, 2);
            assert_eq!(report.counters.decodes_cancelled, 1);
            assert_eq!(report.counters.codec_failures, u64::from(failed));
            assert_eq!(report.counters.decode_wall.samples, 0);
            assert_eq!(report.counters.commit_wall.samples, 0);
            assert_eq!(report.counters.commits_cancelled, 1);
        }
    }

    #[test]
    fn cancelled_decoded_layers_do_not_leave_permanent_late_media_markers() {
        let (mut port, _, decoder) = destination_port();
        let state = port.state.as_mut().expect("state");
        let source_id = ClientSourceId::new(1);
        let session = HoistSessionId::new(1);
        for local in 1..=160 {
            let surface = surface(source_id, 1, local);
            let frame =
                MediaFrameId::new(MediaStreamId::new(local * 2), StreamGeneration::new(1), 0);
            let missing = MediaFrameId::new(
                MediaStreamId::new(local * 2 + 1),
                StreamGeneration::new(1),
                0,
            );
            let mut event = encoded_commit(surface, 1, frame);
            let WireClientSurfaceEventKind::Commit(commit) = &mut event.kind else {
                unreachable!()
            };
            let mut other = commit.buffers[0].clone();
            other.layer = SurfaceLayerId::new(2);
            let WireSurfaceBufferChange::Replaced { buffer, .. } = &mut other.change else {
                unreachable!()
            };
            buffer.frame = missing;
            commit.buffers.push(other);
            state
                .enqueue_media(encoded_media(session, frame))
                .expect("first layer media");
            state
                .enqueue(session, event, &mut Vec::new())
                .expect("commit");
            let token = *decoder.borrow().tokens.last().expect("submitted decode");
            // Held for the second layer, then cancelled. Never imported.
            decoder.borrow_mut().completions.push(DecodeCompletion {
                token,
                result: Ok(vec![DecodedFrame {
                    frame,
                    dmabuf: weld_core::dmabuf::ExternalDmabuf {
                        extent: Extent::new(1, 1),
                        format: 0,
                        modifier: 0,
                        flags: 0,
                        planes: Vec::new(),
                    },
                }]),
            });
            state.drain(&mut Vec::new()).expect("first decoded layer");
            state.cancel_surface(surface).expect("cancel");
            assert_eq!(state.cancelled_frames.len(), 1);
            assert!(
                state
                    .enqueue_media(encoded_media(HoistSessionId::new(2), missing))
                    .is_err()
            );
            state
                .enqueue_media(encoded_media(session, missing))
                .expect("expected late media");
            assert!(state.cancelled_frames.is_empty());
            assert!(state.decoded.is_empty());
            assert!(state.media_frames.is_empty());
        }
        let report = state
            .observations
            .take_report(Instant::now(), DestinationGauges::default(), true)
            .expect("observations");
        assert_eq!(report.counters.decode_wall.samples, 160);
        assert_eq!(report.counters.commit_wall.samples, 0);
        assert_eq!(report.counters.commits_cancelled, 160);
        assert_eq!(report.counters.late_cancelled_media, 160);
        assert_eq!(report.counters.media_received, 160);
    }

    #[test]
    fn protocol_errors_preserve_the_underlying_failure() {
        let error = anyhow::anyhow!("decoder generation budget exhausted").context("decode failed");
        let error = protocol_error(error).to_string();
        assert!(error.contains("decode failed"));
        assert!(error.contains("decoder generation budget exhausted"));
    }

    fn finish_snapshot(
        source: &mut EncodedSourceState,
        encoder: &Rc<RefCell<FakeEncoderState>>,
        destination: &mut EncodedDestinationState,
    ) {
        while let Some(batch) = &source.in_flight {
            complete(encoder, batch.active.token, batch.active.frame, 10);
            source.drain().expect("encode snapshot");
        }
        for packet in source.take_output() {
            if let SourceTransportPacket::Control(SourceEnvelope {
                message: SourceMessage::Surface(event),
                ..
            }) = packet
            {
                destination
                    .update_stream_generations(&event)
                    .expect("receiver inventory");
                destination
                    .sweep_retirement()
                    .expect("retire unreferenced inventory");
            }
        }
    }

    #[test]
    fn complete_snapshots_retire_omitted_layers_before_admitting_replacements() {
        let (mut source, encoder) = source();
        encoder.borrow_mut().generation_limit = Some(3);
        let (mut port, _, decoder) = destination_port();
        let destination = port.state.as_mut().expect("destination");
        let source_id = ClientSourceId::new(1);
        let session = HoistSessionId::new(1);
        let other = surface(source_id, 1, 1);
        let surface = surface(source_id, 1, 2);
        let metadata = ClientBufferMetadata::new(Extent::new(1, 1), true);
        source
            .enqueue(
                session,
                one_buffer_commit(other, 1, 1, shm_lease(source_id, 1, 10, metadata)),
            )
            .expect("unrelated surface");
        finish_snapshot(&mut source, &encoder, destination);
        for revision in 1..=160 {
            let stable = if revision == 1 {
                SurfaceBufferChange::Replaced {
                    metadata,
                    buffer: shm_lease(source_id, 2, 10, metadata),
                }
            } else {
                SurfaceBufferChange::Retained { metadata }
            };
            source
                .enqueue(
                    session,
                    commit(
                        surface,
                        revision,
                        vec![
                            SurfaceBufferUpdate {
                                layer: SurfaceLayerId::new(1),
                                change: stable,
                            },
                            SurfaceBufferUpdate {
                                layer: SurfaceLayerId::new(revision + 1),
                                change: SurfaceBufferChange::Replaced {
                                    metadata,
                                    buffer: shm_lease(source_id, revision + 2, 20, metadata),
                                },
                            },
                        ],
                    ),
                )
                .expect("retire omitted layer before submitting its replacement");
            finish_snapshot(&mut source, &encoder, destination);
            assert_eq!(source.streams.len(), 3);
            assert_eq!(destination.streams.len(), 3);
            assert_eq!(encoder.borrow().retirements.len(), revision as usize - 1);
            assert_eq!(decoder.borrow().retirements.len(), revision as usize - 1);
        }
        let other_generation = destination.streams[&(other, SurfaceLayerId::new(1))];
        // Explicit removal and omission have the same membership semantics.
        source
            .enqueue(
                session,
                commit(
                    surface,
                    161,
                    vec![
                        SurfaceBufferUpdate {
                            layer: SurfaceLayerId::new(1),
                            change: SurfaceBufferChange::Retained { metadata },
                        },
                        SurfaceBufferUpdate {
                            layer: SurfaceLayerId::new(161),
                            change: SurfaceBufferChange::Removed,
                        },
                    ],
                ),
            )
            .expect("explicit removal");
        finish_snapshot(&mut source, &encoder, destination);
        assert_eq!(source.streams.len(), 2);
        assert_eq!(destination.streams.len(), 2);
        let mut unmap = commit(surface, 162, Vec::new());
        if let ClientSurfaceEventKind::Commit(commit) = &mut unmap.kind {
            commit.mapped = false;
        }
        source
            .enqueue(session, unmap)
            .expect("empty unmap snapshot");
        finish_snapshot(&mut source, &encoder, destination);
        assert_eq!(source.streams.len(), 1);
        assert_eq!(destination.streams.len(), 1);
        assert_eq!(
            destination.streams[&(other, SurfaceLayerId::new(1))],
            other_generation
        );
        assert_eq!(encoder.borrow().generations.len(), 1);
    }

    #[test]
    fn cancellation_drops_arrived_but_unsubmitted_media_without_a_tombstone() {
        let (mut port, _, decoder) = destination_port();
        let state = port.state.as_mut().expect("state");
        let source_id = ClientSourceId::new(1);
        let session = HoistSessionId::new(1);
        let active = surface(source_id, 1, 1);
        let pending = surface(source_id, 1, 2);
        let first = MediaFrameId::new(MediaStreamId::new(1), StreamGeneration::new(1), 0);
        let second = MediaFrameId::new(MediaStreamId::new(2), StreamGeneration::new(1), 0);
        state
            .enqueue_media(encoded_media(session, first))
            .expect("active media");
        state
            .enqueue(session, encoded_commit(active, 1, first), &mut Vec::new())
            .expect("active commit");
        state
            .enqueue_media(encoded_media(session, second))
            .expect("pending media");
        state
            .enqueue(session, encoded_commit(pending, 1, second), &mut Vec::new())
            .expect("pending commit");
        state.cancel_surface(pending).expect("cancel pending");
        assert!(state.media_frames.is_empty());
        assert!(state.cancelled_frames.is_empty());
        assert_eq!(decoder.borrow().submitted, vec![first]);
        assert_eq!(
            decoder.borrow().retirements,
            vec![(second.stream, second.generation)]
        );
    }

    #[test]
    fn cancelled_codec_completions_still_require_matching_tokens() {
        let source_id = ClientSourceId::new(1);
        let surface = surface(source_id, 1, 1);
        let session = HoistSessionId::new(1);
        let (mut source, encoder) = source();
        let metadata = ClientBufferMetadata::new(Extent::new(1, 1), true);
        source
            .enqueue(
                session,
                one_buffer_commit(surface, 1, 1, shm_lease(source_id, 1, 10, metadata)),
            )
            .expect("encode");
        source.cancel_surface(surface).expect("cancel encode");
        let token = encoder.borrow().submitted[0].0;
        encoder.borrow_mut().completions.push(EncodeCompletion {
            token: token + 1,
            result: Err(anyhow::anyhow!("obsolete encode failed")),
        });
        assert!(
            source
                .drain()
                .expect_err("wrong encode token")
                .to_string()
                .contains("unexpected source event")
        );

        let (mut port, _, decoder) = destination_port();
        let state = port.state.as_mut().expect("state");
        let frame = MediaFrameId::new(MediaStreamId::new(1), StreamGeneration::new(1), 0);
        state
            .enqueue_media(encoded_media(session, frame))
            .expect("media");
        state
            .enqueue(session, encoded_commit(surface, 1, frame), &mut Vec::new())
            .expect("decode");
        state.cancel_surface(surface).expect("cancel decode");
        let token = decoder.borrow().tokens[0];
        decoder.borrow_mut().completions.push(DecodeCompletion {
            token: token + 1,
            result: Err(anyhow::anyhow!("obsolete decode failed")),
        });
        assert!(
            state
                .drain(&mut Vec::new())
                .expect_err("wrong decode token")
                .to_string()
                .contains("unexpected token")
        );
        let report = state
            .observations
            .take_report(Instant::now(), DestinationGauges::default(), true)
            .expect("observations");
        assert_eq!(report.counters.media_wait.samples, 1);
        assert_eq!(report.counters.decode_wall.samples, 0);
        assert_eq!(report.counters.commit_wall.samples, 0);
        assert_eq!(report.counters.decodes_cancelled, 0);
        assert_eq!(report.counters.codec_failures, 0);
    }

    #[test]
    fn structural_events_behind_a_frame_keep_order_without_another_wake() {
        let (mut port, transport, encoder) = source_port();
        let source_id = ClientSourceId::new(1);
        let surface = surface(source_id, 2, 3);
        let session = HoistSessionId::new(4);
        let metadata = ClientBufferMetadata::new(Extent::new(1, 1), true);
        port.submit(SourcePortCommand::Surface {
            session,
            event: one_buffer_commit(surface, 5, 1, shm_lease(source_id, 5, 10, metadata)),
        })
        .expect("submit frame");
        for interaction in [
            ToplevelInteractionRequestKind::Move,
            ToplevelInteractionRequestKind::End,
        ] {
            port.submit(SourcePortCommand::Surface {
                session,
                event: ClientSurfaceEvent {
                    surface,
                    kind: ClientSurfaceEventKind::Interaction(interaction),
                },
            })
            .expect("queue structural event");
        }

        let (token, frame, _) = encoder.borrow().submitted[0].clone();
        complete(&encoder, token, frame, 1);
        assert!(port.poll().expect("complete frame").is_empty());

        let sent = &transport.borrow().sent;
        assert_eq!(sent.len(), 4);
        assert!(matches!(sent[0], SourceTransportPacket::Media(_)));
        assert!(matches!(
            &sent[1],
            SourceTransportPacket::Control(SourceEnvelope {
                message: SourceMessage::Surface(WireClientSurfaceEvent {
                    kind: WireClientSurfaceEventKind::Commit(_),
                    ..
                }),
                ..
            })
        ));
        assert!(matches!(
            &sent[2],
            SourceTransportPacket::Control(SourceEnvelope {
                message: SourceMessage::Surface(WireClientSurfaceEvent {
                    kind: WireClientSurfaceEventKind::Interaction(
                        ToplevelInteractionRequestKind::Move
                    ),
                    ..
                }),
                ..
            })
        ));
        assert!(matches!(
            &sent[3],
            SourceTransportPacket::Control(SourceEnvelope {
                message: SourceMessage::Surface(WireClientSurfaceEvent {
                    kind: WireClientSurfaceEventKind::Interaction(
                        ToplevelInteractionRequestKind::End
                    ),
                    ..
                }),
                ..
            })
        ));
    }

    #[test]
    fn opaque_mode_survives_a_retained_commit_through_both_ports() {
        let (mut source, _) = source();
        let (mut destination, transport, _) = destination_port();
        let surface = surface(ClientSourceId::new(2), 3, 4);
        let session = HoistSessionId::new(1);
        source
            .enqueue(
                session,
                commit(
                    surface,
                    7,
                    vec![SurfaceBufferUpdate {
                        layer: SurfaceLayerId::new(1),
                        change: SurfaceBufferChange::Retained {
                            metadata: ClientBufferMetadata::new(Extent::new(8, 8), false),
                        },
                    }],
                ),
            )
            .expect("retained source commit");
        transport
            .borrow_mut()
            .incoming
            .extend(source.output.drain(..));
        let records = destination.poll().expect("opaque retained commit");
        let [
            DestinationPortRecord {
                event:
                    DestinationPortEvent::Surface(ClientSurfaceEvent {
                        kind: ClientSurfaceEventKind::Commit(commit),
                        ..
                    }),
                ..
            },
        ] = records.as_slice()
        else {
            panic!("expected one retained destination commit");
        };
        assert_eq!(commit.alpha_mode, SurfaceAlphaMode::Discarded);
        assert!(
            commit.buffers[0]
                .change
                .metadata()
                .expect("retained metadata")
                .opaque
        );
    }

    #[test]
    fn opaque_destination_rejects_an_undeclared_alpha_loss() {
        let (mut destination, transport, _) = destination_port();
        let surface = surface(ClientSourceId::new(2), 3, 4);
        let mut event = encoded_commit(
            surface,
            1,
            MediaFrameId::new(MediaStreamId::new(1), StreamGeneration::new(1), 0),
        );
        if let WireClientSurfaceEventKind::Commit(commit) = &mut event.kind {
            commit.alpha_mode = SurfaceAlphaMode::Preserved;
        }
        transport
            .borrow_mut()
            .incoming
            .push_back(SourceTransportPacket::Control(SourceEnvelope {
                session: HoistSessionId::new(1),
                message: SourceMessage::Surface(event),
            }));
        let error = destination.poll().err().expect("invalid alpha mode");
        assert!(error.to_string().contains("declare discarded alpha"));
        // The owning relay handles a port error by disconnecting the session.
        destination.disconnect();
        assert!(transport.borrow().disconnected);
    }

    #[test]
    fn destination_accepts_control_and_media_in_either_order() {
        for media_first in [false, true] {
            let (mut port, transport, decoder) = destination_port();
            let session = HoistSessionId::new(1);
            let surface = surface(ClientSourceId::new(2), 3, 4);
            let frame = MediaFrameId::new(MediaStreamId::new(5), StreamGeneration::new(6), 7);
            let control = SourceTransportPacket::Control(SourceEnvelope {
                session,
                message: SourceMessage::Surface(encoded_commit(surface, 8, frame)),
            });
            let media = SourceTransportPacket::Media(encoded_media(session, frame));
            let packets = if media_first {
                [media, control]
            } else {
                [control, media]
            };
            transport.borrow_mut().incoming.extend(packets);

            assert!(port.poll().expect("ordered packets").is_empty());
            assert_eq!(decoder.borrow().submitted, vec![frame]);
            let state = port.state.as_mut().expect("state");
            let report = state
                .observations
                .take_report(Instant::now(), DestinationGauges::default(), true)
                .expect("observations");
            assert_eq!(report.counters.commits_received, 1);
            assert_eq!(report.counters.media_received, 1);
            assert_eq!(report.counters.payload_bytes_received, 1);
            assert_eq!(report.counters.media_wait.samples, 1);
            assert_eq!(report.counters.commit_wall.samples, 0);
        }
    }

    #[test]
    fn destination_cancels_without_sending_commit_acknowledgements() {
        let (mut port, transport, _decoder) = destination_port();
        let session = HoistSessionId::new(1);
        let surface = surface(ClientSourceId::new(2), 3, 4);
        let frame = MediaFrameId::new(MediaStreamId::new(5), StreamGeneration::new(6), 7);
        transport.borrow_mut().incoming.extend([
            SourceTransportPacket::Control(SourceEnvelope {
                session,
                message: SourceMessage::Surface(encoded_commit(surface, 8, frame)),
            }),
            SourceTransportPacket::Control(SourceEnvelope {
                session,
                message: SourceMessage::Withdraw { surface },
            }),
        ]);

        let records = port.poll().expect("withdraw pending commit");
        assert!(matches!(
            records.as_slice(),
            [DestinationPortRecord {
                event: DestinationPortEvent::WithdrawSurface(observed),
                ..
            }] if *observed == surface
        ));
        assert!(transport.borrow().sent.is_empty());
        let state = port.state.as_mut().expect("state");
        let report = state
            .observations
            .take_report(Instant::now(), DestinationGauges::default(), true)
            .expect("observations");
        assert_eq!(report.counters.commits_cancelled, 1);
        assert_eq!(report.counters.commit_wall.samples, 0);
    }

    #[test]
    fn receiver_gauges_measure_existing_queue_entries_without_clock_sleeps() {
        let (mut port, _, _) = destination_port();
        let state = port.state.as_mut().expect("state");
        let session = HoistSessionId::new(1);
        let surface = surface(ClientSourceId::new(2), 3, 4);
        let frame = MediaFrameId::new(MediaStreamId::new(5), StreamGeneration::new(6), 7);
        let mut output = Vec::new();
        state
            .enqueue(session, encoded_commit(surface, 8, frame), &mut output)
            .expect("control");
        state
            .enqueue_media(encoded_media(session, frame))
            .expect("media");
        let start = Instant::now();
        state
            .queues
            .get_mut(&surface)
            .expect("queue")
            .front_mut()
            .expect("event")
            .received_at = start;
        state
            .media_frames
            .get_mut(&frame)
            .expect("media")
            .received_at = start + Duration::from_millis(10);
        let now = start + Duration::from_millis(30);
        let gauges = state.observation_gauges(now);
        assert_eq!(gauges.pending_events, 1);
        assert_eq!(gauges.pending_media_frames, 1);
        assert_eq!(gauges.pending_media_bytes, 1);
        assert_eq!(gauges.oldest_control_age, Duration::from_millis(30));
        assert_eq!(gauges.oldest_media_age, Duration::from_millis(20));
        assert!(!gauges.decode_in_flight);
        state.advance(&mut output).expect("submit");
        state
            .decode_in_flight
            .as_mut()
            .expect("decode")
            .submitted_at = start;
        let gauges = state.observation_gauges(now);
        assert_eq!(gauges.pending_events, 1);
        assert_eq!(gauges.pending_media_frames, 0);
        assert_eq!(gauges.pending_media_bytes, 0);
        assert!(gauges.decode_in_flight);
        assert_eq!(gauges.active_decode_age, Duration::from_millis(30));
    }

    #[test]
    fn receiver_does_not_count_duplicate_media_or_failed_decodes_as_success() {
        let (mut port, _, decoder) = destination_port();
        let state = port.state.as_mut().expect("state");
        let session = HoistSessionId::new(1);
        let surface = surface(ClientSourceId::new(2), 3, 4);
        let frame = MediaFrameId::new(MediaStreamId::new(5), StreamGeneration::new(6), 7);
        state
            .enqueue_media(encoded_media(session, frame))
            .expect("media");
        state
            .enqueue(session, encoded_commit(surface, 8, frame), &mut Vec::new())
            .expect("control");
        let token = decoder.borrow().tokens[0];
        decoder.borrow_mut().completions.push(DecodeCompletion {
            token,
            result: Err(anyhow::anyhow!("fake decode failure")),
        });
        assert!(state.drain(&mut Vec::new()).is_err());
        let report = state
            .observations
            .take_report(Instant::now(), DestinationGauges::default(), true)
            .expect("observations");
        assert_eq!(report.counters.codec_failures, 1);
        assert_eq!(report.counters.media_wait.samples, 1);
        assert_eq!(report.counters.decode_wall.samples, 0);
        assert_eq!(report.counters.commit_wall.samples, 0);
        assert_eq!(report.counters.commits_cancelled, 0);

        // A separate destination fails before control/codec work on duplicate media.
        let (mut port, _, _) = destination_port();
        let state = port.state.as_mut().expect("state");
        state
            .enqueue_media(encoded_media(session, frame))
            .expect("media");
        assert!(state.enqueue_media(encoded_media(session, frame)).is_err());
        let report = state
            .observations
            .take_report(Instant::now(), DestinationGauges::default(), true)
            .expect("observations");
        assert_eq!(report.counters.media_received, 1);
        assert_eq!(report.counters.payload_bytes_received, 1);
        assert_eq!(report.counters.media_wait.samples, 0);
    }

    #[test]
    fn ports_reject_native_buffer_lifetimes_and_disconnect_the_transport() {
        let (mut source, source_transport, _encoder) = source_port();
        let session = HoistSessionId::new(1);
        assert!(
            source
                .accept_destination(&DestinationEnvelope {
                    session,
                    message: DestinationMessage::BufferReleased {
                        use_id: ClientBufferUseId::new(ClientSourceId::new(2), 3),
                    },
                })
                .is_err()
        );
        source.disconnect();
        assert!(source_transport.borrow().disconnected);

        let (mut destination, destination_transport, _decoder) = destination_port();
        destination_transport
            .borrow_mut()
            .incoming
            .push_back(SourceTransportPacket::Control(SourceEnvelope {
                session,
                message: SourceMessage::BufferRetired {
                    buffer: ClientBufferId::new(ClientSourceId::new(2), 3),
                },
            }));
        assert!(destination.poll().is_err());
        destination.disconnect();
        assert!(destination_transport.borrow().disconnected);
    }

    #[test]
    fn exact_visible_extent_rotates_and_retires_stream_generations() {
        let (mut source, fake) = source();
        let source_id = ClientSourceId::new(1);
        let surface = surface(source_id, 2, 3);
        let layer = SurfaceLayerId::new(4);
        let metadata = |width| ClientBufferMetadata::new(Extent::new(width, 480), true);

        let (first, _) = source
            .allocate_frame(surface, layer, metadata(484))
            .expect("first frame");
        let (retained, _) = source
            .allocate_frame(surface, layer, metadata(484))
            .expect("retained extent");
        let (odd, _) = source
            .allocate_frame(surface, layer, metadata(485))
            .expect("odd extent");
        let (even, _) = source
            .allocate_frame(surface, layer, metadata(486))
            .expect("even extent");

        assert_eq!((first.generation.raw(), first.sequence), (1, 0));
        assert_eq!((retained.generation.raw(), retained.sequence), (1, 1));
        assert_eq!((odd.generation.raw(), odd.sequence), (2, 0));
        assert_eq!((even.generation.raw(), even.sequence), (3, 0));
        assert_eq!(
            fake.borrow().retirements,
            vec![
                (MediaStreamId::new(1), StreamGeneration::new(1)),
                (MediaStreamId::new(1), StreamGeneration::new(2)),
            ]
        );
    }

    #[test]
    fn resize_coalesces_to_the_latest_frame() {
        let (mut source, fake) = source();
        let source_id = ClientSourceId::new(1);
        let surface = surface(source_id, 2, 3);
        let session = HoistSessionId::new(4);
        let metadata = ClientBufferMetadata::new(Extent::new(1, 1), true);

        source.set_resizing(surface, true).expect("begin resize");
        source
            .enqueue(
                session,
                one_buffer_commit(surface, 1, 1, shm_lease(source_id, 5, 10, metadata)),
            )
            .expect("first resize commit");
        source
            .enqueue(
                session,
                one_buffer_commit(surface, 2, 1, shm_lease(source_id, 6, 20, metadata)),
            )
            .expect("second resize commit");
        assert!(fake.borrow().submitted.is_empty());

        source.set_resizing(surface, false).expect("end resize");
        let (token, frame, pixels) = fake.borrow().submitted[0].clone();
        assert_eq!(pixels, vec![20, 20, 20, 255]);
        complete(&fake, token, frame, 1);
        source.drain().expect("encoder completion");

        assert_eq!(output_kinds(&source.output), vec!["media", "control"]);
    }

    #[test]
    fn encoded_queue_preserves_unmap_and_remap_boundaries() {
        let (mut source, _) = source();
        let surface = surface(ClientSourceId::new(1), 2, 3);
        let session = HoistSessionId::new(4);
        for (revision, mapped) in [(1, true), (2, false), (3, true), (4, true)] {
            let mut event = commit(surface, revision, Vec::new());
            if let ClientSurfaceEventKind::Commit(commit) = &mut event.kind {
                commit.mapped = mapped;
            }
            source.queue_event(session, event).expect("bounded queue");
        }
        let queued = &source.pending[&surface];
        let states = queued
            .iter()
            .map(|(_, event)| match &event.kind {
                ClientSurfaceEventKind::Commit(commit) => (commit.revision.raw(), commit.mapped),
                _ => panic!("only commits were queued"),
            })
            .collect::<Vec<_>>();
        assert_eq!(states, [(1, true), (2, false), (4, true)]);
        assert!(queued.len() < MAX_PENDING_SOURCE_EVENTS);
    }

    #[test]
    fn multi_layer_commit_waits_for_every_encoded_frame() {
        let (mut source, fake) = source();
        let source_id = ClientSourceId::new(1);
        let surface = surface(source_id, 2, 3);
        let session = HoistSessionId::new(4);
        let metadata = ClientBufferMetadata::new(Extent::new(1, 1), true);
        let buffers = [(1, 5, 10), (2, 6, 20)]
            .into_iter()
            .map(|(layer, local, pixel)| SurfaceBufferUpdate {
                layer: SurfaceLayerId::new(layer),
                change: SurfaceBufferChange::Replaced {
                    metadata,
                    buffer: shm_lease(source_id, local, pixel, metadata),
                },
            })
            .collect();
        source
            .enqueue(session, commit(surface, 7, buffers))
            .expect("multi-layer commit");

        let (first_token, first_frame, _) = fake.borrow().submitted[0].clone();
        complete(&fake, first_token, first_frame, 1);
        source.drain().expect("first completion");
        assert!(source.output.is_empty());

        let (second_token, second_frame, _) = fake.borrow().submitted[1].clone();
        complete(&fake, second_token, second_frame, 2);
        source.drain().expect("second completion");
        assert_eq!(
            output_kinds(&source.output),
            vec!["media", "media", "control"]
        );
        let report = source
            .observations
            .take_report(Instant::now(), SourceGauges::default(), true)
            .expect("observations");
        assert_eq!(report.counters.batches_completed, 1);
        assert_eq!(report.counters.layer_frames_completed, 2);
        assert_eq!(report.counters.encoded_payload_bytes, 2);
        assert_eq!(report.counters.batch_wall.samples, 1);
    }

    #[test]
    fn cancellation_discards_a_partial_batch() {
        let (mut source, fake) = source();
        let source_id = ClientSourceId::new(1);
        let surface = surface(source_id, 2, 3);
        let session = HoistSessionId::new(4);
        let metadata = ClientBufferMetadata::new(Extent::new(1, 1), true);
        let buffers = [1, 2]
            .into_iter()
            .map(|local| SurfaceBufferUpdate {
                layer: SurfaceLayerId::new(local),
                change: SurfaceBufferChange::Replaced {
                    metadata,
                    buffer: shm_lease(source_id, local, 10, metadata),
                },
            })
            .collect();
        source
            .enqueue(session, commit(surface, 7, buffers))
            .expect("multi-layer commit");
        let (first_token, first_frame, _) = fake.borrow().submitted[0].clone();
        complete(&fake, first_token, first_frame, 1);
        source.drain().expect("first completion");
        let (second_token, second_frame, _) = fake.borrow().submitted[1].clone();
        source.cancel_surface(surface).expect("cancel surface");
        complete(&fake, second_token, second_frame, 2);
        source.drain().expect("cancelled completion");

        assert!(source.output.is_empty());
    }

    #[test]
    fn multiple_same_surface_batches_progress_without_any_reverse_messages() {
        let (mut port, transport, encoder) = source_port();
        let source_id = ClientSourceId::new(1);
        let surface = surface(source_id, 2, 3);
        let session = HoistSessionId::new(4);
        let metadata = ClientBufferMetadata::new(Extent::new(1, 1), true);
        for revision in 1..=20 {
            port.submit(SourcePortCommand::Surface {
                session,
                event: one_buffer_commit(
                    surface,
                    revision,
                    1,
                    shm_lease(source_id, revision, 10, metadata),
                ),
            })
            .expect("submit without reverse traffic");
            let (token, frame, _) = encoder.borrow().submitted.last().expect("encode").clone();
            complete(&encoder, token, frame, 1);
            assert!(port.poll().expect("complete").is_empty());
        }
        assert_eq!(encoder.borrow().submitted.len(), 20);
        assert_eq!(transport.borrow().sent.len(), 40);
        assert!(transport.borrow().incoming.is_empty());
    }

    #[test]
    fn blocked_media_survives_withdrawal_and_does_not_block_cursor_control() {
        let (mut port, transport, encoder) = source_port();
        let source_id = ClientSourceId::new(1);
        let surface = surface(source_id, 2, 3);
        let session = HoistSessionId::new(4);
        let metadata = ClientBufferMetadata::new(Extent::new(1, 1), true);
        port.submit(SourcePortCommand::Surface {
            session,
            event: one_buffer_commit(surface, 1, 1, shm_lease(source_id, 1, 10, metadata)),
        })
        .expect("start encode");
        transport.borrow_mut().block_media = true;
        let (token, frame, _) = encoder.borrow().submitted[0].clone();
        complete(&encoder, token, frame, 37);
        port.poll().expect("retain completed media");
        port.submit(SourcePortCommand::Cursor {
            session,
            update: weld_client::ClientCursorUpdate {
                surface,
                cursor: weld_client::ClientCursor::Hidden,
            },
            sequence: 1,
        })
        .expect("cursor while media blocked");
        port.submit(SourcePortCommand::WithdrawSurface { session, surface })
            .expect("withdraw");
        assert_eq!(transport.borrow().sent.len(), 3);
        assert!(matches!(
            &transport.borrow().sent[1],
            SourceTransportPacket::Control(SourceEnvelope {
                message: SourceMessage::Cursor { .. },
                ..
            })
        ));
        assert_eq!(port.output.pending_records(), 1);
        transport.borrow_mut().block_media = false;
        port.poll()
            .expect("writable wake without a client commit or ACK");
        assert!(port.output.is_empty());
        assert!(
            matches!(&transport.borrow().sent[3], SourceTransportPacket::Media(packet)
            if packet.access_unit.frame == frame && packet.access_unit.payload == [37])
        );
        assert!(!transport.borrow().disconnected);
    }

    #[test]
    fn retained_control_keeps_commit_before_withdrawal() {
        let (mut port, transport, encoder) = source_port();
        let source_id = ClientSourceId::new(1);
        let surface = surface(source_id, 2, 3);
        let session = HoistSessionId::new(4);
        let metadata = ClientBufferMetadata::new(Extent::new(1, 1), true);
        port.submit(SourcePortCommand::Surface {
            session,
            event: one_buffer_commit(surface, 1, 1, shm_lease(source_id, 1, 10, metadata)),
        })
        .expect("encode");
        transport.borrow_mut().block_control = true;
        let (token, frame, _) = encoder.borrow().submitted[0].clone();
        complete(&encoder, token, frame, 1);
        port.poll().expect("control busy");
        port.submit(SourcePortCommand::WithdrawSurface { session, surface })
            .expect("retain withdraw");
        assert_eq!(port.output.pending_records(), 2);
        transport.borrow_mut().block_control = false;
        port.poll().expect("control writable");
        assert!(matches!(
            &transport.borrow().sent[1],
            SourceTransportPacket::Control(SourceEnvelope {
                message: SourceMessage::Surface(_),
                ..
            })
        ));
        assert!(matches!(
            &transport.borrow().sent[2],
            SourceTransportPacket::Control(SourceEnvelope {
                message: SourceMessage::Withdraw { .. },
                ..
            })
        ));
        assert!(port.output.is_empty());
    }

    #[test]
    fn new_commit_on_capacity_recovery_coalesces_without_overtaking_pending_content() {
        let (mut port, transport, encoder) = source_port();
        let source_id = ClientSourceId::new(1);
        let surface = surface(source_id, 2, 3);
        let session = HoistSessionId::new(4);
        let metadata = ClientBufferMetadata::new(Extent::new(1, 1), true);
        transport.borrow_mut().block_media = true;
        for revision in 1..=2 {
            port.submit(SourcePortCommand::Surface {
                session,
                event: one_buffer_commit(
                    surface,
                    revision,
                    1,
                    shm_lease(source_id, revision, revision as u8, metadata),
                ),
            })
            .expect("queue while busy");
        }
        assert!(encoder.borrow().submitted.is_empty());
        transport.borrow_mut().block_media = false;
        port.submit(SourcePortCommand::Surface {
            session,
            event: one_buffer_commit(surface, 3, 1, shm_lease(source_id, 3, 3, metadata)),
        })
        .expect("new commit races writable notification");
        assert_eq!(encoder.borrow().submitted.len(), 1);
        assert_eq!(encoder.borrow().submitted[0].2, vec![3, 3, 3, 255]);
        let (token, frame, _) = encoder.borrow().submitted[0].clone();
        complete(&encoder, token, frame, 3);
        port.poll().expect("publish latest only");
        assert_eq!(encoder.borrow().submitted.len(), 1);
        assert!(port.state.as_ref().expect("state").pending.is_empty());
    }

    #[test]
    fn full_reference_budget_still_admits_late_tombstone_media() {
        let (mut port, _, _) = destination_port();
        let state = port.state.as_mut().expect("state");
        let session = HoistSessionId::new(1);
        for index in 0..MAX_REFERENCED_FRAMES as u64 {
            let frame = MediaFrameId::new(MediaStreamId::new(1), StreamGeneration::new(1), index);
            state.cancelled_frames.insert(frame, session);
        }
        assert_eq!(state.receive_budget().control_records, 0);
        assert_eq!(
            state.receive_budget().media_records,
            MAX_PENDING_MEDIA_FRAMES
        );
        for index in 0..MAX_REPLACEMENTS_PER_COMMIT as u64 {
            state
                .enqueue_media(encoded_media(
                    session,
                    MediaFrameId::new(MediaStreamId::new(1), StreamGeneration::new(1), index),
                ))
                .expect("late cancelled frame");
        }
        assert_eq!(state.receive_budget().control_records, 1);
        assert!(state.media_frames.is_empty());
    }

    #[test]
    fn queued_resize_generations_do_not_retire_an_active_or_queued_decode() {
        let (mut port, _, decoder) = destination_port();
        let state = port.state.as_mut().expect("state");
        let session = HoistSessionId::new(1);
        let surface = surface(ClientSourceId::new(1), 2, 3);
        let first = MediaFrameId::new(MediaStreamId::new(1), StreamGeneration::new(1), 0);
        let second = MediaFrameId::new(MediaStreamId::new(1), StreamGeneration::new(2), 0);
        state
            .enqueue_media(encoded_media(session, first))
            .expect("first media");
        state
            .enqueue(session, encoded_commit(surface, 1, first), &mut Vec::new())
            .expect("first decode");
        state
            .enqueue_media(encoded_media(session, second))
            .expect("second media");
        state
            .enqueue(session, encoded_commit(surface, 2, second), &mut Vec::new())
            .expect("new metadata while decoding");
        assert!(decoder.borrow().retirements.is_empty());
        assert_eq!(decoder.borrow().submitted, vec![first]);
        state
            .cancel_surface(surface)
            .expect("cancel both generations");
        assert_eq!(
            decoder.borrow().retirements,
            vec![(second.stream, second.generation)]
        );
        let token = decoder.borrow().tokens[0];
        decoder.borrow_mut().completions.push(DecodeCompletion {
            token,
            result: Ok(Vec::new()),
        });
        state.drain(&mut Vec::new()).expect("cancelled completion");
        assert_eq!(decoder.borrow().retirements.len(), 2);
        assert!(state.pending_retirement.is_empty());
        assert!(state.media_frames.is_empty());
    }

    #[test]
    fn source_observations_separate_coalescing_and_cancellation() {
        let (mut source, fake) = source();
        let source_id = ClientSourceId::new(1);
        let surface = surface(source_id, 2, 3);
        let session = HoistSessionId::new(4);
        let metadata = ClientBufferMetadata::new(Extent::new(1, 1), true);
        source
            .enqueue(
                session,
                one_buffer_commit(surface, 1, 1, shm_lease(source_id, 5, 10, metadata)),
            )
            .expect("first commit");
        let (token, frame, _) = fake.borrow().submitted[0].clone();
        complete(&fake, token, frame, 1);
        source.drain().expect("encode completion");
        for revision in [2, 3] {
            source
                .enqueue(
                    session,
                    one_buffer_commit(
                        surface,
                        revision,
                        1,
                        shm_lease(source_id, revision + 5, 20, metadata),
                    ),
                )
                .expect("locally blocked commit");
        }
        assert_eq!(fake.borrow().submitted.len(), 1);
        source.cancel_surface(surface).expect("withdraw surface");
        let report = source
            .observations
            .take_report(Instant::now(), SourceGauges::default(), true)
            .expect("observations");
        assert_eq!(report.counters.commits_received, 3);
        assert_eq!(report.counters.commits_coalesced, 1);
        assert_eq!(report.counters.batches_completed, 1);
        assert_eq!(report.counters.surfaces_cancelled, 1);
        assert!(source.pending.is_empty());
    }

    #[test]
    fn source_observations_count_failures_even_on_cancelled_encodes() {
        for cancelled in [false, true] {
            let (mut source, fake) = source();
            let source_id = ClientSourceId::new(1);
            let surface = surface(source_id, 2, 3);
            let session = HoistSessionId::new(4);
            let metadata = ClientBufferMetadata::new(Extent::new(1, 1), true);
            source
                .enqueue(
                    session,
                    one_buffer_commit(surface, 1, 1, shm_lease(source_id, 5, 10, metadata)),
                )
                .expect("commit");
            let token = fake.borrow().submitted[0].0;
            if cancelled {
                source.cancel_surface(surface).expect("cancel encode");
            }
            fake.borrow_mut().completions.push(EncodeCompletion {
                token,
                result: Err(anyhow::anyhow!("fake codec failure")),
            });
            assert_eq!(source.drain().is_ok(), cancelled);
            let report = source
                .observations
                .take_report(Instant::now(), SourceGauges::default(), true)
                .expect("failure observations");
            assert_eq!(report.counters.codec_failures, 1);
            assert_eq!(report.counters.batches_cancelled, u64::from(cancelled));
            assert_eq!(report.counters.batches_completed, 0);
            assert_eq!(report.counters.encoded_payload_bytes, 0);
        }
    }

    #[test]
    fn transport_collection_shares_source_report_gating_and_clock() {
        let (mut source, _) = source();
        let calls = Cell::new(0);
        source.report_observations(true, |_| {
            calls.set(calls.get() + 1);
            None
        });
        assert_eq!(
            calls.get(),
            0,
            "idle source does not request a transport report"
        );
        source
            .observations
            .record(SourceObservation::CommitReceived);
        let before = Instant::now();
        source.report_observations(true, |now| {
            assert!(now >= before && now <= Instant::now());
            calls.set(calls.get() + 1);
            Some(TransportSnapshot {
                observed_at: now,
                media: Default::default(),
                path: None,
            })
        });
        assert_eq!(calls.get(), 1);
    }
}
