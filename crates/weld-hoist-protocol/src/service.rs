//! Logical application-service lane records.
use crate::thumbnail::{ThumbnailReply, ThumbnailRequest};
use serde::{Deserialize, Serialize};

pub const LANE_MARKER: &[u8; 8] = b"weldsvc1";

#[derive(Serialize, Deserialize)]
pub enum ServiceRequest {
    Thumbnail(ThumbnailRequest),
}
#[derive(Serialize, Deserialize)]
pub enum ServiceReply {
    Thumbnail(ThumbnailReply),
}
