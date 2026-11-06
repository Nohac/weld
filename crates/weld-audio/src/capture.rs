#[path = "discovery.rs"]
mod discovery;
use crate::{
    format::format,
    worker::{Failure, Worker},
};
use anyhow::{Context, Result};
use pipewire::{
    core::CoreRc,
    keys,
    properties::properties,
    spa,
    stream::{StreamFlags, StreamListener, StreamRc, StreamState},
};
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex, OnceLock, Weak,
        atomic::{AtomicU64, Ordering},
        mpsc::{self, Receiver, SyncSender, TrySendError},
    },
    time::Instant,
};
use weld_media::audio::{AudioPacket, FRAME_BYTES, MAX_PACKET_BYTES, SAMPLE_RATE};

/// Select an output explicitly or the streams belonging to a local process.
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub enum CaptureTarget {
    Output(String),
    Process(u32),
}

static CAPTURES: OnceLock<Mutex<HashMap<CaptureTarget, Weak<SharedCapture>>>> = OnceLock::new();
struct Consumer {
    sender: SyncSender<AudioPacket>,
    origin: Instant,
    dropped: AtomicU64,
    wake: Arc<dyn Fn() + Send + Sync>,
}
struct SharedCapture {
    worker: Worker,
    consumers: Arc<Mutex<Vec<Weak<Consumer>>>>,
}

/// Independent bounded subscription to a shared application capture. Streaming
/// and recording reuse the same PipeWire streams, with separate clocks/queues.
pub struct Capture {
    receiver: Receiver<AudioPacket>,
    consumer: Arc<Consumer>,
    shared: Arc<SharedCapture>,
}
impl Capture {
    pub fn new(
        target: CaptureTarget,
        origin: Instant,
        wake: Arc<dyn Fn() + Send + Sync>,
    ) -> Result<Self> {
        let (sender, receiver) = mpsc::sync_channel(32);
        let consumer = Arc::new(Consumer {
            sender,
            origin,
            dropped: AtomicU64::new(0),
            wake,
        });
        let mut captures = CAPTURES
            .get_or_init(Default::default)
            .lock()
            .map_err(|_| anyhow::anyhow!("audio capture directory unavailable"))?;
        captures.retain(|_, capture| capture.strong_count() > 0);
        let shared = if let Some(shared) = captures.get(&target).and_then(Weak::upgrade) {
            shared
        } else {
            let consumers = Arc::new(Mutex::new(Vec::<Weak<Consumer>>::new()));
            let recipients = consumers.clone();
            let selected_target = target.clone();
            let worker = Worker::spawn(move |core, failure| {
                discovery::Discovery::new(core, selected_target, origin, recipients, failure)
            })?;
            let shared = Arc::new(SharedCapture { worker, consumers });
            captures.insert(target, Arc::downgrade(&shared));
            shared
        };
        shared
            .consumers
            .lock()
            .map_err(|_| anyhow::anyhow!("audio subscriptions unavailable"))?
            .push(Arc::downgrade(&consumer));
        Ok(Self {
            receiver,
            consumer,
            shared,
        })
    }
    pub fn drain(&self) -> Result<Vec<AudioPacket>> {
        self.shared.worker.failure.check()?;
        Ok(self.receiver.try_iter().take(32).collect())
    }
    pub fn dropped_packets(&self) -> u64 {
        self.consumer.dropped.load(Ordering::Relaxed)
    }
}

// Listener must be retired before its stream. All callbacks run on the worker's
// ordinary loop (RT_PROCESS is deliberately absent), never the real-time thread.
struct CaptureStream {
    _listener: StreamListener<CaptureState>,
    _stream: StreamRc,
}
struct CaptureState {
    serial: u64,
    origin: Instant,
    clock_origin_ns: i128,
    consumers: Arc<Mutex<Vec<Weak<Consumer>>>>,
    failure: Failure,
}
impl CaptureStream {
    fn new(core: &CoreRc, state: CaptureState, monitor: bool) -> Result<Self> {
        let stream = StreamRc::new(
            core.clone(),
            "Weld application capture",
            properties! {
                *keys::MEDIA_TYPE => "Audio", *keys::MEDIA_CATEGORY => "Capture", *keys::MEDIA_ROLE => "Screen",
                *keys::TARGET_OBJECT => state.serial.to_string(), *keys::STREAM_CAPTURE_SINK => monitor.to_string(),
                "node.dont-fallback" => "true", *keys::NODE_DONT_RECONNECT => "true", *keys::NODE_PASSIVE => "true",
                *keys::NODE_LATENCY => "960/48000",
            },
        )?;
        let listener = stream
            .add_local_listener_with_user_data(state)
            .state_changed(|_, state, _, new| {
                if let StreamState::Error(error) = new {
                    state.failure.set(error);
                    state.notify();
                }
            })
            .process(|stream, state| {
                let observed_ns = monotonic_ns();
                let timing = stream.time().ok();
                let delay_us = timing.as_ref().map_or(0, |time| {
                    let rate = time.rate();
                    if rate.denom == 0 {
                        return 0;
                    }
                    time.delay().max(0) as u64 * u64::from(rate.num) * 1_000_000
                        / u64::from(rate.denom)
                });
                let Some(mut buffer) = stream.dequeue_buffer() else {
                    return;
                };
                let header_time = buffer
                    .find_meta::<spa::buffer::meta::MetaHeader>()
                    .map(|header| i128::from(header.pts()))
                    .filter(|pts| *pts >= 0 && (*pts - observed_ns).abs() < 2_000_000_000);
                let Some(data) = buffer.datas_mut().first_mut() else {
                    return;
                };
                let offset = data.chunk().offset() as usize;
                let size = data.chunk().size() as usize;
                let Some(bytes) = data
                    .data()
                    .and_then(|bytes| bytes.get(offset..offset.saturating_add(size)))
                else {
                    return;
                };
                if !size.is_multiple_of(FRAME_BYTES) {
                    state.failure.set("unaligned PipeWire audio buffer");
                    return;
                }
                let duration = (size / FRAME_BYTES) as u64 * 1_000_000 / u64::from(SAMPLE_RATE);
                let graph_ns = timing
                    .as_ref()
                    .map_or(observed_ns, |time| i128::from(time.now()));
                let first_ns =
                    header_time.unwrap_or(graph_ns - i128::from(delay_us + duration) * 1000);
                let timestamp = ((first_ns - state.clock_origin_ns).max(0) / 1000) as u64;
                for (index, samples) in bytes.chunks(MAX_PACKET_BYTES).enumerate() {
                    if samples.is_empty() {
                        continue;
                    }
                    let packet = AudioPacket {
                        stream: state.serial,
                        timestamp_micros: timestamp
                            + (index * MAX_PACKET_BYTES / FRAME_BYTES) as u64 * 1_000_000
                                / u64::from(SAMPLE_RATE),
                        samples: samples.to_vec(),
                    };
                    state.publish(packet);
                }
            })
            .register()?;
        let format = format()?;
        let mut params = [spa::pod::Pod::from_bytes(&format).context("audio format missing")?];
        stream.connect(
            spa::utils::Direction::Input,
            None,
            StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS,
            &mut params,
        )?;
        Ok(Self {
            _listener: listener,
            _stream: stream,
        })
    }
}

fn belongs_to(mut candidate: u32, root: u32) -> bool {
    // Bounded ancestry lookup occurs only on stream discovery, never per sample.
    for _ in 0..64 {
        if candidate == root {
            return true;
        }
        if candidate <= 1 {
            break;
        }
        let Ok(status) = std::fs::read_to_string(format!("/proc/{candidate}/status")) else {
            break;
        };
        let Some(parent) = status.lines().find_map(|line| {
            line.strip_prefix("PPid:")
                .and_then(|v| v.trim().parse::<u32>().ok())
        }) else {
            break;
        };
        if parent == candidate {
            break;
        }
        candidate = parent;
    }
    false
}

impl CaptureState {
    fn notify(&self) {
        if let Ok(consumers) = self.consumers.lock() {
            for consumer in consumers.iter().filter_map(Weak::upgrade) {
                (consumer.wake)();
            }
        }
    }
    fn publish(&self, packet: AudioPacket) {
        let Ok(mut consumers) = self.consumers.lock() else {
            return;
        };
        consumers.retain(|consumer| consumer.strong_count() > 0);
        let time = self.origin + std::time::Duration::from_micros(packet.timestamp_micros);
        for consumer in consumers.iter().filter_map(Weak::upgrade) {
            // A subscription begins at its requested origin, never with older audio.
            if time < consumer.origin {
                continue;
            }
            let mut packet = packet.clone();
            packet.timestamp_micros = time.duration_since(consumer.origin).as_micros() as u64;
            match consumer.sender.try_send(packet) {
                Ok(()) => (consumer.wake)(),
                Err(TrySendError::Full(_)) => {
                    consumer.dropped.fetch_add(1, Ordering::Relaxed);
                }
                Err(TrySendError::Disconnected(_)) => {}
            }
        }
    }
}

fn monotonic_ns() -> i128 {
    let time = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
    i128::from(time.tv_sec) * 1_000_000_000 + i128::from(time.tv_nsec)
}
