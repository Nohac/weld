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
    DestinationEnvelope, DestinationMessage, EncodedBuffer, EncodedCommitOutcome, MediaEnvelope,
    SourceEnvelope, SourceMessage,
};
use weld_media::{EncodedAccessUnit, MediaFrameId, MediaStreamId, StreamGeneration, VideoCodec};

use crate::codec::{
    DecodeBackend, DecodeRequest, EncodeBackend, EncodeInput, EncodeRequest, SubmitError,
};
use crate::observations::{SourceGauges, SourceObservation, SourceObservations};

/// One packet sent from an encoded source to its destination.
pub enum SourceTransportPacket {
    Control(SourceEnvelope<EncodedBuffer>),
    Media(MediaEnvelope<EncodedAccessUnit>),
}

/// Nonblocking transport half used by the encoded source port.
pub trait EncodedSourceTransport {
    fn send(&self, packet: SourceTransportPacket) -> HoistPortResult<()>;
    fn drain(&self) -> HoistPortResult<Vec<DestinationEnvelope>>;
    fn disconnect(&self);
}

/// Nonblocking transport half used by the encoded destination port.
pub trait EncodedDestinationTransport {
    fn send(&self, packet: DestinationEnvelope) -> HoistPortResult<()>;
    fn drain(&self) -> HoistPortResult<Vec<SourceTransportPacket>>;
    fn disconnect(&self);
}

const MAX_PENDING_SOURCE_EVENTS: usize = 128;
const MAX_DESTINATION_EVENTS: usize = 128;
const MAX_REPLACEMENTS_PER_COMMIT: usize = 16;
const MAX_PENDING_MEDIA_FRAMES: usize = 128;
const MAX_CANCELLED_MEDIA_FRAMES: usize = 128;
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
}

struct PreparedEncode {
    request: EncodeRequest,
    retained_dmabuf_lease: Option<ClientBufferLease>,
}

struct ActiveEncode {
    token: u64,
    frame: MediaFrameId,
    retained_dmabuf_lease: Option<ClientBufferLease>,
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

struct OutstandingCredit {
    revision: ClientCommitRevision,
    sent_at: Instant,
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
    pending: HashMap<ClientSurfaceId, VecDeque<(HoistSessionId, ClientSurfaceEvent)>>,
    pending_order: VecDeque<ClientSurfaceId>,
    resizing: HashSet<ClientSurfaceId>,
    awaiting_credit: HashMap<ClientSurfaceId, OutstandingCredit>,
    streams: HashMap<(ClientSurfaceId, SurfaceLayerId), SourceStream>,
    in_flight: Option<InFlightEncodeBatch>,
    next_stream: Option<u64>,
    next_token: Option<u64>,
    started_at: Instant,
    last_timestamp_micros: u64,
    dump: Option<AccessUnitDump>,
    observations: SourceObservations,
}

impl EncodedSourceState {
    fn new(backend: Box<dyn EncodeBackend>) -> Self {
        let started_at = Instant::now();
        Self {
            backend,
            output: VecDeque::new(),
            pending: HashMap::new(),
            pending_order: VecDeque::new(),
            resizing: HashSet::new(),
            awaiting_credit: HashMap::new(),
            streams: HashMap::new(),
            in_flight: None,
            next_stream: Some(1),
            next_token: Some(1),
            started_at,
            last_timestamp_micros: 0,
            dump: None,
            observations: SourceObservations::new(started_at),
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
            || self.awaiting_credit.contains_key(&surface)
            || self
                .in_flight
                .as_ref()
                .is_some_and(|in_flight| in_flight.surface == surface);
        match &event.kind {
            ClientSurfaceEventKind::Commit(_) => {
                if self.resizing.contains(&surface)
                    || self.awaiting_credit.contains_key(&surface)
                    || self.in_flight.is_some()
                {
                    self.queue_event(session, event)?;
                } else {
                    self.submit_or_send(session, event)?;
                }
            }
            ClientSurfaceEventKind::Destroyed => {
                // The relay has removed this route, so no further credit can arrive.
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
            ensure!(
                self.awaiting_credit
                    .insert(
                        batch.surface,
                        OutstandingCredit {
                            revision,
                            sent_at: Instant::now(),
                        },
                    )
                    .is_none(),
                "encoded surface already held destination credit"
            );
            self.output
                .push_back(SourceTransportPacket::Control(SourceEnvelope {
                    session: batch.session,
                    message: SourceMessage::Surface(batch.event),
                }));
            self.schedule()?;
        }
        Ok(())
    }

    fn report_observations(&mut self, final_report: bool) {
        let now = Instant::now();
        if !self.observations.report_due(now, final_report) {
            return;
        }
        let gauges = SourceGauges {
            pending_events: self.pending.values().map(VecDeque::len).sum(),
            awaiting_credit: self.awaiting_credit.len(),
            active_streams: self.streams.len(),
            encode_in_flight: self.in_flight.is_some(),
            oldest_credit_age: self
                .awaiting_credit
                .values()
                .map(|credit| now.saturating_duration_since(credit.sent_at))
                .max()
                .unwrap_or_default(),
            active_batch_age: self
                .in_flight
                .as_ref()
                .map(|batch| now.saturating_duration_since(batch.started_at))
                .unwrap_or_default(),
        };
        if let Some(report) = self.observations.take_report(now, gauges, final_report) {
            report.emit();
        }
    }

    fn cancel_surface(&mut self, surface: ClientSurfaceId) -> Result<()> {
        self.pending.remove(&surface);
        self.pending_order.retain(|candidate| *candidate != surface);
        self.resizing.remove(&surface);
        if self.awaiting_credit.remove(&surface).is_some() {
            self.observations.record(SourceObservation::CreditCancelled);
        }
        if let Some(in_flight) = self.in_flight.as_mut()
            && in_flight.surface == surface
        {
            in_flight.cancelled = true;
            in_flight.pending.clear();
            in_flight.completed.clear();
        }
        self.retire_surface_streams(surface)
    }

    fn finish_remote_commit(
        &mut self,
        surface: ClientSurfaceId,
        revision: ClientCommitRevision,
        outcome: EncodedCommitOutcome,
    ) -> Result<()> {
        let Some(expected) = self.awaiting_credit.get(&surface) else {
            self.observations
                .record(SourceObservation::StaleCreditOutcome);
            tracing::trace!(?surface, ?revision, "ignored stale encoded commit outcome");
            return Ok(());
        };
        ensure!(
            expected.revision == revision,
            "encoded commit outcome revision did not match outstanding credit"
        );
        let credit = self
            .awaiting_credit
            .remove(&surface)
            .context("encoded destination credit disappeared")?;
        let turnaround = credit.sent_at.elapsed();
        self.observations.record(match outcome {
            EncodedCommitOutcome::Applied => SourceObservation::CreditApplied(turnaround),
            EncodedCommitOutcome::Dropped => SourceObservation::CreditCancelled,
        });
        tracing::trace!(
            ?surface,
            ?revision,
            ?outcome,
            credit_round_trip_micros = turnaround.as_micros(),
            pending_events = self.pending.values().map(VecDeque::len).sum::<usize>(),
            "finished encoded destination credit"
        );
        self.schedule()
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
        if self.in_flight.is_some() {
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
            if self.awaiting_credit.contains_key(&surface)
                || (self.resizing.contains(&surface)
                    && matches!(event.kind, ClientSurfaceEventKind::Commit(_)))
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
            let frame = self.allocate_frame(surface, layer, lease.metadata())?;
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
                },
                retained_dmabuf_lease,
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
        } = prepared;
        let token = request.token;
        let frame = request.frame;
        match self.backend.try_submit(request) {
            Ok(()) => Ok(ActiveEncode {
                token,
                frame,
                retained_dmabuf_lease,
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
    ) -> Result<MediaFrameId> {
        let visible_extent = (metadata.extent.width, metadata.extent.height);
        ensure!(
            visible_extent.0 > 0 && visible_extent.1 > 0,
            "encoded extent is zero"
        );
        let key = (surface, layer);
        if !self.streams.contains_key(&key) {
            let stream = take_counter(&mut self.next_stream, "encoded stream")?;
            self.streams.insert(
                key,
                SourceStream {
                    stream: MediaStreamId::new(stream),
                    generation: StreamGeneration::new(1),
                    visible_extent,
                    next_sequence: Some(0),
                },
            );
        }
        let (frame, retired) = {
            let stream = self
                .streams
                .get_mut(&key)
                .context("encoded stream disappeared")?;
            let retired = if stream.visible_extent != visible_extent {
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
            let sequence = take_counter(&mut stream.next_sequence, "encoded frame sequence")?;
            (
                MediaFrameId::new(stream.stream, stream.generation, sequence),
                retired,
            )
        };
        if let Some((stream, generation)) = retired {
            self.retire_generation(stream, generation)?;
        }
        Ok(frame)
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
        self.report_observations(true);
    }
}

/// Source relay port that schedules encoded commits over a binding-owned transport.
pub struct EncodedSourcePort<T> {
    transport: T,
    state: Option<EncodedSourceState>,
}

impl<T: EncodedSourceTransport> EncodedSourcePort<T> {
    pub fn new(transport: T, backend: Box<dyn EncodeBackend>) -> Self {
        Self {
            transport,
            state: Some(EncodedSourceState::new(backend)),
        }
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
        for packet in state.take_output() {
            self.transport.send(packet)?;
        }
        Ok(())
    }
}

impl<T: EncodedSourceTransport> HoistSourcePort for EncodedSourcePort<T> {
    fn submit(&mut self, command: SourcePortCommand) -> HoistPortResult<()> {
        match command {
            SourcePortCommand::MapSurface { session, surface } => {
                self.transport
                    .send(SourceTransportPacket::Control(SourceEnvelope {
                        session,
                        message: SourceMessage::Mapped { surface },
                    }))
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
                self.transport
                    .send(SourceTransportPacket::Control(SourceEnvelope {
                        session,
                        message: SourceMessage::Withdraw { surface },
                    }))?;
                self.state
                    .as_mut()
                    .ok_or_else(|| protocol_error("encoded source port is disconnected"))?
                    .cancel_surface(surface)
                    .map_err(protocol_error)
            }
            SourcePortCommand::RetireUpstreamBuffer(_) => Ok(()),
            SourcePortCommand::Cursor {
                session,
                update,
                sequence,
            } => self
                .transport
                .send(SourceTransportPacket::Control(SourceEnvelope {
                    session,
                    message: SourceMessage::Cursor { update, sequence },
                })),
        }
    }

    fn poll(&mut self) -> HoistPortResult<Vec<DestinationEnvelope>> {
        let state = self
            .state
            .as_mut()
            .ok_or_else(|| protocol_error("encoded source port is disconnected"))?;
        state.drain().map_err(protocol_error)?;
        state.report_observations(false);
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
            DestinationMessage::EncodedCommitFinished {
                surface,
                revision,
                outcome,
            } => self
                .state
                .as_mut()
                .ok_or_else(|| protocol_error("encoded source port is disconnected"))?
                .finish_remote_commit(*surface, *revision, *outcome)
                .map_err(protocol_error)?,
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
    }
}

struct QueuedDestinationEvent {
    session: HoistSessionId,
    source_surface: ClientSurfaceId,
    event: WireClientSurfaceEvent<EncodedBuffer>,
}

struct EncodedDestinationEvent {
    session: HoistSessionId,
    event: ClientSurfaceEvent,
}

struct EncodedCommitOutcomeRecord {
    session: HoistSessionId,
    surface: ClientSurfaceId,
    revision: ClientCommitRevision,
    outcome: EncodedCommitOutcome,
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
    media_frames: HashMap<MediaFrameId, (HoistSessionId, weld_media::EncodedAccessUnit)>,
    decoded: HashMap<MediaFrameId, weld_core::dmabuf::ExternalDmabuf>,
    decode_in_flight: Option<InFlightDecode>,
    cancelled_frames: HashMap<MediaFrameId, HoistSessionId>,
    streams: HashMap<(ClientSurfaceId, SurfaceLayerId), EncodedGeneration>,
    outcomes: Vec<EncodedCommitOutcomeRecord>,
    next_token: Option<u64>,
    next_buffer: Option<u64>,
    next_use: Option<u64>,
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
            outcomes: Vec::new(),
            next_token: Some(1),
            next_buffer: Some(1),
            next_use: Some(1),
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
        self.queues
            .entry(source_surface)
            .or_default()
            .push_back(QueuedDestinationEvent {
                session,
                source_surface,
                event,
            });
        self.advance(output)
    }

    fn take_outcomes(&mut self) -> Vec<EncodedCommitOutcomeRecord> {
        std::mem::take(&mut self.outcomes)
    }

    fn enqueue_media(&mut self, packet: MediaEnvelope<EncodedAccessUnit>) -> Result<()> {
        let frame = packet.access_unit.frame;
        if let Some(session) = self.cancelled_frames.get(&frame) {
            ensure!(
                *session == packet.session,
                "cancelled encoded media crossed hoist sessions"
            );
            self.cancelled_frames.remove(&frame);
            return Ok(());
        }
        ensure!(
            self.media_frames.len() < MAX_PENDING_MEDIA_FRAMES,
            "encoded destination media-frame bound exceeded"
        );
        ensure!(
            self.media_frames
                .insert(frame, (packet.session, packet.access_unit))
                .is_none(),
            "encoded media frame was delivered more than once"
        );
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
            if in_flight.cancelled {
                if let Err(error) = completion.result {
                    tracing::debug!(frame = ?in_flight.frame, error = %format_args!("{error:#}"),
                        "discarded cancelled decode failure");
                }
                self.backend
                    .retire(in_flight.frame.stream, in_flight.frame.generation)?;
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
            tracing::trace!(
                frame = ?frame.frame,
                decode_micros = in_flight.submitted_at.elapsed().as_micros(),
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
                            self.cancelled_frames.len() < MAX_CANCELLED_MEDIA_FRAMES,
                            "encoded destination cancelled-frame bound exceeded"
                        );
                        self.cancelled_frames.insert(*frame, event.session);
                    }
                }
                if !frames.is_empty() {
                    let revision = commit_revision(&event.event)
                        .context("encoded destination event was not a commit")?;
                    self.outcomes.push(EncodedCommitOutcomeRecord {
                        session: event.session,
                        surface: event.source_surface,
                        revision,
                        outcome: EncodedCommitOutcome::Dropped,
                    });
                }
            }
        }
        self.retire_surface_generations(surface)?;
        Ok(())
    }

    fn update_stream_generations(
        &mut self,
        event: &WireClientSurfaceEvent<EncodedBuffer>,
    ) -> Result<()> {
        let WireClientSurfaceEventKind::Commit(commit) = &event.kind else {
            return Ok(());
        };
        // Commits carry complete inventories. Credit orders them after the
        // previous decode; cancellation already extracts any active generation
        // from this map and defers its retirement to the matching completion.
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
            self.backend.retire(stream, generation)?;
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
                self.backend.retire(previous.0, previous.1)?;
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
            // A cancelled job can still create its generation after submission.
            // Defer that retirement to its matching completion.
            if self.decode_in_flight.as_ref().is_some_and(|active| {
                active.cancelled
                    && (active.frame.stream, active.frame.generation) == (stream, generation)
            }) {
                continue;
            }
            self.backend.retire(stream, generation)?;
        }
        Ok(())
    }

    fn advance(&mut self, output: &mut Vec<EncodedDestinationEvent>) -> Result<()> {
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
                    self.outcomes.push(EncodedCommitOutcomeRecord {
                        session: queued.session,
                        surface: queued.source_surface,
                        revision,
                        outcome: EncodedCommitOutcome::Applied,
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
        Ok(())
    }

    fn schedule_decode(&mut self, frame: MediaFrameId, session: HoistSessionId) -> Result<()> {
        if self.decode_in_flight.is_some() {
            return Ok(());
        }
        let Some((media_session, access_unit)) = self.media_frames.remove(&frame) else {
            return Ok(());
        };
        ensure!(
            media_session == session,
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
            access_unit,
            visible_width: metadata.extent.width,
            visible_height: metadata.extent.height,
        };
        match self.backend.try_submit(request) {
            Ok(()) => {
                self.decode_in_flight = Some(InFlightDecode {
                    token,
                    frame,
                    cancelled: false,
                    submitted_at: Instant::now(),
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

    fn flush_outcomes(&mut self) -> HoistPortResult<()> {
        let state = self
            .state
            .as_mut()
            .ok_or_else(|| protocol_error("encoded destination port is disconnected"))?;
        for outcome in state.take_outcomes() {
            self.transport.send(DestinationEnvelope {
                session: outcome.session,
                message: DestinationMessage::EncodedCommitFinished {
                    surface: outcome.surface,
                    revision: outcome.revision,
                    outcome: outcome.outcome,
                },
            })?;
        }
        Ok(())
    }
}

impl<T: EncodedDestinationTransport> HoistDestinationPort for EncodedDestinationPort<T> {
    fn poll(&mut self) -> HoistPortResult<Vec<DestinationPortRecord>> {
        let mut records = Vec::new();
        for packet in self.transport.drain()? {
            self.apply_source_packet(packet, &mut records)?;
        }
        let mut decoded = Vec::new();
        self.state
            .as_mut()
            .ok_or_else(|| protocol_error("encoded destination port is disconnected"))?
            .drain(&mut decoded)
            .map_err(protocol_error)?;
        extend_encoded_records(&mut records, decoded);
        self.flush_outcomes()?;
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
    use std::{
        cell::{Cell, RefCell},
        rc::Rc,
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
    }

    #[derive(Clone)]
    struct FakeSourceTransport(Rc<RefCell<FakeSourceTransportState>>);

    impl EncodedSourceTransport for FakeSourceTransport {
        fn send(&self, packet: SourceTransportPacket) -> HoistPortResult<()> {
            self.0.borrow_mut().sent.push(packet);
            Ok(())
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

        fn drain(&self) -> HoistPortResult<Vec<SourceTransportPacket>> {
            Ok(self.0.borrow_mut().incoming.drain(..).collect())
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
    }

    struct FakeEncoder(Rc<RefCell<FakeEncoderState>>);

    impl EncodeBackend for FakeEncoder {
        fn try_submit(&mut self, request: EncodeRequest) -> Result<(), SubmitError<EncodeRequest>> {
            let EncodeInput::PackedBgra { pixels, .. } = request.input else {
                return Err(SubmitError::Rejected(anyhow::anyhow!(
                    "test expected packed BGRA"
                )));
            };
            let mut state = self.0.borrow_mut();
            let generation = (request.frame.stream, request.frame.generation);
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
            state.submitted.push((request.token, request.frame, pixels));
            Ok(())
        }

        fn drain(&mut self) -> Vec<EncodeCompletion> {
            std::mem::take(&mut self.0.borrow_mut().completions)
        }

        fn retire(&mut self, stream: MediaStreamId, generation: StreamGeneration) -> Result<()> {
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
    fn cursor_overtaking_credit_blocked_unmap_remap_keeps_its_newer_preference() {
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
        source.poll().expect("park first frame in credit");
        assert!(
            source
                .state
                .as_ref()
                .expect("state")
                .awaiting_credit
                .contains_key(&surface)
        );

        // Model an already-displayed first frame while withholding its credit.
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
                .expect("credit-blocked lifecycle");
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
            .accept_destination(&DestinationEnvelope {
                session,
                message: DestinationMessage::EncodedCommitFinished {
                    surface,
                    revision: ClientCommitRevision::new(1),
                    outcome: EncodedCommitOutcome::Applied,
                },
            })
            .expect("release frame credit");
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
    fn destroyed_relay_surfaces_do_not_wait_for_credit_or_leak_streams() {
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
            )), "destruction must bypass the now-unroutable credit");
            transport
                .borrow_mut()
                .incoming
                .push_back(DestinationEnvelope {
                    session,
                    message: DestinationMessage::EncodedCommitFinished {
                        surface,
                        revision: ClientCommitRevision::new(1),
                        outcome: EncodedCommitOutcome::Applied,
                    },
                });
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
        assert_eq!(encoder.borrow().submitted.len(), 2);
        assert_eq!(output_kinds(&source.output), vec!["control"]);
    }

    #[test]
    fn cancelled_decodes_ignore_results_and_retire_only_after_completion() {
        for result in [
            Err(anyhow::anyhow!("obsolete decode failed")),
            Ok(Vec::new()),
        ] {
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
            state.take_outcomes();
        }
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
                if let Some(revision) = commit_revision(&event) {
                    source
                        .finish_remote_commit(
                            event.surface,
                            revision,
                            EncodedCommitOutcome::Applied,
                        )
                        .expect("credit");
                }
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
        port.accept_destination(&DestinationEnvelope {
            session,
            message: DestinationMessage::EncodedCommitFinished {
                surface,
                revision: ClientCommitRevision::new(5),
                outcome: EncodedCommitOutcome::Applied,
            },
        })
        .expect("return destination credit");

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
        }
    }

    #[test]
    fn destination_flushes_dropped_commit_outcomes() {
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
        assert!(matches!(
            transport.borrow().sent.as_slice(),
            [DestinationEnvelope {
                message: DestinationMessage::EncodedCommitFinished {
                    surface: observed,
                    revision,
                    outcome: EncodedCommitOutcome::Dropped,
                },
                ..
            }] if *observed == surface && *revision == ClientCommitRevision::new(8)
        ));
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

        let first = source
            .allocate_frame(surface, layer, metadata(484))
            .expect("first frame");
        let retained = source
            .allocate_frame(surface, layer, metadata(484))
            .expect("retained extent");
        let odd = source
            .allocate_frame(surface, layer, metadata(485))
            .expect("odd extent");
        let even = source
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
    fn destination_credit_blocks_only_its_surface() {
        let (mut source, fake) = source();
        let source_id = ClientSourceId::new(1);
        let first = surface(source_id, 2, 3);
        let second = surface(source_id, 4, 5);
        let session = HoistSessionId::new(6);
        let metadata = ClientBufferMetadata::new(Extent::new(1, 1), true);

        source
            .enqueue(
                session,
                one_buffer_commit(first, 1, 1, shm_lease(source_id, 7, 10, metadata)),
            )
            .expect("first surface");
        let (token, frame, _) = fake.borrow().submitted[0].clone();
        complete(&fake, token, frame, 1);
        source.drain().expect("first completion");
        source.take_output();

        for (local, pixel) in [(8, 20), (9, 30)] {
            source
                .enqueue(
                    session,
                    one_buffer_commit(
                        first,
                        local,
                        1,
                        shm_lease(source_id, local, pixel, metadata),
                    ),
                )
                .expect("coalesced first surface");
        }
        source
            .enqueue(
                session,
                one_buffer_commit(second, 10, 1, shm_lease(source_id, 10, 40, metadata)),
            )
            .expect("second surface");
        assert_eq!(fake.borrow().submitted.len(), 2);
        assert_eq!(fake.borrow().submitted[1].2, vec![40, 40, 40, 255]);

        source
            .finish_remote_commit(
                first,
                ClientCommitRevision::new(1),
                EncodedCommitOutcome::Applied,
            )
            .expect("first credit");
        assert_eq!(fake.borrow().submitted.len(), 2);
        let report = source
            .observations
            .take_report(Instant::now(), SourceGauges::default(), true)
            .expect("applied credit observations");
        assert_eq!(report.counters.applied_credit_turnaround.samples, 1);
        assert_eq!(report.counters.credits_cancelled, 0);
    }

    #[test]
    fn source_observations_separate_coalescing_cancellation_and_late_credit() {
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
                .expect("credit-blocked commit");
        }
        assert_eq!(fake.borrow().submitted.len(), 1);
        source.cancel_surface(surface).expect("withdraw credit");
        source
            .finish_remote_commit(
                surface,
                ClientCommitRevision::new(1),
                EncodedCommitOutcome::Applied,
            )
            .expect("late reply");
        let report = source
            .observations
            .take_report(Instant::now(), SourceGauges::default(), true)
            .expect("observations");
        assert_eq!(report.counters.commits_received, 3);
        assert_eq!(report.counters.commits_coalesced, 1);
        assert_eq!(report.counters.batches_completed, 1);
        assert_eq!(report.counters.credits_cancelled, 1);
        assert_eq!(report.counters.stale_credit_outcomes, 1);
        assert_eq!(report.counters.applied_credit_turnaround.samples, 0);
        assert!(source.pending.is_empty());
        assert!(source.awaiting_credit.is_empty());
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
    fn source_observations_do_not_accept_a_mismatched_credit_revision() {
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
        let (token, frame, _) = fake.borrow().submitted[0].clone();
        complete(&fake, token, frame, 1);
        source.drain().expect("completion");
        assert!(
            source
                .finish_remote_commit(
                    surface,
                    ClientCommitRevision::new(99),
                    EncodedCommitOutcome::Applied
                )
                .is_err()
        );
        assert!(source.awaiting_credit.contains_key(&surface));
        let report = source
            .observations
            .take_report(Instant::now(), SourceGauges::default(), true)
            .expect("observations");
        assert_eq!(report.counters.applied_credit_turnaround.samples, 0);
        assert_eq!(report.counters.credits_cancelled, 0);
        assert_eq!(report.counters.stale_credit_outcomes, 0);
    }
}
