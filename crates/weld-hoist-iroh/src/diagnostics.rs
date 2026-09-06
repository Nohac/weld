//! Opt-in Iroh path diagnostics. Never retains a connection or wakes the compositor.

use std::time::Duration;

use futures_lite::StreamExt;
use iroh::{
    TransportAddr,
    endpoint::{Connection, PathEvent},
};
use tokio::time::MissedTickBehavior;

const TARGET: &str = "weld_network_diag";

pub(crate) fn observe(connection: &Connection) {
    if !tracing::enabled!(target: TARGET, tracing::Level::DEBUG) {
        return;
    }
    let weak = connection.weak_handle();
    let mut events = connection.path_events();
    let closed = weak.closed();
    let peer = connection.remote_id();
    snapshot(connection, "initial");
    tokio::spawn(async move {
        let mut interval = tokio::time::interval_at(
            tokio::time::Instant::now() + Duration::from_secs(5),
            Duration::from_secs(5),
        );
        interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
        tokio::pin!(closed);
        loop {
            tokio::select! {
                _ = &mut closed => break,
                event = events.next() => {
                    match event {
                        Some(PathEvent::Selected { id, remote_addr, .. }) => {
                            tracing::debug!(target: TARGET, %peer, path = ?id, transport = path_kind(&remote_addr), "Iroh transmission path selected");
                        }
                        Some(PathEvent::Lagged { missed, .. }) => {
                            tracing::debug!(target: TARGET, %peer, missed, "Iroh path events missed; resnapshotting");
                            if let Some(connection) = weak.upgrade() { snapshot(&connection, "resnapshot"); }
                            else { break; }
                        }
                        Some(_) => {}
                        None => break,
                    }
                }
                _ = interval.tick() => {
                    if let Some(connection) = weak.upgrade() { snapshot(&connection, "periodic"); }
                    else { break; }
                }
            }
        }
        tracing::debug!(target: TARGET, %peer, "Iroh path observation ended");
    });
}

fn snapshot(connection: &Connection, reason: &'static str) {
    let paths = connection.paths();
    if let Some(path) = paths.iter().find(|path| path.is_selected()) {
        tracing::debug!(target: TARGET,
            peer = %connection.remote_id(), path = ?path.id(),
            transport = path_kind(path.remote_addr()),
            rtt_ms = path.rtt().as_secs_f64() * 1000.0,
            reason, "Iroh selected path snapshot");
    } else {
        tracing::debug!(target: TARGET, peer = %connection.remote_id(), reason, "Iroh has no selected path");
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
