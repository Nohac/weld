use crate::{format::format, worker::Worker};
use anyhow::{Context, Result, ensure};
use pipewire::{
    properties::properties,
    spa,
    stream::{StreamFlags, StreamRc, StreamState},
};
use std::{
    collections::VecDeque,
    sync::mpsc::{self, SyncSender},
    time::{Duration, Instant},
};
use weld_media::audio::{AudioPacket, FRAME_BYTES, SAMPLE_RATE};

/// Bounded playback on the receiving desktop. Each instance represents one
/// audio stream; PipeWire mixes independent streams at the destination output.
pub struct Playback {
    sender: SyncSender<AudioPacket>,
    worker: Worker,
}
impl Playback {
    pub fn new(target: Option<String>) -> Result<Self> {
        let (sender, receiver) = mpsc::sync_channel::<AudioPacket>(16);
        let worker = Worker::with_wake(move |core, failure| {
            let mut props = properties! {
                "media.type" => "Audio", "media.category" => "Playback", "media.role" => "Screen",
                "node.name" => "weld.remote.audio", "node.latency" => "960/48000",
            };
            if let Some(target) = target {
                props.insert("target.object", target);
                props.insert("node.dont-fallback", "true");
                props.insert("node.dont-reconnect", "true");
            }
            let stream = StreamRc::new(core.clone(), "Weld remote audio", props)?;
            let mut queue = SampleQueue::default();
            let mut idle_frames = 0;
            let listener = stream
                .add_local_listener::<()>()
                .state_changed(move |_, _, _, new| {
                    if let StreamState::Error(error) = new {
                        failure.set(error);
                    }
                })
                .process(move |stream, _| {
                    for packet in receiver.try_iter().take(16) {
                        queue.push(packet);
                        idle_frames = 0;
                    }
                    let Some(mut buffer) = stream.dequeue_buffer() else {
                        return;
                    };
                    let requested = usize::try_from(buffer.requested()).unwrap_or(960);
                    let Some(data) = buffer.datas_mut().first_mut() else {
                        return;
                    };
                    let length = if let Some(bytes) = data.data() {
                        let frames = requested.max(1).min(bytes.len() / FRAME_BYTES);
                        let length = frames * FRAME_BYTES;
                        queue.render(&mut bytes[..length]);
                        length
                    } else {
                        0
                    };
                    let chunk = data.chunk_mut();
                    *chunk.offset_mut() = 0;
                    *chunk.stride_mut() = FRAME_BYTES as i32;
                    *chunk.size_mut() = length as u32;
                    drop(buffer);
                    if queue.packets.is_empty() {
                        idle_frames += length / FRAME_BYTES;
                        if idle_frames >= SAMPLE_RATE as usize / 5 {
                            let _ = stream.set_active(false);
                            queue.sample = None;
                            queue.waiting_since = None;
                            idle_frames = 0;
                        }
                    }
                })
                .register()?;
            let format = format()?;
            let mut params = [spa::pod::Pod::from_bytes(&format).context("audio format missing")?];
            stream.connect(
                spa::utils::Direction::Output,
                None,
                StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS | StreamFlags::INACTIVE,
                &mut params,
            )?;
            let activate = stream.clone();
            let wake: Box<dyn Fn()> = Box::new(move || {
                let _ = activate.set_active(true);
            });
            Ok(((listener, stream), Some(wake)))
        })?;
        Ok(Self { sender, worker })
    }
    /// Queue pressure drops this packet. Source timestamps preserve discontinuities.
    pub fn push(&self, packet: AudioPacket) -> Result<bool> {
        ensure!(packet.is_valid(), "invalid audio packet");
        self.worker.failure.check()?;
        match self.sender.try_send(packet) {
            Ok(()) => {
                self.worker.wake();
                Ok(true)
            }
            Err(mpsc::TrySendError::Full(_)) => Ok(false),
            Err(mpsc::TrySendError::Disconnected(_)) => anyhow::bail!("audio playback stopped"),
        }
    }
}

#[derive(Default)]
struct SampleQueue {
    packets: VecDeque<AudioPacket>,
    sample: Option<u64>,
    waiting_since: Option<Instant>,
}
impl SampleQueue {
    fn push(&mut self, packet: AudioPacket) {
        if !packet.is_valid() {
            return;
        }
        // Keep at most 160 ms of queued PCM. Resume near the live edge after stalls.
        self.waiting_since.get_or_insert_with(Instant::now);
        while self
            .packets
            .iter()
            .map(|packet| packet.samples.len())
            .sum::<usize>()
            + packet.samples.len()
            > SAMPLE_RATE as usize * FRAME_BYTES * 160 / 1000
        {
            if self.packets.pop_front().is_none() {
                break;
            }
            self.sample = None;
        }
        if self.packets.len() >= 128 {
            self.packets.pop_front();
            self.sample = None;
        }
        self.packets.push_back(packet);
        self.packets
            .make_contiguous()
            .sort_by_key(|packet| packet.timestamp_micros);
    }
    fn render(&mut self, bytes: &mut [u8]) {
        self.render_at(bytes, Instant::now());
    }
    fn render_at(&mut self, bytes: &mut [u8], now: Instant) {
        bytes.fill(0);
        if self.sample.is_none()
            && self
                .packets
                .iter()
                .map(|packet| packet.samples.len())
                .sum::<usize>()
                < 1920 * FRAME_BYTES
            && self.waiting_since.is_none_or(|started| {
                now.saturating_duration_since(started) < Duration::from_millis(40)
            })
        {
            return;
        }
        let total_frames = bytes.len() / FRAME_BYTES;
        for (index, frame) in bytes
            .as_chunks_mut::<FRAME_BYTES>()
            .0
            .iter_mut()
            .enumerate()
        {
            loop {
                let Some(packet) = self.packets.front() else {
                    if let Some(sample) = &mut self.sample {
                        *sample += (total_frames - index) as u64;
                    }
                    return;
                };
                let start =
                    (packet.timestamp_micros * u64::from(SAMPLE_RATE) + 500_000) / 1_000_000;
                let sample = *self.sample.get_or_insert(start);
                if sample < start {
                    self.sample = Some(sample + 1);
                    break;
                }
                let offset = (sample - start) as usize * FRAME_BYTES;
                if let Some(source) = packet
                    .samples
                    .get(offset..offset.saturating_add(FRAME_BYTES))
                {
                    frame.copy_from_slice(source);
                    self.sample = Some(sample + 1);
                    break;
                }
                self.packets.pop_front();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn short_sounds_start_after_bounded_wait_and_underruns_keep_time() {
        let mut queue = SampleQueue::default();
        let now = Instant::now();
        queue.push(AudioPacket {
            stream: 1,
            timestamp_micros: 0,
            samples: vec![7; 4],
        });
        queue.waiting_since = Some(now);
        let mut bytes = [0; 12];
        queue.render_at(&mut bytes, now);
        assert_eq!(bytes, [0; 12]);
        queue.render_at(&mut bytes, now + Duration::from_millis(40));
        assert_eq!(&bytes[..4], &[7; 4]);
        assert_eq!(queue.sample, Some(3));
    }
    #[test]
    fn timestamps_preserve_gaps_and_skip_overlapping_samples() {
        let mut queue = SampleQueue::default();
        queue.push(AudioPacket {
            stream: 1,
            timestamp_micros: 0,
            samples: vec![1; 4],
        });
        queue.push(AudioPacket {
            stream: 1,
            timestamp_micros: 42,
            samples: vec![2; 4],
        });
        queue.sample = Some(0);
        let mut bytes = [0; 12];
        queue.render(&mut bytes);
        assert_eq!(bytes, [1, 1, 1, 1, 0, 0, 0, 0, 2, 2, 2, 2]);
    }
}
