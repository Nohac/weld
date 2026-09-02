//! Encoded local-hoist state machines, independent from the hardware backend.

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
    ClientCommitRevision, ClientSourceDescriptor, ClientSurfaceEvent, ClientSurfaceEventKind,
    ClientSurfaceId, SurfaceBufferChange, SurfaceLayerId, WireClientSurfaceEvent,
    WireClientSurfaceEventKind, WireSurfaceBufferChange,
};
use weld_core::dmabuf::{DirectClientBufferAccess, DmabufContext, export_client_dmabuf};
use weld_hoist_core::HoistSessionId;
use weld_media::{MediaFrameId, MediaStreamId, StreamGeneration, VideoCodec};

use crate::{
    LocalBuffer, LocalBufferContent, LocalEncodedBuffer, LocalEncodedCommitOutcome,
    LocalMediaPacket, LocalPacketConnection, LocalSourceMessage, LocalSourcePacket,
    codec::{
        LocalDecodeBackend, LocalDecodeRequest, LocalEncodeBackend, LocalEncodeInput,
        LocalEncodeRequest, LocalSubmitError,
    },
    import_access_unit,
    media::export_access_unit,
};

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
    request: LocalEncodeRequest,
    retained_dmabuf_lease: Option<ClientBufferLease>,
}

struct ActiveEncode {
    token: u64,
    retained_dmabuf_lease: Option<ClientBufferLease>,
}

struct InFlightEncodeBatch {
    session: HoistSessionId,
    surface: ClientSurfaceId,
    event: WireClientSurfaceEvent<LocalBuffer>,
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

pub(crate) struct EncodedSourceState {
    backend: Box<dyn LocalEncodeBackend>,
    media: LocalPacketConnection,
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
}

impl EncodedSourceState {
    pub(crate) fn new(backend: Box<dyn LocalEncodeBackend>, media: LocalPacketConnection) -> Self {
        Self {
            backend,
            media,
            pending: HashMap::new(),
            pending_order: VecDeque::new(),
            resizing: HashSet::new(),
            awaiting_credit: HashMap::new(),
            streams: HashMap::new(),
            in_flight: None,
            next_stream: Some(1),
            next_token: Some(1),
            started_at: Instant::now(),
            last_timestamp_micros: 0,
            dump: None,
        }
    }

    pub(crate) fn with_access_unit_dump_directory(
        mut self,
        directory: PathBuf,
        codec: VideoCodec,
    ) -> Result<Self> {
        self.dump = Some(AccessUnitDump::new(directory, codec)?);
        Ok(self)
    }

    pub(crate) fn enqueue(
        &mut self,
        session: HoistSessionId,
        event: ClientSurfaceEvent,
        control: &LocalPacketConnection,
    ) -> Result<()> {
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
                    self.submit_or_send(session, event, control)?;
                }
            }
            ClientSurfaceEventKind::Destroyed => {
                if surface_busy {
                    self.queue_event(session, event)?;
                } else {
                    self.send_without_buffer(session, event, control)?;
                }
            }
            ClientSurfaceEventKind::Role(_) | ClientSurfaceEventKind::Interaction(_) => {
                if surface_busy {
                    self.queue_event(session, event)?;
                } else {
                    self.send_without_buffer(session, event, control)?;
                }
            }
        }
        Ok(())
    }

    pub(crate) fn set_resizing(
        &mut self,
        surface: ClientSurfaceId,
        resizing: bool,
        control: &LocalPacketConnection,
    ) -> Result<()> {
        if resizing {
            self.resizing.insert(surface);
        } else {
            self.resizing.remove(&surface);
            self.schedule(control)?;
        }
        Ok(())
    }

    pub(crate) fn drain(&mut self, control: &LocalPacketConnection) -> Result<()> {
        for completion in self.backend.drain() {
            let mut batch = self
                .in_flight
                .take()
                .context("encoder completed without an in-flight source event")?;
            ensure!(
                completion.token == batch.active.token,
                "encoder completed an unexpected source event"
            );
            let access_unit = completion.result?;
            drop(batch.active.retained_dmabuf_lease.take());
            if batch.cancelled {
                drop(batch.pending);
                self.schedule(control)?;
                continue;
            }
            batch.completed.push(access_unit);
            if let Some(next) = batch.pending.pop_front() {
                batch.active = self.submit_prepared(next)?;
                self.in_flight = Some(batch);
                continue;
            }
            let revision = commit_revision(&batch.event)
                .context("encoded batch control event was not a commit")?;
            tracing::trace!(
                surface = ?batch.surface,
                ?revision,
                frames = batch.completed.len(),
                payload_bytes = batch.completed.iter().map(|unit| unit.payload.len()).sum::<usize>(),
                encode_batch_micros = batch.started_at.elapsed().as_micros(),
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
                let (access_unit, descriptors) = export_access_unit(access_unit)?;
                self.media.queue(
                    &LocalMediaPacket {
                        session: batch.session,
                        access_unit,
                    },
                    descriptors,
                )?;
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
            let removed_layers = removed_layers(&batch.event);
            control.queue(
                &LocalSourcePacket {
                    session: batch.session,
                    message: LocalSourceMessage::Surface(batch.event),
                },
                Vec::new(),
            )?;
            self.retire_layers(batch.surface, removed_layers)?;
            self.schedule(control)?;
        }
        Ok(())
    }

    pub(crate) fn cancel_surface(&mut self, surface: ClientSurfaceId) -> Result<()> {
        self.pending.remove(&surface);
        self.pending_order.retain(|candidate| *candidate != surface);
        self.resizing.remove(&surface);
        self.awaiting_credit.remove(&surface);
        if let Some(in_flight) = self.in_flight.as_mut()
            && in_flight.surface == surface
        {
            in_flight.cancelled = true;
            in_flight.pending.clear();
            in_flight.completed.clear();
        }
        self.retire_surface_streams(surface)
    }

    pub(crate) fn finish_remote_commit(
        &mut self,
        surface: ClientSurfaceId,
        revision: ClientCommitRevision,
        outcome: LocalEncodedCommitOutcome,
        control: &LocalPacketConnection,
    ) -> Result<()> {
        let Some(expected) = self.awaiting_credit.get(&surface) else {
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
        tracing::trace!(
            ?surface,
            ?revision,
            ?outcome,
            credit_round_trip_micros = credit.sent_at.elapsed().as_micros(),
            pending_events = self.pending.values().map(VecDeque::len).sum::<usize>(),
            "finished encoded destination credit"
        );
        self.schedule(control)
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
        {
            current.carry_unobserved_content_from(previous);
            queue.pop_back();
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

    fn schedule(&mut self, control: &LocalPacketConnection) -> Result<()> {
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
            self.submit_or_send(session, event, control)?;
            if self.in_flight.is_some() {
                return Ok(());
            }
        }
        Ok(())
    }

    fn submit_or_send(
        &mut self,
        session: HoistSessionId,
        event: ClientSurfaceEvent,
        control: &LocalPacketConnection,
    ) -> Result<()> {
        let replaced = replaced_buffer_count(&event);
        match replaced {
            0 => self.send_without_buffer(session, event, control),
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
        control: &LocalPacketConnection,
    ) -> Result<()> {
        let surface = event.surface;
        let destroyed = matches!(event.kind, ClientSurfaceEventKind::Destroyed);
        let event = WireClientSurfaceEvent::try_from_client(event, |_| {
            Err::<LocalBuffer, _>(anyhow::anyhow!(
                "buffer replacement entered the structural encoded path"
            ))
        })?;
        let removed_layers = removed_layers(&event);
        control.queue(
            &LocalSourcePacket {
                session,
                message: LocalSourceMessage::Surface(event),
            },
            Vec::new(),
        )?;
        if destroyed {
            self.retire_surface_streams(surface)?;
        } else {
            self.retire_layers(surface, removed_layers)?;
        }
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
                    (LocalEncodeInput::Dmabuf(dmabuf), Some(lease.clone()))
                }
                DirectClientBufferAccess::Shm(shm) => (
                    LocalEncodeInput::PackedBgra {
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
                request: LocalEncodeRequest {
                    token,
                    frame,
                    timestamp_micros,
                    input,
                },
                retained_dmabuf_lease,
            });
            Ok::<_, anyhow::Error>(LocalBuffer {
                buffer: lease.buffer(),
                use_id: lease.use_id(),
                content: LocalBufferContent::Encoded(LocalEncodedBuffer { frame }),
            })
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
        match self.backend.try_submit(request) {
            Ok(()) => Ok(ActiveEncode {
                token,
                retained_dmabuf_lease,
            }),
            Err(LocalSubmitError::Busy(_)) => {
                bail!("encoder queue was busy without an in-flight source frame")
            }
            Err(LocalSubmitError::Stopped(_)) => bail!("encoder worker stopped"),
            Err(LocalSubmitError::Rejected(error)) => Err(error),
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

    fn retire_layers(
        &mut self,
        surface: ClientSurfaceId,
        layers: Vec<SurfaceLayerId>,
    ) -> Result<()> {
        for layer in layers {
            if let Some(stream) = self.streams.remove(&(surface, layer)) {
                self.retire_generation(stream.stream, stream.generation)?;
            }
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
        self.backend.retire(stream, generation)?;
        if let Some(dump) = &mut self.dump {
            dump.retire(stream, generation)?;
        }
        Ok(())
    }
}

struct QueuedDestinationEvent {
    session: HoistSessionId,
    source_surface: ClientSurfaceId,
    event: WireClientSurfaceEvent<LocalBuffer>,
}

pub(crate) struct EncodedDestinationEvent {
    pub(crate) session: HoistSessionId,
    pub(crate) event: ClientSurfaceEvent,
}

pub(crate) struct EncodedCommitOutcomeRecord {
    pub(crate) session: HoistSessionId,
    pub(crate) surface: ClientSurfaceId,
    pub(crate) revision: ClientCommitRevision,
    pub(crate) outcome: LocalEncodedCommitOutcome,
}

struct InFlightDecode {
    token: u64,
    frame: MediaFrameId,
    submitted_at: Instant,
}

pub(crate) struct EncodedDestinationState {
    backend: Box<dyn LocalDecodeBackend>,
    media: LocalPacketConnection,
    descriptor: ClientSourceDescriptor,
    dmabuf: DmabufContext,
    queues: HashMap<ClientSurfaceId, VecDeque<QueuedDestinationEvent>>,
    media_frames: HashMap<MediaFrameId, (HoistSessionId, weld_media::EncodedAccessUnit)>,
    decoded: HashMap<MediaFrameId, weld_core::dmabuf::ExternalDmabuf>,
    decode_in_flight: Option<InFlightDecode>,
    cancelled_frames: HashSet<MediaFrameId>,
    streams: HashMap<(ClientSurfaceId, SurfaceLayerId), EncodedGeneration>,
    outcomes: Vec<EncodedCommitOutcomeRecord>,
    next_token: Option<u64>,
    next_buffer: Option<u64>,
    next_use: Option<u64>,
}

impl EncodedDestinationState {
    pub(crate) fn new(
        backend: Box<dyn LocalDecodeBackend>,
        media: LocalPacketConnection,
        descriptor: ClientSourceDescriptor,
        dmabuf: DmabufContext,
    ) -> Self {
        Self {
            backend,
            media,
            descriptor,
            dmabuf,
            queues: HashMap::new(),
            media_frames: HashMap::new(),
            decoded: HashMap::new(),
            decode_in_flight: None,
            cancelled_frames: HashSet::new(),
            streams: HashMap::new(),
            outcomes: Vec::new(),
            next_token: Some(1),
            next_buffer: Some(1),
            next_use: Some(1),
        }
    }

    pub(crate) fn enqueue(
        &mut self,
        session: HoistSessionId,
        mut event: WireClientSurfaceEvent<LocalBuffer>,
        output: &mut Vec<EncodedDestinationEvent>,
    ) -> Result<()> {
        let source_surface = event.surface;
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

    pub(crate) fn take_outcomes(&mut self) -> Vec<EncodedCommitOutcomeRecord> {
        std::mem::take(&mut self.outcomes)
    }

    pub(crate) fn drain(&mut self, output: &mut Vec<EncodedDestinationEvent>) -> Result<()> {
        let packets = self.media.drain::<LocalMediaPacket>()?;
        for packet in packets {
            ensure!(
                packet.file_descriptors.len() == 1,
                "encoded media packet must attach exactly one descriptor"
            );
            let frame = packet.message.access_unit.header.frame;
            if self.cancelled_frames.remove(&frame) {
                continue;
            }
            ensure!(
                self.media_frames.len() < MAX_PENDING_MEDIA_FRAMES,
                "encoded destination media-frame bound exceeded"
            );
            let access_unit =
                import_access_unit(packet.message.access_unit, packet.file_descriptors)?;
            ensure!(
                self.media_frames
                    .insert(frame, (packet.message.session, access_unit))
                    .is_none(),
                "encoded media frame was delivered more than once"
            );
        }
        for completion in self.backend.drain() {
            let in_flight = self
                .decode_in_flight
                .take()
                .context("decoder completed without an in-flight frame")?;
            ensure!(
                completion.token == in_flight.token,
                "decoder completed an unexpected token"
            );
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
            if !self.cancelled_frames.remove(&frame.frame) {
                self.decoded.insert(frame.frame, frame.dmabuf);
            }
        }
        self.advance(output)
    }

    pub(crate) fn cancel_surface(&mut self, surface: ClientSurfaceId) -> Result<()> {
        if let Some(queue) = self.queues.remove(&surface) {
            for event in queue {
                let frames = encoded_frames(&event.event);
                for frame in &frames {
                    let media_was_pending = self.media_frames.remove(frame).is_some();
                    self.decoded.remove(frame);
                    let decode_is_in_flight = self
                        .decode_in_flight
                        .as_ref()
                        .is_some_and(|submitted| submitted.frame == *frame);
                    if decode_is_in_flight || !media_was_pending {
                        ensure!(
                            self.cancelled_frames.len() < MAX_CANCELLED_MEDIA_FRAMES,
                            "encoded destination cancelled-frame bound exceeded"
                        );
                        self.cancelled_frames.insert(*frame);
                    }
                }
                if !frames.is_empty() {
                    let revision = commit_revision(&event.event)
                        .context("encoded destination event was not a commit")?;
                    self.outcomes.push(EncodedCommitOutcomeRecord {
                        session: event.session,
                        surface: event.source_surface,
                        revision,
                        outcome: LocalEncodedCommitOutcome::Dropped,
                    });
                }
            }
        }
        self.retire_surface_generations(surface)?;
        Ok(())
    }

    fn update_stream_generations(
        &mut self,
        event: &WireClientSurfaceEvent<LocalBuffer>,
    ) -> Result<()> {
        let WireClientSurfaceEventKind::Commit(commit) = &event.kind else {
            return Ok(());
        };
        for update in &commit.buffers {
            let key = (event.surface, update.layer);
            let next = match &update.change {
                WireSurfaceBufferChange::Replaced { buffer, .. } => match buffer.content {
                    LocalBufferContent::Encoded(encoded) => {
                        Some((encoded.frame.stream, encoded.frame.generation))
                    }
                    _ => None,
                },
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
                    let event = queued
                        .event
                        .try_into_client(|buffer, _| match buffer.content {
                            LocalBufferContent::Encoded(encoded) => {
                                let dmabuf = self
                                    .decoded
                                    .remove(&encoded.frame)
                                    .context("decoded layer frame disappeared")?;
                                self.import_decoded(dmabuf)
                            }
                            _ => bail!("encoded destination received native buffer content"),
                        })?;
                    output.push(EncodedDestinationEvent {
                        session: queued.session,
                        event,
                    });
                    self.outcomes.push(EncodedCommitOutcomeRecord {
                        session: queued.session,
                        surface: queued.source_surface,
                        revision,
                        outcome: LocalEncodedCommitOutcome::Applied,
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
        let request = LocalDecodeRequest {
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
                    submitted_at: Instant::now(),
                });
                Ok(())
            }
            Err(LocalSubmitError::Busy(_)) => {
                bail!("decoder queue was busy without an in-flight frame")
            }
            Err(LocalSubmitError::Stopped(_)) => bail!("decoder worker stopped"),
            Err(LocalSubmitError::Rejected(error)) => Err(error),
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
        let metadata = ClientBufferMetadata::new(dmabuf.extent, true);
        let access = self.dmabuf.import_external(dmabuf)?;
        let access_for_release = access.clone();
        let dmabuf_for_release = self.dmabuf.clone();
        let buffer = ClientBufferId::new(
            self.descriptor.id,
            take_counter(&mut self.next_buffer, "decoded buffer")?,
        );
        let use_id = ClientBufferUseId::new(
            self.descriptor.id,
            take_counter(&mut self.next_use, "decoded buffer use")?,
        );
        self.dmabuf
            .lease_external(buffer, use_id, metadata, access, move |_| {
                dmabuf_for_release.remove_external(&access_for_release)
            })
    }
}

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

fn commit_revision(event: &WireClientSurfaceEvent<LocalBuffer>) -> Option<ClientCommitRevision> {
    match &event.kind {
        WireClientSurfaceEventKind::Commit(commit) => Some(commit.revision),
        WireClientSurfaceEventKind::Role(_)
        | WireClientSurfaceEventKind::Interaction(_)
        | WireClientSurfaceEventKind::Destroyed => None,
    }
}

fn encoded_frames(event: &WireClientSurfaceEvent<LocalBuffer>) -> Vec<MediaFrameId> {
    let WireClientSurfaceEventKind::Commit(commit) = &event.kind else {
        return Vec::new();
    };
    commit
        .buffers
        .iter()
        .filter_map(|update| match &update.change {
            WireSurfaceBufferChange::Replaced { buffer, .. } => match buffer.content {
                LocalBufferContent::Encoded(encoded) => Some(encoded.frame),
                _ => None,
            },
            WireSurfaceBufferChange::Retained { .. } | WireSurfaceBufferChange::Removed => None,
        })
        .collect()
}

fn removed_layers(event: &WireClientSurfaceEvent<LocalBuffer>) -> Vec<SurfaceLayerId> {
    let WireClientSurfaceEventKind::Commit(commit) = &event.kind else {
        return Vec::new();
    };
    commit
        .buffers
        .iter()
        .filter_map(|update| {
            matches!(update.change, WireSurfaceBufferChange::Removed).then_some(update.layer)
        })
        .collect()
}

fn encoded_metadata(
    event: &WireClientSurfaceEvent<LocalBuffer>,
    frame: MediaFrameId,
) -> Option<ClientBufferMetadata> {
    let WireClientSurfaceEventKind::Commit(commit) = &event.kind else {
        return None;
    };
    commit.buffers.iter().find_map(|update| match &update.change {
        WireSurfaceBufferChange::Replaced { metadata, buffer }
            if matches!(buffer.content, LocalBufferContent::Encoded(encoded) if encoded.frame == frame) =>
        {
            Some(*metadata)
        }
        _ => None,
    })
}

fn mark_encoded_buffers_opaque(event: &mut WireClientSurfaceEvent<LocalBuffer>) {
    let WireClientSurfaceEventKind::Commit(commit) = &mut event.kind else {
        return;
    };
    for update in &mut commit.buffers {
        if let WireSurfaceBufferChange::Replaced { metadata, buffer } = &mut update.change
            && matches!(buffer.content, LocalBufferContent::Encoded(_))
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
    use std::{cell::RefCell, rc::Rc};

    use weld_client::{
        ClientBufferId, ClientBufferUseId, ClientCommitRevision, ClientId, ClientSourceId,
        ClientSurfaceCommit, Extent, SurfaceBufferUpdate, SurfaceLayerId,
        ToplevelInteractionRequestKind,
    };
    use weld_media::{EncodedAccessUnit, EncodedFrameKind, VideoCodec};

    use super::*;
    use crate::codec::LocalEncodeCompletion;

    #[derive(Default)]
    struct FakeEncoderState {
        submitted: Vec<(u64, MediaFrameId, Vec<u8>)>,
        completions: Vec<LocalEncodeCompletion>,
        retirements: Vec<(MediaStreamId, StreamGeneration)>,
    }

    struct FakeEncoder(Rc<RefCell<FakeEncoderState>>);

    impl LocalEncodeBackend for FakeEncoder {
        fn try_submit(
            &mut self,
            request: LocalEncodeRequest,
        ) -> Result<(), LocalSubmitError<LocalEncodeRequest>> {
            let LocalEncodeInput::PackedBgra { pixels, .. } = request.input else {
                return Err(LocalSubmitError::Rejected(anyhow::anyhow!(
                    "test expected packed BGRA"
                )));
            };
            self.0
                .borrow_mut()
                .submitted
                .push((request.token, request.frame, pixels));
            Ok(())
        }

        fn drain(&mut self) -> Vec<LocalEncodeCompletion> {
            std::mem::take(&mut self.0.borrow_mut().completions)
        }

        fn retire(&mut self, stream: MediaStreamId, generation: StreamGeneration) -> Result<()> {
            self.0.borrow_mut().retirements.push((stream, generation));
            Ok(())
        }
    }

    #[test]
    fn exact_visible_extent_rotates_and_retires_stream_generations() {
        let (media, _media_peer) = LocalPacketConnection::pair().expect("media pair");
        let fake = Rc::new(RefCell::new(FakeEncoderState::default()));
        let mut source = EncodedSourceState::new(Box::new(FakeEncoder(fake.clone())), media);
        let source_id = ClientSourceId::new(1);
        let surface = ClientSurfaceId::new(ClientId::new(source_id, 2), 3);
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
    fn resize_coalesces_to_latest_shm_frame_and_releases_before_encode() {
        let (control, control_peer) = LocalPacketConnection::pair().expect("control pair");
        let (media, media_peer) = LocalPacketConnection::pair().expect("media pair");
        let fake = Rc::new(RefCell::new(FakeEncoderState::default()));
        let mut source = EncodedSourceState::new(Box::new(FakeEncoder(fake.clone())), media);
        let source_id = ClientSourceId::new(1);
        let surface = ClientSurfaceId::new(ClientId::new(source_id, 2), 3);
        let session = HoistSessionId::new(4);
        let releases = Rc::new(RefCell::new(Vec::new()));

        source
            .set_resizing(surface, true, &control)
            .expect("begin resize");
        for (use_local, pixel) in [(5, 10), (6, 20)] {
            let releases_for_callback = releases.clone();
            let use_id = ClientBufferUseId::new(source_id, use_local);
            let metadata = ClientBufferMetadata::new(Extent::new(1, 1), true);
            let lease = ClientBufferLease::new(
                ClientBufferId::new(source_id, use_local),
                use_id,
                metadata,
                Rc::new(DirectClientBufferAccess::Shm(
                    weld_core::dmabuf::WaylandShmBuffer {
                        bgra_pixels: vec![pixel, pixel, pixel, 255],
                    },
                )),
                move |released| releases_for_callback.borrow_mut().push(released),
            )
            .expect("matching source");
            source
                .enqueue(session, commit(surface, metadata, lease), &control)
                .expect("queued resize commit");
        }
        assert!(fake.borrow().submitted.is_empty());

        source
            .set_resizing(surface, false, &control)
            .expect("settle resize");
        let (token, frame, pixels) = fake.borrow().submitted[0].clone();
        assert_eq!(pixels, vec![20, 20, 20, 255]);
        assert_eq!(releases.borrow().len(), 2);

        fake.borrow_mut().completions.push(LocalEncodeCompletion {
            token,
            result: Ok(EncodedAccessUnit {
                frame,
                codec: VideoCodec::H264,
                kind: EncodedFrameKind::Keyframe,
                timestamp_micros: 7,
                payload: vec![1, 2, 3],
            }),
        });
        source.drain(&control).expect("completed encoded frame");
        control.pump().expect("send control");
        source.media.pump().expect("send media");

        let control_packets = control_peer
            .drain::<LocalSourcePacket>()
            .expect("control packet");
        let media_packets = media_peer
            .drain::<LocalMediaPacket>()
            .expect("media packet");
        assert_eq!(control_packets.len(), 1);
        assert_eq!(media_packets.len(), 1);
        assert_eq!(media_packets[0].message.access_unit.header.frame, frame);
    }

    #[test]
    fn multi_layer_commit_is_published_only_after_every_frame_completes() {
        let (control, control_peer) = LocalPacketConnection::pair().expect("control pair");
        let (media, media_peer) = LocalPacketConnection::pair().expect("media pair");
        let fake = Rc::new(RefCell::new(FakeEncoderState::default()));
        let mut source = EncodedSourceState::new(Box::new(FakeEncoder(fake.clone())), media);
        let source_id = ClientSourceId::new(1);
        let surface = ClientSurfaceId::new(ClientId::new(source_id, 2), 3);
        let session = HoistSessionId::new(4);
        let metadata = ClientBufferMetadata::new(Extent::new(1, 1), true);
        let releases = Rc::new(RefCell::new(Vec::new()));
        let buffers = [(1, 5, 10), (2, 6, 20)]
            .into_iter()
            .map(|(layer, local, pixel)| {
                let releases_for_callback = releases.clone();
                let lease = ClientBufferLease::new(
                    ClientBufferId::new(source_id, local),
                    ClientBufferUseId::new(source_id, local),
                    metadata,
                    Rc::new(DirectClientBufferAccess::Shm(
                        weld_core::dmabuf::WaylandShmBuffer {
                            bgra_pixels: vec![pixel, pixel, pixel, 255],
                        },
                    )),
                    move |released| releases_for_callback.borrow_mut().push(released),
                )
                .expect("matching source");
                SurfaceBufferUpdate {
                    layer: SurfaceLayerId::new(layer),
                    change: SurfaceBufferChange::Replaced {
                        metadata,
                        buffer: lease,
                    },
                }
            })
            .collect();

        source
            .enqueue(session, commit_with_buffers(surface, 7, buffers), &control)
            .expect("submit multi-layer commit");
        assert_eq!(fake.borrow().submitted.len(), 1);
        assert_eq!(releases.borrow().len(), 2);

        let (first_token, first_frame, _) = fake.borrow().submitted[0].clone();
        complete(&fake, first_token, first_frame, 1);
        source.drain(&control).expect("complete first layer");
        assert_eq!(fake.borrow().submitted.len(), 2);
        control.pump().expect("pump withheld control");
        source.media.pump().expect("pump withheld media");
        assert!(
            control_peer
                .drain::<LocalSourcePacket>()
                .expect("control")
                .is_empty()
        );
        assert!(
            media_peer
                .drain::<LocalMediaPacket>()
                .expect("media")
                .is_empty()
        );

        let (second_token, second_frame, _) = fake.borrow().submitted[1].clone();
        assert_ne!(first_frame.stream, second_frame.stream);
        complete(&fake, second_token, second_frame, 2);
        source.drain(&control).expect("complete second layer");
        control.pump().expect("send control");
        source.media.pump().expect("send media");

        let control_packets = control_peer
            .drain::<LocalSourcePacket>()
            .expect("control packet");
        let media_packets = media_peer
            .drain::<LocalMediaPacket>()
            .expect("media packets");
        assert_eq!(control_packets.len(), 1);
        assert_eq!(media_packets.len(), 2);
        assert_eq!(
            media_packets[0].message.access_unit.header.frame,
            first_frame
        );
        assert_eq!(
            media_packets[1].message.access_unit.header.frame,
            second_frame
        );
    }

    #[test]
    fn structural_events_behind_a_frame_keep_order_without_another_wake() {
        let (control, control_peer) = LocalPacketConnection::pair().expect("control pair");
        let (media, _media_peer) = LocalPacketConnection::pair().expect("media pair");
        let fake = Rc::new(RefCell::new(FakeEncoderState::default()));
        let mut source = EncodedSourceState::new(Box::new(FakeEncoder(fake.clone())), media);
        let source_id = ClientSourceId::new(1);
        let surface = ClientSurfaceId::new(ClientId::new(source_id, 2), 3);
        let session = HoistSessionId::new(4);
        let metadata = ClientBufferMetadata::new(Extent::new(1, 1), true);
        let lease = ClientBufferLease::new(
            ClientBufferId::new(source_id, 5),
            ClientBufferUseId::new(source_id, 5),
            metadata,
            Rc::new(DirectClientBufferAccess::Shm(
                weld_core::dmabuf::WaylandShmBuffer {
                    bgra_pixels: vec![10, 10, 10, 255],
                },
            )),
            |_| {},
        )
        .expect("matching source");
        source
            .enqueue(session, commit(surface, metadata, lease), &control)
            .expect("submit frame");
        for interaction in [
            ToplevelInteractionRequestKind::Move,
            ToplevelInteractionRequestKind::End,
        ] {
            source
                .enqueue(
                    session,
                    ClientSurfaceEvent {
                        surface,
                        kind: ClientSurfaceEventKind::Interaction(interaction),
                    },
                    &control,
                )
                .expect("queue structural event");
        }

        let (token, frame, _) = fake.borrow().submitted[0].clone();
        complete(&fake, token, frame, 1);
        source.drain(&control).expect("complete frame and queue");
        source
            .finish_remote_commit(
                surface,
                ClientCommitRevision::new(5),
                LocalEncodedCommitOutcome::Applied,
                &control,
            )
            .expect("return destination credit");
        control.pump().expect("send control events");

        let packets = control_peer
            .drain::<LocalSourcePacket>()
            .expect("control events");
        assert_eq!(packets.len(), 3);
        assert!(matches!(
            packets[0].message.message,
            LocalSourceMessage::Surface(WireClientSurfaceEvent {
                kind: WireClientSurfaceEventKind::Commit(_),
                ..
            })
        ));
        assert!(matches!(
            packets[1].message.message,
            LocalSourceMessage::Surface(WireClientSurfaceEvent {
                kind: WireClientSurfaceEventKind::Interaction(ToplevelInteractionRequestKind::Move),
                ..
            })
        ));
        assert!(matches!(
            packets[2].message.message,
            LocalSourceMessage::Surface(WireClientSurfaceEvent {
                kind: WireClientSurfaceEventKind::Interaction(ToplevelInteractionRequestKind::End),
                ..
            })
        ));
    }

    #[test]
    fn cancelling_a_partial_batch_publishes_neither_media_nor_control() {
        let (control, control_peer) = LocalPacketConnection::pair().expect("control pair");
        let (media, media_peer) = LocalPacketConnection::pair().expect("media pair");
        let fake = Rc::new(RefCell::new(FakeEncoderState::default()));
        let mut source = EncodedSourceState::new(Box::new(FakeEncoder(fake.clone())), media);
        let source_id = ClientSourceId::new(1);
        let surface = ClientSurfaceId::new(ClientId::new(source_id, 2), 3);
        let session = HoistSessionId::new(4);
        let metadata = ClientBufferMetadata::new(Extent::new(1, 1), true);
        let buffers = [1, 2]
            .into_iter()
            .map(|local| {
                let buffer = ClientBufferLease::new(
                    ClientBufferId::new(source_id, local),
                    ClientBufferUseId::new(source_id, local),
                    metadata,
                    Rc::new(DirectClientBufferAccess::Shm(
                        weld_core::dmabuf::WaylandShmBuffer {
                            bgra_pixels: vec![10, 10, 10, 255],
                        },
                    )),
                    |_| {},
                )
                .expect("matching source");
                SurfaceBufferUpdate {
                    layer: SurfaceLayerId::new(local),
                    change: SurfaceBufferChange::Replaced { metadata, buffer },
                }
            })
            .collect();
        source
            .enqueue(session, commit_with_buffers(surface, 7, buffers), &control)
            .expect("submit multi-layer commit");

        let (first_token, first_frame, _) = fake.borrow().submitted[0].clone();
        complete(&fake, first_token, first_frame, 1);
        source.drain(&control).expect("complete first layer");
        let (second_token, second_frame, _) = fake.borrow().submitted[1].clone();
        source.cancel_surface(surface).expect("cancel batch");
        complete(&fake, second_token, second_frame, 2);
        source.drain(&control).expect("retire cancelled batch");
        control.pump().expect("pump control");
        source.media.pump().expect("pump media");

        assert!(
            control_peer
                .drain::<LocalSourcePacket>()
                .expect("control")
                .is_empty()
        );
        assert!(
            media_peer
                .drain::<LocalMediaPacket>()
                .expect("media")
                .is_empty()
        );
    }

    #[test]
    fn destination_credit_coalesces_one_surface_without_blocking_another() {
        let (control, _control_peer) = LocalPacketConnection::pair().expect("control pair");
        let (media, _media_peer) = LocalPacketConnection::pair().expect("media pair");
        let fake = Rc::new(RefCell::new(FakeEncoderState::default()));
        let mut source = EncodedSourceState::new(Box::new(FakeEncoder(fake.clone())), media);
        let source_id = ClientSourceId::new(1);
        let first_surface = ClientSurfaceId::new(ClientId::new(source_id, 2), 3);
        let second_surface = ClientSurfaceId::new(ClientId::new(source_id, 4), 5);
        let session = HoistSessionId::new(6);
        let metadata = ClientBufferMetadata::new(Extent::new(1, 1), true);

        source
            .enqueue(
                session,
                commit(
                    first_surface,
                    metadata,
                    shm_lease(source_id, 7, 10, metadata),
                ),
                &control,
            )
            .expect("submit first surface");
        let (token, frame, _) = fake.borrow().submitted[0].clone();
        complete(&fake, token, frame, 1);
        source.drain(&control).expect("complete first surface");

        for (local, pixel) in [(8, 20), (9, 30)] {
            source
                .enqueue(
                    session,
                    commit(
                        first_surface,
                        metadata,
                        shm_lease(source_id, local, pixel, metadata),
                    ),
                    &control,
                )
                .expect("coalesce credited surface");
        }
        assert_eq!(fake.borrow().submitted.len(), 1);

        source
            .enqueue(
                session,
                commit(
                    second_surface,
                    metadata,
                    shm_lease(source_id, 10, 40, metadata),
                ),
                &control,
            )
            .expect("submit independent surface");
        assert_eq!(fake.borrow().submitted.len(), 2);
        let (token, frame, _) = fake.borrow().submitted[1].clone();
        complete(&fake, token, frame, 2);
        source
            .drain(&control)
            .expect("complete independent surface");

        source
            .finish_remote_commit(
                ClientSurfaceId::new(ClientId::new(source_id, 99), 99),
                ClientCommitRevision::new(1),
                LocalEncodedCommitOutcome::Applied,
                &control,
            )
            .expect("ignore stale outcome");
        assert!(
            source
                .finish_remote_commit(
                    first_surface,
                    ClientCommitRevision::new(999),
                    LocalEncodedCommitOutcome::Applied,
                    &control,
                )
                .is_err()
        );
        source
            .finish_remote_commit(
                first_surface,
                ClientCommitRevision::new(7),
                LocalEncodedCommitOutcome::Applied,
                &control,
            )
            .expect("return first surface credit");

        assert_eq!(fake.borrow().submitted.len(), 3);
        assert_eq!(fake.borrow().submitted[2].2, vec![30, 30, 30, 255]);
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

    fn complete(
        fake: &Rc<RefCell<FakeEncoderState>>,
        token: u64,
        frame: MediaFrameId,
        payload: u8,
    ) {
        fake.borrow_mut().completions.push(LocalEncodeCompletion {
            token,
            result: Ok(EncodedAccessUnit {
                frame,
                codec: VideoCodec::H264,
                kind: EncodedFrameKind::Keyframe,
                timestamp_micros: token,
                payload: vec![payload],
            }),
        });
    }

    fn commit(
        surface: ClientSurfaceId,
        metadata: ClientBufferMetadata,
        buffer: ClientBufferLease,
    ) -> ClientSurfaceEvent {
        commit_with_buffers(
            surface,
            buffer.use_id().local(),
            vec![SurfaceBufferUpdate {
                layer: SurfaceLayerId::new(1),
                change: SurfaceBufferChange::Replaced { metadata, buffer },
            }],
        )
    }

    fn commit_with_buffers(
        surface: ClientSurfaceId,
        revision: u64,
        buffers: Vec<SurfaceBufferUpdate>,
    ) -> ClientSurfaceEvent {
        ClientSurfaceEvent {
            surface,
            kind: ClientSurfaceEventKind::Commit(ClientSurfaceCommit {
                revision: ClientCommitRevision::new(revision),
                mapped: true,
                root: None,
                window_geometry: None,
                overlays: Vec::new(),
                inputs: Vec::new(),
                buffers,
            }),
        }
    }
}
