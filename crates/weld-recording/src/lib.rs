//! Bounded encoded recordings and off-thread inspection of retained clips.

mod engine;
mod inspect;
pub use engine::Recorder;
pub use inspect::{FrameOptions, Image, SheetOptions, Tile, Video, frame, sheet, video};

use anyhow::{Result, ensure};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use weld_media::{EncodedAccessUnit, VideoCodec};

pub const MAX_BYTES: usize = 32 * 1024 * 1024;
pub const MAX_FRAMES: usize = 2400;

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Codec {
    #[default]
    Av1,
    H264,
}
impl From<Codec> for VideoCodec {
    fn from(value: Codec) -> Self {
        match value {
            Codec::Av1 => Self::Av1,
            Codec::H264 => Self::H264,
        }
    }
}

/// Window-local logical crop, retained as the coordinate origin of the recording.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, JsonSchema)]
pub struct Region {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}
impl Region {
    pub fn validate(&self, width: f32, height: f32) -> Result<()> {
        ensure!(
            [self.x, self.y, self.width, self.height]
                .iter()
                .all(|v| v.is_finite())
                && self.x >= 0.
                && self.y >= 0.
                && self.width > 0.
                && self.height > 0.
                && self.x + self.width <= width
                && self.y + self.height <= height,
            "region must fit inside the recorded logical viewport"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct Options {
    pub duration_ms: u32,
    pub region: Option<Region>,
    #[serde(default)]
    pub codec: Codec,
    pub after_sequence: Option<String>,
}
impl Options {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            (1..=10_000).contains(&self.duration_ms),
            "duration_ms must be 1..10000"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct FrameInfo {
    pub index: u32,
    pub time_us: u64,
    pub revision: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Recording,
    Complete,
    Failed,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct Info {
    pub id: String,
    pub status: Status,
    pub duration_ms: u32,
    pub codec: Codec,
    pub region: Region,
    pub width: u32,
    pub height: u32,
    pub submitted_frames: u32,
    /// Final count after the encoder drains; pending work is unknown while recording.
    pub dropped_frames: Option<u32>,
    pub frame_count: u32,
    pub frames: Vec<FrameInfo>,
    pub bytes: usize,
    pub error: Option<String>,
    pub expires_after_idle_ms: u32,
}

pub struct EncodedFrame {
    pub info: FrameInfo,
    pub unit: EncodedAccessUnit,
}
pub struct Clip {
    pub info: Info,
    pub frames: Vec<EncodedFrame>,
}

impl std::fmt::Display for Info {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} {:?}: {} frames, {} dropped, {}x{}",
            self.id,
            self.status,
            self.frame_count,
            self.dropped_frames
                .map_or_else(|| "pending".to_owned(), |count| count.to_string()),
            self.width,
            self.height
        )
    }
}

impl std::str::FromStr for Options {
    type Err = serde_json::Error;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        serde_json::from_str(value)
    }
}
impl std::str::FromStr for FrameOptions {
    type Err = serde_json::Error;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        serde_json::from_str(value)
    }
}
impl std::str::FromStr for SheetOptions {
    type Err = serde_json::Error;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        serde_json::from_str(value)
    }
}
