//! Peer report collection outcomes and per-connection request budget.
use crate::ReportBundle;
use serde::{Deserialize, Serialize};
use std::{
    fmt,
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum CollectionStatus {
    Collected,
    Offline,
    Denied,
    Unavailable,
    TimedOut,
    Busy,
    Failed,
}
impl fmt::Display for CollectionStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Collected => "matching peer report collected",
            Self::Offline => "peer is offline; reconnect to collect its report",
            Self::Denied => "peer denied session diagnostics access",
            Self::Unavailable => "peer report is unavailable or has expired",
            Self::TimedOut => "peer report request timed out; the peer may be unreachable or running an older build",
            Self::Busy => "diagnostic collection is busy; try again shortly",
            Self::Failed => "peer report request failed or returned invalid evidence",
        })
    }
}

#[derive(Serialize, Deserialize)]
pub struct DiagnosticCollection {
    pub bundle: ReportBundle,
    pub status: CollectionStatus,
}

#[derive(Default)]
pub struct RequestBudget {
    last: Option<Instant>,
}
impl RequestBudget {
    pub fn admit(&mut self, now: Instant) -> bool {
        if self
            .last
            .is_some_and(|last| now.saturating_duration_since(last) < Duration::from_secs(1))
        {
            return false;
        }
        self.last = Some(now);
        true
    }
}
