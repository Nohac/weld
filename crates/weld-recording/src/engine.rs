//! Recording sink over the same encoded source port used by streaming.

use crate::{
    Clip, FrameInfo, IDLE_RETENTION_MS, Info, MAX_BYTES, MAX_FRAMES, Options, Region, Status,
    storage::Storage,
};
use anyhow::{Context, Result, ensure};
use std::{
    cell::RefCell,
    collections::HashMap,
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};
use weld_client::{
    ClientBufferLease, ClientBufferMetadata, ClientCommitRevision, ClientSurfaceCommit,
    ClientSurfaceEvent, ClientSurfaceEventKind, ClientSurfaceId, ComposedBuffer, Extent,
    LogicalSize, SurfaceBufferChange, WireClientSurfaceEventKind, WireSurfaceBufferChange,
};
use weld_hoist_core::{HoistPortResult, HoistSessionId, HoistSourcePort, SourcePortCommand};
use weld_hoist_encoded::{
    EncodeBackend, EncodedSourcePort, EncodedSourceTransport, SendStatus, SourceTransportPacket,
};
use weld_hoist_protocol::{DestinationEnvelope, SourceMessage};
use weld_media::{EncodedAccessUnit, MediaFrameId};

struct Collected {
    metadata: HashMap<u64, FrameInfo>,
    packets: HashMap<MediaFrameId, EncodedAccessUnit>,
    storage: Storage,
    bytes: usize,
}
#[derive(Clone)]
struct Sink(Rc<RefCell<Collected>>);
impl EncodedSourceTransport for Sink {
    fn try_send(
        &self,
        packet: SourceTransportPacket,
    ) -> HoistPortResult<SendStatus<SourceTransportPacket>> {
        let mut data = self.0.borrow_mut();
        if matches!(&packet, SourceTransportPacket::Media(_))
            && !data.storage.has_capacity(data.packets.len())
        {
            return Ok(SendStatus::Busy(packet));
        }
        match packet {
            SourceTransportPacket::Media(media) => {
                if data.bytes.saturating_add(media.access_unit.payload.len()) > MAX_BYTES {
                    return Err("recording exceeds the 2 GiB encoded budget".into());
                }
                if media.access_unit.payload.len() > 32 * 1024 * 1024 {
                    return Err("recording packet exceeds 32 MiB".into());
                }
                data.bytes += media.access_unit.payload.len();
                data.packets
                    .insert(media.access_unit.frame, media.access_unit);
            }
            SourceTransportPacket::Control(envelope) => {
                if let SourceMessage::Surface(event) = envelope.message
                    && let WireClientSurfaceEventKind::Commit(commit) = event.kind
                {
                    for update in commit.buffers {
                        if let WireSurfaceBufferChange::Replaced { buffer, .. } = update.change {
                            let unit = data
                                .packets
                                .remove(&buffer.frame)
                                .ok_or("recording media missing")?;
                            let info = data
                                .metadata
                                .remove(&commit.revision.raw())
                                .ok_or("recording timestamp missing")?;
                            // The source emits commits in order. Earlier revisions were
                            // coalesced and will never produce another packet.
                            data.metadata
                                .retain(|revision, _| *revision > commit.revision.raw());
                            data.storage.push(info, unit).map_err(
                                |error| -> Box<dyn std::error::Error + Send + Sync> {
                                    error.into()
                                },
                            )?;
                        }
                    }
                }
            }
        }
        Ok(SendStatus::Sent)
    }
    fn media_headroom(&self) -> bool {
        let data = self.0.borrow();
        data.storage.has_capacity(data.packets.len())
    }
    fn drain(&self) -> HoistPortResult<Vec<DestinationEnvelope>> {
        Ok(Vec::new())
    }
    fn disconnect(&self) {}
}

pub struct Recorder {
    port: EncodedSourcePort<Sink>,
    collected: Rc<RefCell<Collected>>,
    surface: ClientSurfaceId,
    options: Options,
    info: Info,
    started: Instant,
    last_cursor: Option<weld_client::CompositionCursor>,
}

impl Recorder {
    /// Monotonic origin shared with a host-scheduled input sequence.
    pub fn started(&self) -> Instant {
        self.started
    }

    pub fn new(
        id: String,
        surface: ClientSurfaceId,
        initial: ClientSurfaceCommit,
        options: Options,
        backend: Box<dyn EncodeBackend>,
        now: Instant,
        wake: Arc<dyn Fn() + Send + Sync>,
    ) -> Result<Self> {
        options.validate()?;
        let scene = scene(&initial)?;
        let region = options.region.unwrap_or(Region {
            x: 0.,
            y: 0.,
            width: scene.logical_size.width,
            height: scene.logical_size.height,
        });
        region.validate(scene.logical_size.width, scene.logical_size.height)?;
        let width = (region.width * scene.extent.width as f32 / scene.logical_size.width)
            .round()
            .max(1.) as u32;
        let height = (region.height * scene.extent.height as f32 / scene.logical_size.height)
            .round()
            .max(1.) as u32;
        ensure!(
            u64::from(width) * u64::from(height) <= 16_777_216,
            "recording exceeds the 16 megapixel frame budget"
        );
        let collected = Rc::new(RefCell::new(Collected {
            metadata: HashMap::new(),
            packets: HashMap::new(),
            bytes: 0,
            storage: Storage::new(wake)?,
        }));
        let port = EncodedSourcePort::composed_input(Sink(collected.clone()), backend)?;
        let info = Info {
            include_cursor: options.include_cursor,
            id,
            status: Status::Recording,
            duration_ms: options.duration_ms,
            codec: options.codec,
            region,
            width,
            height,
            submitted_frames: 0,
            dropped_frames: None,
            frame_count: 0,
            frames: Vec::new(),
            event_count: 0,
            events: Vec::new(),
            bytes: 0,
            error: None,
            expires_after_idle_ms: IDLE_RETENTION_MS,
        };
        let mut recorder = Self {
            port,
            collected,
            surface,
            options,
            info,
            started: now,
            last_cursor: None,
        };
        recorder.submit(&initial, now)?;
        recorder.tick(now)?;
        Ok(recorder)
    }

    pub fn set_include_cursor(&mut self, include: bool, now: Instant) -> Result<()> {
        ensure!(
            now < self.end(),
            "cursor visibility can only change while recording"
        );
        self.options.include_cursor = include;
        self.info.include_cursor = include;
        Ok(())
    }

    pub fn observe_cursor(&mut self, frame: &ClientSurfaceCommit, now: Instant) -> Result<()> {
        let cursor = self.visible_cursor(scene(frame)?);
        if cursor == self.last_cursor {
            return Ok(());
        }
        self.observe(frame, now)
    }

    fn visible_cursor(&self, scene: &ComposedBuffer) -> Option<weld_client::CompositionCursor> {
        scene.cursor.clone().filter(|cursor| {
            self.options.include_cursor
                && !matches!(cursor.image, weld_client::ClientCursor::Hidden)
        })
    }

    pub fn observe(&mut self, frame: &ClientSurfaceCommit, now: Instant) -> Result<()> {
        if now > self.end() {
            return Ok(());
        }
        self.submit(frame, now)?;
        Ok(())
    }

    fn end(&self) -> Instant {
        self.started + Duration::from_millis(u64::from(self.options.duration_ms))
    }
    pub fn next_deadline(&self) -> Option<Instant> {
        let deadline = if Instant::now() < self.end() {
            self.end()
        } else {
            self.end() + Duration::from_secs(5)
        };
        self.port
            .next_deadline()
            .into_iter()
            .chain(Some(deadline))
            .min()
    }

    pub fn tick(&mut self, now: Instant) -> Result<bool> {
        ensure!(
            now <= self.end() + Duration::from_secs(5),
            "recording encoder drain timed out"
        );
        self.port
            .poll()
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        self.port
            .progress_after_destination()
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        let storage_idle = self.collected.borrow().storage.is_idle()?;
        Ok(now >= self.end() && self.port.is_idle() && storage_idle)
    }

    fn submit(&mut self, source_frame: &ClientSurfaceCommit, now: Instant) -> Result<()> {
        ensure!(
            (self.info.submitted_frames as usize) < MAX_FRAMES,
            "recording frame budget exhausted"
        );
        let source = scene(source_frame)?;
        let mut frame = source_frame.clone().into_state();
        self.info
            .region
            .validate(source.logical_size.width, source.logical_size.height)?;
        let mut cropped = source.clone();
        cropped.cursor = self.visible_cursor(source);
        self.last_cursor = cropped.cursor.clone();
        cropped.extent = Extent::new(self.info.width, self.info.height);
        cropped.logical_size = LogicalSize {
            width: self.info.region.width,
            height: self.info.region.height,
        };
        if let Some(cursor) = &mut cropped.cursor {
            cursor.position.x -= self.info.region.x;
            cursor.position.y -= self.info.region.y;
        }
        for layer in &mut cropped.layers {
            layer.placement.position.x -= self.info.region.x;
            layer.placement.position.y -= self.info.region.y;
        }
        let original_revision = frame.revision.raw();
        self.info.submitted_frames += 1;
        frame.revision = ClientCommitRevision::new(u64::from(self.info.submitted_frames));
        self.collected.borrow_mut().metadata.insert(
            frame.revision.raw(),
            FrameInfo {
                index: 0,
                time_us: now.saturating_duration_since(self.started).as_micros() as u64,
                revision: original_revision.to_string(),
            },
        );
        for update in &mut frame.buffers {
            if let SurfaceBufferChange::Replaced { buffer, metadata } = &mut update.change {
                *metadata = ClientBufferMetadata::new(cropped.extent, true);
                *buffer = ClientBufferLease::new(
                    buffer.buffer(),
                    buffer.use_id(),
                    *metadata,
                    Rc::new(cropped.clone()),
                    |_| {},
                )?;
            }
        }
        self.port
            .submit(SourcePortCommand::Surface {
                session: HoistSessionId::new(1),
                event: ClientSurfaceEvent {
                    surface: self.surface,
                    kind: ClientSurfaceEventKind::Commit(frame.into()),
                },
            })
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        Ok(())
    }

    pub fn observe_input(
        &mut self,
        index: u32,
        action: &serde_json::Value,
        now: Instant,
        complete: bool,
        revision: Option<String>,
    ) -> Result<()> {
        self.collected.borrow_mut().storage.note_event(
            index,
            action,
            now.saturating_duration_since(self.started).as_micros() as u64,
            complete,
            revision,
        )
    }

    pub fn timeline(&self) -> crate::Timeline {
        self.collected.borrow().storage.timeline()
    }

    pub fn info(&self) -> Info {
        let mut info = self.info.clone();
        let data = self.collected.borrow();
        info.bytes = data.bytes;
        info.event_count = data.storage.event_count();
        info.frame_count = data.storage.frame_count() as u32;
        info
    }

    pub fn finish(self) -> Result<Clip> {
        let mut info = self.info();
        info.status = Status::Complete;
        info.dropped_frames = Some(info.submitted_frames.saturating_sub(info.frame_count));
        info.events = self.timeline().events()?;
        let frames = self.collected.borrow_mut().storage.finish()?;
        info.frames = frames.iter().map(|frame| frame.info.clone()).collect();
        Ok(Clip { info, frames })
    }
}

fn scene(frame: &ClientSurfaceCommit) -> Result<&ComposedBuffer> {
    ensure!(frame.mapped, "recorded window is unmapped");
    frame
        .buffers
        .iter()
        .find_map(|update| match &update.change {
            SurfaceBufferChange::Replaced { buffer, .. } => buffer.access::<ComposedBuffer>(),
            _ => None,
        })
        .context("recorded window has no composed frame")
}
