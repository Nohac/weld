//! Device enrollment, application catalogue and session-control records.

use serde::{Deserialize, Serialize};
use weld_client::ClientSurfaceId;
use weld_media::VideoCodec;

/// Capabilities approved locally for a paired endpoint.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DevicePermissions {
    pub browse: bool,
    pub hoist: bool,
    #[serde(default)]
    pub diagnostics: bool,
    /// Permission to fetch application pixels while browsing the catalogue.
    #[serde(default)]
    pub thumbnails: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PairedDevice {
    pub identity: String,
    pub name: String,
    pub permissions: DevicePermissions,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PairingCandidate {
    pub identity: String,
    pub name: String,
    pub verification: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApplicationInfo {
    pub window: u64,
    pub surface: ClientSurfaceId,
    pub title: String,
    pub app_id: String,
    pub available: bool,
    pub hoisted_here: bool,
}

#[derive(Serialize, Deserialize)]
pub struct PairRequest {
    pub token: [u8; 32],
    pub name: String,
}
#[derive(Serialize, Deserialize)]
pub enum PairReply {
    Verify(String),
    Approved(DevicePermissions),
}

#[derive(Serialize, Deserialize)]
pub enum DeviceRequest {
    List,
    Hoist(u64),
    Release,
    Diagnostics(Box<weld_diagnostics::Report>),
    Subscribe,
}
#[derive(Serialize, Deserialize)]
pub enum DeviceReply {
    Catalogue(Vec<ApplicationInfo>),
    Accepted(bool),
    Diagnostics(Option<Box<weld_diagnostics::Report>>),
}
#[derive(Serialize, Deserialize)]
pub struct DeviceHello {
    pub preferences: crate::presentation::ReceiverPreferences,
}
#[derive(Serialize, Deserialize)]
pub struct DeviceWelcome {
    pub codec: VideoCodec,
}
