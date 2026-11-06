//! Bounded audio packets shared by live playback and recording.
//!
//! The initial wire format is interleaved stereo signed 16-bit little-endian PCM
//! at 48 kHz. Timestamps identify the first sample on the sender's media clock.

pub const SAMPLE_RATE: u32 = 48_000;
pub const CHANNELS: u32 = 2;
pub const FRAME_BYTES: usize = 4;
pub const MAX_PACKET_BYTES: usize = 32_768;

#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AudioPacket {
    /// Source-local stream identity; separate application streams remain distinct.
    pub stream: u64,
    pub timestamp_micros: u64,
    pub samples: Vec<u8>,
}

impl AudioPacket {
    pub fn is_valid(&self) -> bool {
        self.timestamp_micros <= u64::MAX / u64::from(SAMPLE_RATE) - 1_000_000
            && !self.samples.is_empty()
            && self.samples.len() <= MAX_PACKET_BYTES
            && self.samples.len().is_multiple_of(FRAME_BYTES)
    }

    pub fn duration_micros(&self) -> u64 {
        (self.samples.len() / FRAME_BYTES) as u64 * 1_000_000 / u64::from(SAMPLE_RATE)
    }
}
