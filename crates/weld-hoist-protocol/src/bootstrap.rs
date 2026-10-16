//! Shared source/destination negotiation records.

use crate::{ProtocolRevision, SurfaceMode};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum PeerRole {
    Source,
    Destination,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct BootstrapOffer {
    pub revision: ProtocolRevision,
    pub role: PeerRole,
    pub mode: SurfaceMode,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct BootstrapAnswer {
    pub revision: ProtocolRevision,
    pub role: PeerRole,
    pub rejection: Option<String>,
    pub stream_mode: weld_client::SurfaceStreamMode,
}

/// Revision acknowledgement for a locally transferred surface-mode offer.
#[derive(Debug, Deserialize, Serialize)]
pub struct BootstrapAcknowledgement {
    pub revision: ProtocolRevision,
    pub rejection: Option<String>,
}
