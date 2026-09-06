//! Always-on bounded path observations, with opt-in five-second diagnostics.
//! Sampling holds only a weak connection and never wakes the compositor.

use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use futures_lite::StreamExt;
use iroh::{
    TransportAddr,
    endpoint::{Connection, PathEvent, PathId},
};
use tokio::time::MissedTickBehavior;
use weld_hoist_encoded::NetworkPathSnapshot;

const TARGET: &str = "weld_network_diag";

#[derive(Clone, Default)]
pub(crate) struct PathMonitor(Arc<Mutex<PathHistory>>);

struct PathHistory {
    id: Option<PathId>,
    epoch: Option<u64>,
    latest: Option<NetworkPathSnapshot>,
}

impl Default for PathHistory {
    fn default() -> Self {
        Self {
            id: None,
            epoch: Some(0),
            latest: None,
        }
    }
}

impl PathHistory {
    fn update(&mut self, next: Option<(PathId, NetworkPathSnapshot)>, uncertain: bool) {
        if let (Some(previous), Some((_, sample))) = (self.latest, next)
            && sample.sampled_at < previous.sampled_at
        {
            return;
        }
        let id = next.map(|(id, _)| id);
        let rollback = self
            .latest
            .zip(next)
            .is_some_and(|(previous, (_, sample))| {
                sample.sent_bytes < previous.sent_bytes
                    || sample.received_bytes < previous.received_bytes
                    || sample.lost_packets < previous.lost_packets
                    || sample.lost_bytes < previous.lost_bytes
                    || sample.congestion_events < previous.congestion_events
            });
        if uncertain || id != self.id || rollback {
            self.epoch = self.epoch.and_then(|epoch| epoch.checked_add(1));
        }
        self.id = id;
        self.latest = next.and_then(|(_, mut sample)| {
            sample.epoch = self.epoch?;
            Some(sample)
        });
    }
}

impl PathMonitor {
    pub fn snapshot(&self) -> Option<NetworkPathSnapshot> {
        self.0.lock().ok()?.latest
    }

    fn update(&self, next: Option<(PathId, NetworkPathSnapshot)>, uncertain: bool) {
        if let Ok(mut state) = self.0.lock() {
            state.update(next, uncertain);
        }
    }
}

pub(crate) fn observe(connection: &Connection) -> PathMonitor {
    let monitor = PathMonitor::default();
    let weak = connection.weak_handle();
    let mut events = connection.path_events();
    let closed = weak.closed();
    let peer = connection.remote_id();
    snapshot(connection, &monitor, false, Some("initial"));
    let observer = monitor.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval_at(
            tokio::time::Instant::now() + Duration::from_secs(1),
            Duration::from_secs(1),
        );
        interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let mut last_log = Instant::now();
        tokio::pin!(closed);
        loop {
            tokio::select! {
                _ = &mut closed => break,
                event = events.next() => {
                    let (uncertain, reason) = match event {
                        Some(PathEvent::Selected { id, remote_addr, .. }) => {
                            tracing::debug!(target: TARGET, %peer, path = ?id, transport = path_kind(&remote_addr), "Iroh transmission path selected");
                            (true, None)
                        }
                        Some(PathEvent::Lagged { missed, .. }) => {
                            tracing::debug!(target: TARGET, %peer, missed, "Iroh path events missed; resnapshotting");
                            (true, Some("resnapshot"))
                        }
                        Some(_) => (false, None),
                        None => break,
                    };
                    if let Some(connection) = weak.upgrade() { snapshot(&connection, &observer, uncertain, reason); }
                    else { break; }
                }
                _ = interval.tick() => {
                    let reason = if last_log.elapsed() >= Duration::from_secs(5) {
                        last_log = Instant::now(); Some("periodic")
                    } else { None };
                    if let Some(connection) = weak.upgrade() { snapshot(&connection, &observer, false, reason); }
                    else { break; }
                }
            }
        }
        observer.update(None, false);
        tracing::debug!(target: TARGET, %peer, "Iroh path observation ended");
    });
    monitor
}

fn snapshot(
    connection: &Connection,
    monitor: &PathMonitor,
    uncertain: bool,
    reason: Option<&'static str>,
) {
    if connection.close_reason().is_some() {
        monitor.update(None, uncertain);
        return;
    }
    let paths = connection.paths();
    if let Some(path) = paths.iter().find(|path| path.is_selected()) {
        let stats = path.stats();
        monitor.update(
            Some((
                path.id(),
                NetworkPathSnapshot {
                    sampled_at: Instant::now(),
                    epoch: 0,
                    rtt: stats.rtt,
                    congestion_window_bytes: stats.cwnd,
                    sent_bytes: stats.udp_tx.bytes,
                    received_bytes: stats.udp_rx.bytes,
                    lost_packets: stats.lost_packets,
                    lost_bytes: stats.lost_bytes,
                    congestion_events: stats.congestion_events,
                },
            )),
            uncertain,
        );
        if let Some(reason) = reason {
            tracing::debug!(target: TARGET,
                peer = %connection.remote_id(), path = ?path.id(),
                transport = path_kind(path.remote_addr()), rtt_ms = stats.rtt.as_secs_f64() * 1000.0,
                reason, "Iroh selected path snapshot");
        }
    } else {
        monitor.update(None, uncertain);
        if let Some(reason) = reason {
            tracing::debug!(target: TARGET, peer = %connection.remote_id(), reason, "Iroh has no selected path");
        }
    }
}

fn path_kind(address: &TransportAddr) -> &'static str {
    match address {
        TransportAddr::Ip(address) if address.is_ipv4() => "ipv4",
        TransportAddr::Ip(_) => "ipv6",
        TransportAddr::Relay(_) => "relay",
        _ => "other",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(now: Instant, bytes: u64) -> NetworkPathSnapshot {
        NetworkPathSnapshot {
            sampled_at: now,
            epoch: 0,
            rtt: Duration::from_millis(40),
            congestion_window_bytes: 12000,
            sent_bytes: bytes,
            received_bytes: bytes,
            lost_packets: 0,
            lost_bytes: 0,
            congestion_events: 0,
        }
    }

    #[test]
    fn selection_and_uncertainty_change_epochs_not_ordinary_samples() {
        let mut history = PathHistory::default();
        let now = Instant::now();
        history.update(Some((PathId::ZERO, sample(now, 10))), false);
        assert_eq!(history.latest.expect("first").epoch, 1);
        history.update(Some((PathId::ZERO, sample(now, 20))), false);
        assert_eq!(history.latest.expect("same path").epoch, 1);
        history.update(Some((PathId::MAX, sample(now, 30))), false);
        assert_eq!(history.latest.expect("other path").epoch, 2);
        history.update(Some((PathId::ZERO, sample(now, 40))), false);
        assert_eq!(history.latest.expect("return path").epoch, 3);
        history.update(Some((PathId::ZERO, sample(now, 40))), true);
        assert_eq!(history.latest.expect("lagged selection history").epoch, 4);
        history.update(Some((PathId::ZERO, sample(now, 1))), false);
        assert_eq!(history.latest.expect("counter reset").epoch, 5);
        history.update(None, false);
        assert!(history.latest.is_none());
        history.update(Some((PathId::ZERO, sample(now, 2))), false);
        assert_eq!(history.latest.expect("returned after no path").epoch, 7);
    }

    #[test]
    fn snapshots_are_owned_and_stale_updates_do_not_replace_fresh_samples() {
        let monitor = PathMonitor::default();
        let other = PathMonitor::default();
        let now = Instant::now();
        monitor.update(
            Some((PathId::ZERO, sample(now + Duration::from_secs(1), 20))),
            false,
        );
        let owned = monitor.snapshot().expect("no tracing required");
        monitor.update(Some((PathId::MAX, sample(now, 10))), false);
        assert_eq!(monitor.snapshot(), Some(owned));
        assert!(other.snapshot().is_none());
        monitor.update(None, false);
        assert!(monitor.snapshot().is_none());
        assert_eq!(owned.sent_bytes, 20);
    }

    #[test]
    fn exhausted_epoch_never_reuses_an_old_identity() {
        let mut history = PathHistory {
            epoch: Some(u64::MAX),
            ..Default::default()
        };
        history.update(Some((PathId::ZERO, sample(Instant::now(), 1))), false);
        assert!(history.latest.is_none());
        history.update(Some((PathId::ZERO, sample(Instant::now(), 2))), false);
        assert!(history.latest.is_none());
    }
}
