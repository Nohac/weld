use serde::{Deserialize, Serialize};
use std::{
    fmt,
    io::{self, Read, Write},
    str::FromStr,
};

pub const SCHEMA_VERSION: u16 = 1;
pub const MAX_EVENTS: usize = 256;
pub const MAX_EXPORT_BYTES: u64 = 512 * 1024;

/// Public, per-connection correlation identifier, independent of device names.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct SessionId(pub [u8; 16]);
impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}
impl From<SessionId> for String {
    fn from(id: SessionId) -> Self {
        id.to_string()
    }
}
impl TryFrom<String> for SessionId {
    type Error = &'static str;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}
impl FromStr for SessionId {
    type Err = &'static str;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.len() != 32 || !value.is_ascii() {
            return Err("expected 32 hexadecimal session characters");
        }
        let mut bytes = [0; 16];
        for (output, pair) in bytes.iter_mut().zip(value.as_bytes().chunks_exact(2)) {
            let digit = |b: u8| (b as char).to_digit(16).map(|n| n as u8);
            *output = digit(pair[0]).ok_or("invalid session hex")? * 16
                + digit(pair[1]).ok_or("invalid session hex")?;
        }
        Ok(Self(bytes))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum Endpoint {
    Source,
    Receiver,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum PathKind {
    Ipv4,
    Ipv6,
    Relay,
    Adb,
    Other,
    Unavailable,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum Operation {
    SessionWrite,
    SessionRead,
    Control,
    Media,
    Connection,
    Encode,
    Decode,
    Presentation,
    Shutdown,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum Cause {
    Timeout,
    PeerClosed,
    TransportLost,
    ProtocolOrIo,
    Codec,
    LocalShutdown,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum Stage {
    TransportSend,
    Encode,
    Receive,
    Decode,
    Presentation,
}

/// Cumulative counters belong to one path epoch. A path change resets comparison.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct NetworkSample {
    pub epoch: u64,
    pub rtt_us: u64,
    pub congestion_window_bytes: u64,
    pub sent_bytes: u64,
    pub received_bytes: u64,
    pub lost_packets: u64,
    pub congestion_events: u64,
}

/// Stage-local interval observations. Durations may overlap across stages and
/// must never be summed into an end-to-end latency estimate.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct StageSample {
    pub interval_us: u64,
    pub completed: u64,
    pub pending: u64,
    pub oldest_pending_us: u64,
    pub work_max_us: u64,
    pub superseded: u64,
    pub stale: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum Observation {
    Path(PathKind),
    Network(NetworkSample),
    Stage { stage: Stage, sample: StageSample },
    Failure { operation: Operation, cause: Cause },
    Ended,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub sequence: u64,
    pub elapsed_us: u64,
    pub observation: Observation,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Report {
    pub schema: u16,
    pub session: SessionId,
    pub endpoint: Endpoint,
    pub started_unix_ms: u64,
    pub ended: bool,
    pub overwritten_events: u64,
    pub first_failure: Option<Event>,
    pub events: Vec<Event>,
}
impl Report {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.schema != SCHEMA_VERSION {
            return Err("unsupported diagnostics schema");
        }
        if self.events.len() > MAX_EVENTS {
            return Err("diagnostic event limit exceeded");
        }
        if self
            .first_failure
            .as_ref()
            .is_some_and(|event| !matches!(event.observation, Observation::Failure { .. }))
        {
            return Err("invalid first failure");
        }
        if self.events.windows(2).any(|pair| {
            pair[0].sequence >= pair[1].sequence || pair[0].elapsed_us > pair[1].elapsed_us
        }) {
            return Err("unordered diagnostic timeline");
        }
        Ok(())
    }
}

/// Each endpoint's timeline is kept separate. Peer evidence is an authenticated
/// peer's assertion; merging does not imply clock synchronization or causality.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReportBundle {
    pub local: Report,
    pub peer: Option<Report>,
}
impl ReportBundle {
    pub fn validate(&self) -> Result<(), &'static str> {
        self.local.validate()?;
        if let Some(peer) = &self.peer {
            peer.validate()?;
            if peer.session != self.local.session || peer.endpoint == self.local.endpoint {
                return Err("peer report belongs to a different session or endpoint");
            }
        }
        Ok(())
    }
    pub fn read(reader: impl Read) -> io::Result<Self> {
        let mut bytes = Vec::new();
        reader.take(MAX_EXPORT_BYTES + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_EXPORT_BYTES {
            return Err(io::Error::other("diagnostic export exceeds limit"));
        }
        let bundle: Self = serde_json::from_slice(&bytes)?;
        bundle.validate().map_err(io::Error::other)?;
        Ok(bundle)
    }
    pub fn write(&self, mut writer: impl Write) -> io::Result<()> {
        self.validate().map_err(io::Error::other)?;
        let bytes = serde_json::to_vec_pretty(self)?;
        if bytes.len() as u64 > MAX_EXPORT_BYTES {
            return Err(io::Error::other("diagnostic export exceeds limit"));
        }
        writer.write_all(&bytes)
    }
}

pub fn micros(duration: std::time::Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Recorder;
    #[test]
    fn imports_reject_wrong_sessions_roles_schemas_and_unbounded_history() {
        let local = Recorder::new(SessionId([1; 16]), Endpoint::Source)
            .snapshot()
            .expect("report");
        let mut peer = Recorder::new(SessionId([2; 16]), Endpoint::Receiver)
            .snapshot()
            .expect("report");
        assert!(
            ReportBundle {
                local: local.clone(),
                peer: Some(peer.clone())
            }
            .validate()
            .is_err()
        );
        peer.session = local.session;
        peer.endpoint = local.endpoint;
        assert!(
            ReportBundle {
                local: local.clone(),
                peer: Some(peer.clone())
            }
            .validate()
            .is_err()
        );
        peer.endpoint = Endpoint::Receiver;
        peer.schema += 1;
        assert!(peer.validate().is_err());
        peer.schema = SCHEMA_VERSION;
        peer.events = vec![
            Event {
                sequence: 1,
                elapsed_us: 0,
                observation: Observation::Ended
            };
            MAX_EVENTS + 1
        ];
        assert!(peer.validate().is_err());
        let oversized = std::io::repeat(b' ').take(MAX_EXPORT_BYTES + 1);
        assert!(ReportBundle::read(oversized).is_err());
    }
}
