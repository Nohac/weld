//! Synchronous input admission and one asynchronous control writer.
//! Adjacent compatible pointer motions or same-contact touch motions can coalesce.
//! Every other record (including PointerLeft) is an ordering barrier.

use std::{
    collections::VecDeque,
    sync::Mutex,
    time::{Duration, Instant},
};

use anyhow::{Result, ensure};
use async_channel::{Receiver, Sender};
use weld_client::{ClientInputTarget, InputEventKind};
use weld_hoist_protocol::{DestinationEnvelope, DestinationMessage};

const QUEUE_CAPACITY: usize = 256;

/// Bounded semantic input queue shared by asynchronous connection writers.
/// Its short lock covers tail coalescing and dequeue; I/O runs after unlocking.
pub struct InputOutbox {
    state: Mutex<State>,
    ready: Sender<()>,
    notified: Receiver<()>,
}

impl Default for InputOutbox {
    fn default() -> Self {
        let (ready, notified) = async_channel::bounded(1);
        Self {
            state: Mutex::new(State::default()),
            ready,
            notified,
        }
    }
}

struct State {
    closed: bool,
    records: VecDeque<QueuedRecord>,
    received: [u64; 6],
    coalesced: u64,
    gamepad_coalesced: u64,
    observations: InputObservations,
    started: Instant,
    last_report: Instant,
}

struct QueuedRecord {
    packet: DestinationEnvelope,
    /// Age of the latest retained event, not the first superseded motion.
    enqueued_at: Instant,
}

#[derive(Clone, Copy, Default)]
struct InputObservations {
    motions_received: u64,
    dequeued: u64,
    writes_completed: u64,
    motions_written: u64,
    framed_bytes: u64,
    queue_high_water: usize,
    retained_wait_max: Duration,
    write_wall_max: Duration,
}

impl Default for State {
    fn default() -> Self {
        let now = Instant::now();
        Self {
            closed: false,
            records: VecDeque::new(),
            received: [0; 6],
            coalesced: 0,
            gamepad_coalesced: 0,
            observations: InputObservations::default(),
            started: now,
            last_report: now,
        }
    }
}

impl InputOutbox {
    pub fn push(&self, mut packet: DestinationEnvelope) -> Result<()> {
        let enqueued_at = Instant::now();
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("input outbox lock is poisoned"))?;
        ensure!(!state.closed, "input outbox is closed");
        let kind = match packet.message {
            DestinationMessage::Input(_) => 0,
            DestinationMessage::Request(_) => 1,
            DestinationMessage::CursorReceived { .. } => 2,
            DestinationMessage::BufferReleased { .. } => 3,
            DestinationMessage::Reclaim => 4,
            DestinationMessage::Gamepad(_) => 5,
        };
        state.received[kind] = state.received[kind].saturating_add(1);
        if is_motion(&packet) {
            state.observations.motions_received =
                state.observations.motions_received.saturating_add(1);
        }
        if let Some(previous) = state.records.back_mut()
            && (coalesce_motion(&previous.packet, &mut packet)
                || compatible_gamepad(&previous.packet, &packet))
        {
            let gamepad = matches!(packet.message, DestinationMessage::Gamepad(_));
            *previous = QueuedRecord {
                packet,
                enqueued_at,
            };
            if gamepad {
                state.gamepad_coalesced = state.gamepad_coalesced.saturating_add(1);
            } else {
                state.coalesced = state.coalesced.saturating_add(1);
            }
            return Ok(());
        }
        // A synchronous compositor caller cannot await. Keep a terminal bound
        // for sustained discrete/control overload rather than lose a release.
        ensure!(
            state.records.len() < QUEUE_CAPACITY,
            "destination control backlog exhausted: {} queued; received [input, request, cursor-ack, buffer-release, reclaim, gamepad]={:?}; coalesced_motions={}",
            state.records.len(),
            state.received,
            state.coalesced
        );
        let wake = state.records.is_empty();
        state.records.push_back(QueuedRecord {
            packet,
            enqueued_at,
        });
        state.observations.queue_high_water =
            state.observations.queue_high_water.max(state.records.len());
        drop(state);
        if wake {
            let _ = self.ready.try_send(());
        }
        Ok(())
    }

    /// Only the peer's single control writer consumes this queue. Never pull
    /// a batch into another queue: leave unwritten motions available to coalesce.
    pub async fn recv(&self) -> Result<DestinationEnvelope> {
        loop {
            {
                let mut state = self
                    .state
                    .lock()
                    .map_err(|_| anyhow::anyhow!("input outbox lock is poisoned"))?;
                ensure!(!state.closed, "input outbox is closed");
                if let Some(packet) = state.records.pop_front() {
                    state.observations.dequeued = state.observations.dequeued.saturating_add(1);
                    state.observations.retained_wait_max = state
                        .observations
                        .retained_wait_max
                        .max(packet.enqueued_at.elapsed());
                    return Ok(packet.packet);
                }
            }
            // The bounded signal retains a wake if push races this await.
            self.notified
                .recv()
                .await
                .map_err(|_| anyhow::anyhow!("input outbox is closed"))?;
        }
    }

    /// Completion is local stream acceptance, not peer receipt or application.
    /// Failed writes do not count. Diagnostics never change transport outcomes.
    pub fn record_written(&self, packet: &DestinationEnvelope, bytes: usize, wall: Duration) {
        let diagnostics_enabled =
            tracing::enabled!(target: "weld_network_diag", tracing::Level::DEBUG);
        let report = {
            let Ok(mut state) = self.state.lock() else {
                return;
            };
            let observations = &mut state.observations;
            observations.writes_completed = observations.writes_completed.saturating_add(1);
            if is_motion(packet) {
                observations.motions_written = observations.motions_written.saturating_add(1);
            }
            observations.framed_bytes = observations
                .framed_bytes
                .saturating_add(u64::try_from(bytes).unwrap_or(u64::MAX));
            observations.write_wall_max = observations.write_wall_max.max(wall);
            let now = Instant::now();
            if now.duration_since(state.last_report) < Duration::from_secs(1)
                || !diagnostics_enabled
            {
                return;
            }
            state.last_report = now;
            (
                state.received,
                state.coalesced,
                state.gamepad_coalesced,
                state.observations,
                state.records.len(),
                now.duration_since(state.started),
            )
        };
        let (received, coalesced, gamepad_coalesced, observations, queued, uptime) = report;
        // All counts and maxima are cumulative per outbox. Emission requires
        // successful traffic, so neither idle nor blocked writes start a timer.
        tracing::debug!(target: "weld_network_diag",
            uptime_ms = uptime.as_millis(),
            received_by_kind_total = ?received,
            motions_received_total = observations.motions_received,
            motions_coalesced_total = coalesced,
            gamepad_coalesced_total = gamepad_coalesced,
            dequeued_total = observations.dequeued,
            writes_completed_total = observations.writes_completed,
            motions_written_total = observations.motions_written,
            framed_bytes_total = observations.framed_bytes,
            queued, queue_high_water = observations.queue_high_water,
            retained_wait_max_us = observations.retained_wait_max.as_micros(),
            write_wall_max_us = observations.write_wall_max.as_micros(),
            "Hoist outgoing input summary");
    }

    pub fn close(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.closed = true;
            state.records.clear();
        }
        self.ready.close();
    }
}

fn is_motion(packet: &DestinationEnvelope) -> bool {
    matches!(&packet.message, DestinationMessage::Input(input)
        if matches!(input.event, InputEventKind::PointerMotion { .. } | InputEventKind::Touch { event: weld_client::TouchEvent::Motion { .. } }))
}

fn coalesce_motion(previous: &DestinationEnvelope, next: &mut DestinationEnvelope) -> bool {
    let (DestinationMessage::Input(previous_input), DestinationMessage::Input(next_input)) =
        (&previous.message, &mut next.message)
    else {
        return false;
    };
    if previous.session != next.session || previous_input.target != next_input.target {
        return false;
    }
    if let (
        InputEventKind::Touch {
            event: weld_client::TouchEvent::Motion { id: old, .. },
        },
        InputEventKind::Touch {
            event: weld_client::TouchEvent::Motion { id: new, .. },
        },
    ) = (&previous_input.event, &next_input.event)
    {
        return matches!(previous_input.target, ClientInputTarget::Touch { .. }) && old == new;
    }
    if !matches!(previous_input.target, ClientInputTarget::Pointer { .. }) {
        return false;
    }
    let (
        InputEventKind::PointerMotion {
            relative: older, ..
        },
        InputEventKind::PointerMotion {
            relative: newer, ..
        },
    ) = (&previous_input.event, &mut next_input.event)
    else {
        return false;
    };
    let Some(merged) = weld_client::RelativeMotion::coalesce(*newer, *older) else {
        return false;
    };
    *newer = merged;
    true
}

fn compatible_gamepad(previous: &DestinationEnvelope, next: &DestinationEnvelope) -> bool {
    matches!((&previous.message, &next.message),
        (DestinationMessage::Gamepad(old), DestinationMessage::Gamepad(new))
        if previous.session == next.session && new.supersedes(*old))
}

#[cfg(test)]
mod tests;
