//! Receiver preferences exchanged before admitting source surfaces.

use serde::{Deserialize, Serialize};
use weld_client::SurfaceStreamMode;
use weld_media::VideoCodec;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IrohReceiverPreferences {
    pub codecs: Vec<VideoCodec>,
    pub stream_mode: SurfaceStreamMode,
}

impl From<Vec<VideoCodec>> for IrohReceiverPreferences {
    fn from(codecs: Vec<VideoCodec>) -> Self {
        Self {
            codecs,
            stream_mode: SurfaceStreamMode::Independent,
        }
    }
}

pub(crate) struct SourcePresentation {
    pub codec: VideoCodec,
    pub stream_mode: SurfaceStreamMode,
}
