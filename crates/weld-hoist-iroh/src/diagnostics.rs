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

pub(crate) fn observe(
    connection: &Connection,
    recorder: Option<weld_diagnostics::Recorder>,
) -> PathMonitor {
    let monitor = PathMonitor::default();
    let weak = connection.weak_handle();
    let mut events = connection.path_events();
    let closed = weak.closed();
    let peer = connection.remote_id();
    snapshot(
        connection,
        &monitor,
        false,
        Some("initial"),
        recorder.as_ref(),
    );
    let observer = monitor.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval_at(
            tokio::time::Instant::now() + Duration::from_secs(1),
            Duration::from_secs(1),
        );
        interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let mut last_log = Instant::now();
        tokio::pin!(closed);
        let reason = loop {
            tokio::select! {
                reason = &mut closed => break reason,
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
                        None => break (&mut closed).await,
                    };
                    if let Some(connection) = weak.upgrade() { snapshot(&connection, &observer, uncertain, reason, recorder.as_ref()); }
                    else { break (&mut closed).await; }
                }
                _ = interval.tick() => {
                    let reason = if last_log.elapsed() >= Duration::from_secs(5) {
                        last_log = Instant::now(); Some("periodic")
                    } else { None };
                    if let Some(connection) = weak.upgrade() { snapshot(&connection, &observer, false, reason, recorder.as_ref()); }
                    else { break (&mut closed).await; }
                }
            }
        };
        // The registered weak close future retains the reason even after the
        // final connection handle is dropped. Every observer exit freezes the report.
        if let Some(recorder) = &recorder {
            use weld_diagnostics::{Cause, Observation, Operation};
            let cause = match reason.map(|closed| closed.reason) {
                Some(iroh::endpoint::ConnectionError::LocallyClosed) | None => None,
                Some(iroh::endpoint::ConnectionError::ApplicationClosed(_)) => {
                    Some(Cause::PeerClosed)
                }
                Some(_) => Some(Cause::TransportLost),
            };
            if let Some(cause) = cause {
                recorder.record(Observation::Failure {
                    operation: Operation::Connection,
                    cause,
                });
            }
            recorder.record(Observation::Ended);
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
    recorder: Option<&weld_diagnostics::Recorder>,
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
        if let Some(recorder) = recorder
            && let Some(sample) = monitor.snapshot()
        {
            use weld_diagnostics::{NetworkSample, Observation, PathKind};
            if reason == Some("initial") || uncertain {
                let kind = match path_kind(path.remote_addr()) {
                    "ipv4" => PathKind::Ipv4,
                    "ipv6" => PathKind::Ipv6,
                    "relay" => PathKind::Relay,
                    "adb" => PathKind::Adb,
                    _ => PathKind::Other,
                };
                recorder.record(Observation::Path(kind));
            }
            recorder.record(Observation::Network(NetworkSample {
                epoch: sample.epoch,
                rtt_us: weld_diagnostics::micros(sample.rtt),
                congestion_window_bytes: sample.congestion_window_bytes,
                sent_bytes: sample.sent_bytes,
                received_bytes: sample.received_bytes,
                lost_packets: sample.lost_packets,
                congestion_events: sample.congestion_events,
            }));
        }
        if let Some(reason) = reason {
            tracing::debug!(target: TARGET,
                peer = %connection.remote_id(), path = ?path.id(),
                transport = path_kind(path.remote_addr()), rtt_ms = stats.rtt.as_secs_f64() * 1000.0,
                sent_bytes = stats.udp_tx.bytes, received_bytes = stats.udp_rx.bytes,
                congestion_window_bytes = stats.cwnd, lost_packets = stats.lost_packets,
                congestion_events = stats.congestion_events,
                reason, "Iroh selected path snapshot");
        }
    } else {
        monitor.update(None, uncertain);
        if (uncertain || reason == Some("initial"))
            && let Some(recorder) = recorder
        {
            recorder.record(weld_diagnostics::Observation::Path(
                weld_diagnostics::PathKind::Unavailable,
            ));
        }
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
        TransportAddr::Custom(address) if address.id() == crate::adb::TRANSPORT_ID => "adb",
        _ => "other",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn dropping_the_last_connection_always_finishes_its_report() {
        use weld_diagnostics::{Endpoint, Recorder, SessionId};
        for explicit_close in [false, true] {
            let (source, receiver, connection, remote) = crate::tests::connection_pair().await;
            let recorder = Recorder::new(SessionId([7; 16]), Endpoint::Source);
            observe(&connection, Some(recorder.clone()));
            if explicit_close {
                connection.close(0_u32.into(), b"test completed");
            }
            drop(connection);
            tokio::time::timeout(Duration::from_secs(3), async {
                while !recorder.snapshot().expect("report").ended {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .expect("observer must terminate for either close path");
            assert!(recorder.snapshot().expect("report").first_failure.is_none());
            drop(remote);
            source.close().await;
            receiver.close().await;
        }
    }

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
