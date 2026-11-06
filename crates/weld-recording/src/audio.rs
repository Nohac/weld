//! Disk-backed mixing for export; live recording retains original stream packets.
use crate::Clip;
use anyhow::{Context, Result, ensure};
use ffmpeg_next::{self as ffmpeg, codec, format};
use std::{collections::HashMap, fs::File, os::unix::fs::FileExt};
use weld_media::audio::{CHANNELS, FRAME_BYTES, SAMPLE_RATE};

pub(crate) struct AudioMux {
    file: File,
    next_frame: u64,
    frames: u64,
    stream: usize,
}
impl AudioMux {
    pub fn new(
        clip: &Clip,
        output: &mut format::context::Output,
        validate: &impl Fn() -> Result<()>,
    ) -> Result<Option<Self>> {
        if clip.audio.is_empty() {
            return Ok(None);
        }
        let file = tempfile::tempfile().context("creating mixed recording audio")?;
        let frames = u64::from(clip.info.duration_ms) * u64::from(SAMPLE_RATE) / 1000;
        // Accumulate in 32-bit samples so clipping happens once, after all streams.
        file.set_len(frames * FRAME_BYTES as u64 * 2)?;
        let mut ends = HashMap::<u64, u64>::new();
        for packet in &clip.audio {
            validate()?;
            let bytes = packet.bytes()?;
            ensure!(
                bytes.len().is_multiple_of(FRAME_BYTES),
                "unaligned recorded audio"
            );
            let original = packet
                .timestamp_micros
                .saturating_mul(u64::from(SAMPLE_RATE))
                / 1_000_000;
            let start = original.max(ends.get(&packet.stream).copied().unwrap_or(0));
            let end = (original + (bytes.len() / FRAME_BYTES) as u64).min(frames);
            if start >= end {
                continue;
            }
            let skip = (start - original) as usize * FRAME_BYTES;
            let length = (end - start) as usize * FRAME_BYTES;
            let mut mixed = vec![0; length * 2];
            file.read_exact_at(&mut mixed, start * FRAME_BYTES as u64 * 2)?;
            for (existing, incoming) in mixed
                .as_chunks_mut::<4>()
                .0
                .iter_mut()
                .zip(bytes[skip..skip + length].as_chunks::<2>().0.iter())
            {
                let current = i32::from_le_bytes(*existing);
                let added = i16::from_le_bytes([incoming[0], incoming[1]]);
                existing.copy_from_slice(&current.saturating_add(i32::from(added)).to_le_bytes());
            }
            file.write_all_at(&mixed, start * FRAME_BYTES as u64 * 2)?;
            ends.insert(packet.stream, end);
        }
        let codec =
            ffmpeg::encoder::find(codec::Id::PCM_S16LE).context("PCM audio codec unavailable")?;
        let mut encoder = codec::Context::new_with_codec(codec).encoder().audio()?;
        encoder.set_rate(SAMPLE_RATE as i32);
        encoder.set_channel_layout(ffmpeg::ChannelLayout::STEREO);
        encoder.set_format(ffmpeg::format::Sample::I16(
            ffmpeg::format::sample::Type::Packed,
        ));
        encoder.set_time_base((1, SAMPLE_RATE as i32));
        encoder.set_bit_rate(SAMPLE_RATE as usize * CHANNELS as usize * 16);
        let encoder = encoder.open_as(codec)?;
        let mut stream = output.add_stream(codec)?;
        stream.set_parameters(codec::Parameters::from(&encoder));
        stream.set_time_base((1, SAMPLE_RATE as i32));
        Ok(Some(Self {
            file,
            frames,
            next_frame: 0,
            stream: stream.index(),
        }))
    }
    pub fn write_until(
        &mut self,
        time_us: u64,
        output: &mut format::context::Output,
        validate: &impl Fn() -> Result<()>,
    ) -> Result<()> {
        let until = (time_us.saturating_mul(u64::from(SAMPLE_RATE)) / 1_000_000).min(self.frames);
        let time_base = output
            .stream(self.stream)
            .context("recording audio track missing")?
            .time_base();
        while self.next_frame < until {
            validate()?;
            let frames = (until - self.next_frame).min(960);
            let mut mixed = vec![0; frames as usize * FRAME_BYTES * 2];
            self.file
                .read_exact_at(&mut mixed, self.next_frame * FRAME_BYTES as u64 * 2)?;
            let bytes: Vec<_> = mixed
                .as_chunks::<4>()
                .0
                .iter()
                .flat_map(|sample| {
                    (i32::from_le_bytes(*sample).clamp(i32::from(i16::MIN), i32::from(i16::MAX))
                        as i16)
                        .to_le_bytes()
                })
                .collect();
            let mut packet = ffmpeg::Packet::copy(&bytes);
            packet.set_stream(self.stream);
            packet.set_pts(Some(self.next_frame as i64));
            packet.set_dts(Some(self.next_frame as i64));
            packet.set_duration(frames as i64);
            packet.rescale_ts((1, SAMPLE_RATE as i32), time_base);
            packet.write_interleaved(output)?;
            self.next_frame += frames;
        }
        Ok(())
    }
}
