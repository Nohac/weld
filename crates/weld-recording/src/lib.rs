//! Bounded encoded recordings and off-thread inspection of retained clips.

mod annotations;
pub use annotations::Gutter;
mod engine;
mod inspect;
mod storage;
pub use engine::Recorder;
pub use inspect::{
    EventAnchor, EventSelection, FrameOptions, Image, SheetOptions, Tile, Video, VideoFile, frame,
    sheet, video,
};
pub use storage::{EncodedFrame, Packet, Timeline};

use anyhow::{Result, ensure};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use weld_media::VideoCodec;

pub const MAX_DURATION_MS: u32 = 600_000;
pub const IDLE_RETENTION_MS: u32 = 1_800_000;
/// Encoded storage budget, with headroom over ten minutes at 16 Mbit/s.
pub const MAX_BYTES: usize = 2 * 1024 * 1024 * 1024;
/// Up to 240 observed updates per second plus the initial frame.
pub const MAX_FRAMES: usize = MAX_DURATION_MS as usize * 240 / 1000 + 1;

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
    /// Draw the controller cursor; off by default. Client-requested hiding is always respected.
    #[serde(default)]
    pub include_cursor: bool,
    pub duration_ms: u32,
    pub region: Option<Region>,
    #[serde(default)]
    pub codec: Codec,
    pub after_sequence: Option<String>,
}
impl Options {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            (1..=MAX_DURATION_MS).contains(&self.duration_ms),
            "duration_ms must be 1..{MAX_DURATION_MS}"
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

/// A delivered input row, sampled on the recording clock. Partial rows remain explicit.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct RecordedEvent {
    pub index: u32,
    pub action: serde_json::Value,
    pub first_time_us: u64,
    /// Last acknowledgement (completion time when complete is true).
    pub time_us: u64,
    pub complete: bool,
    pub revision: Option<String>,
}

impl RecordedEvent {
    pub fn kind(&self) -> &'static str {
        match self.action.get(0).and_then(serde_json::Value::as_str) {
            Some("m") => "move",
            Some("p") => "key press",
            Some("d") => "key down",
            Some("u") => "key up",
            Some("t") => "text",
            Some("b") if self.action.get(2).and_then(serde_json::Value::as_bool) == Some(true) => {
                "button down"
            }
            Some("b") => "button up",
            Some("click") => "click",
            Some("wheel") => "wheel",
            Some("scroll") => "scroll",
            _ => "input",
        }
    }
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
    /// Current cursor policy; already encoded frames are unaffected by later changes.
    pub include_cursor: bool,
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
    pub event_count: u32,
    pub events: Vec<RecordedEvent>,
    pub bytes: usize,
    pub error: Option<String>,
    pub expires_after_idle_ms: u32,
}

impl Info {
    /// Copy counters without allocating a potentially ten-minute frame timeline.
    pub fn summary(&self) -> Self {
        Self {
            include_cursor: self.include_cursor,
            id: self.id.clone(),
            status: self.status.clone(),
            duration_ms: self.duration_ms,
            codec: self.codec,
            region: self.region,
            width: self.width,
            height: self.height,
            submitted_frames: self.submitted_frames,
            dropped_frames: self.dropped_frames,
            frame_count: self.frame_count,
            frames: Vec::new(),
            event_count: self.event_count,
            events: Vec::new(),
            bytes: self.bytes,
            error: self.error.clone(),
            expires_after_idle_ms: self.expires_after_idle_ms,
        }
    }
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
