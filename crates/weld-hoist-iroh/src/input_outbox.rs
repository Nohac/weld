//! Synchronous input admission and one asynchronous control writer.
//! Like ApplicationInputBuffer, only adjacent absolute motions can coalesce.
//! Every other record (including PointerLeft) is an ordering barrier.

use std::{collections::VecDeque, sync::Mutex};

use anyhow::{Result, ensure};
use tokio::sync::Notify;
use weld_client::{ClientInputTarget, InputEventKind};
use weld_hoist_protocol::{DestinationEnvelope, DestinationMessage};

use crate::peer::QUEUE_CAPACITY;

#[derive(Default)]
pub(super) struct InputOutbox {
    state: Mutex<State>,
    ready: Notify,
}

#[derive(Default)]
struct State {
    closed: bool,
    records: VecDeque<DestinationEnvelope>,
    received: [u64; 6],
    coalesced: u64,
}

impl InputOutbox {
    pub fn push(&self, packet: DestinationEnvelope) -> Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("Iroh input outbox lock is poisoned"))?;
        ensure!(!state.closed, "Iroh input outbox is closed");
        let kind = match packet.message {
            DestinationMessage::Input(_) => 0,
            DestinationMessage::Request(_) => 1,
            DestinationMessage::EncodedCommitFinished { .. } => 2,
            DestinationMessage::CursorReceived { .. } => 3,
            DestinationMessage::BufferReleased { .. } => 4,
            DestinationMessage::Reclaim => 5,
        };
        state.received[kind] = state.received[kind].saturating_add(1);
        if let Some(previous) = state.records.back_mut()
            && compatible_motion(previous, &packet)
        {
            *previous = packet;
            state.coalesced = state.coalesced.saturating_add(1);
            return Ok(());
        }
        // A synchronous compositor caller cannot await. Keep a terminal bound
        // for sustained discrete/control overload rather than lose a release.
        ensure!(
            state.records.len() < QUEUE_CAPACITY,
            "Iroh destination control backlog exhausted: {} queued; received [input, request, frame-ack, cursor-ack, buffer-release, reclaim]={:?}; coalesced_motions={}",
            state.records.len(),
            state.received,
            state.coalesced
        );
        let wake = state.records.is_empty();
        state.records.push_back(packet);
        drop(state);
        if wake {
            self.ready.notify_one();
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
                    .map_err(|_| anyhow::anyhow!("Iroh input outbox lock is poisoned"))?;
                ensure!(!state.closed, "Iroh input outbox is closed");
                if let Some(packet) = state.records.pop_front() {
                    return Ok(packet);
                }
            }
            // notify_one stores a permit if push/close races this await.
            self.ready.notified().await;
        }
    }

    pub fn close(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.closed = true;
            state.records.clear();
        }
        self.ready.notify_one();
    }
}

fn compatible_motion(previous: &DestinationEnvelope, next: &DestinationEnvelope) -> bool {
    let (DestinationMessage::Input(previous_input), DestinationMessage::Input(next_input)) =
        (&previous.message, &next.message)
    else {
        return false;
    };
    previous.session == next.session
        && previous_input.target == next_input.target
        && matches!(previous_input.target, ClientInputTarget::Pointer { .. })
        && matches!(previous_input.event, InputEventKind::PointerMotion { .. })
        && matches!(next_input.event, InputEventKind::PointerMotion { .. })
}

#[cfg(test)]
mod tests {
    use futures_lite::future::poll_once;
    use weld_client::{
        ButtonState, ClientId, ClientSourceId, ClientSurfaceId, InputPosition, LinuxButtonCode,
        LinuxKeycode, RawScrollFrame, RawScrollPhase, RawScrollSource, SurfaceLayerId,
        WireClientInputEvent,
    };
    use weld_hoist_protocol::HoistSessionId;

    use super::*;

    fn motion(time: u32) -> DestinationEnvelope {
        DestinationEnvelope {
            session: HoistSessionId::new(1),
            message: DestinationMessage::Input(WireClientInputEvent {
                target: ClientInputTarget::Pointer {
                    surface: ClientSurfaceId::new(ClientId::new(ClientSourceId::new(0), 1), 1),
                    layer: SurfaceLayerId::new(0),
                },
                event: InputEventKind::PointerMotion {
                    position: InputPosition::new(f64::from(time), 0.0),
                },
                time,
            }),
        }
    }

    #[tokio::test]
    async fn motion_burst_preserves_latest_position_time_and_release() {
        let queue = InputOutbox::default();
        for time in 0..4096 {
            queue.push(motion(time)).expect("motion");
        }
        let mut release = motion(4096);
        if let DestinationMessage::Input(input) = &mut release.message {
            input.event = InputEventKind::PointerButton {
                position: None,
                button: LinuxButtonCode(272),
                state: ButtonState::Released,
            };
        }
        queue.push(release).expect("release");
        let DestinationMessage::Input(input) = queue.recv().await.expect("latest").message else {
            panic!("input")
        };
        assert_eq!(input.time, 4095);
        assert_eq!(
            input.event,
            InputEventKind::PointerMotion {
                position: InputPosition::new(4095.0, 0.0)
            }
        );
        assert!(matches!(
            queue.recv().await.expect("release").message,
            DestinationMessage::Input(WireClientInputEvent {
                event: InputEventKind::PointerButton {
                    state: ButtonState::Released,
                    ..
                },
                ..
            })
        ));
    }

    #[tokio::test]
    async fn leave_keys_and_control_are_coalescing_barriers() {
        for event in [
            InputEventKind::PointerLeft {
                position: InputPosition::default(),
            },
            InputEventKind::Keyboard {
                keycode: LinuxKeycode(30),
                state: ButtonState::Pressed,
            },
            InputEventKind::PointerAxis {
                position: None,
                axis: RawScrollFrame {
                    source: RawScrollSource::Wheel,
                    phase: RawScrollPhase::Moved,
                    horizontal: 0.0,
                    vertical: 1.0,
                    horizontal_v120: None,
                    vertical_v120: Some(120),
                    horizontal_stop: false,
                    vertical_stop: false,
                },
            },
            InputEventKind::PointerButton {
                position: None,
                button: LinuxButtonCode(272),
                state: ButtonState::Pressed,
            },
        ] {
            let queue = InputOutbox::default();
            let mut barrier = motion(2);
            if let DestinationMessage::Input(input) = &mut barrier.message {
                input.event = event;
            }
            queue.push(motion(1)).expect("before");
            queue.push(barrier).expect("barrier");
            queue.push(motion(3)).expect("after");
            assert_eq!(queue.state.lock().expect("state").records.len(), 3);
        }
        let mut other_session = motion(2);
        other_session.session = HoistSessionId::new(2);
        let mut other_target = motion(2);
        if let DestinationMessage::Input(input) = &mut other_target.message {
            input.target = ClientInputTarget::Pointer {
                surface: input.target.surface(),
                layer: SurfaceLayerId::new(1),
            };
        }
        for barrier in [
            other_session,
            other_target,
            DestinationEnvelope {
                session: HoistSessionId::new(1),
                message: DestinationMessage::Reclaim,
            },
            DestinationEnvelope {
                session: HoistSessionId::new(1),
                message: DestinationMessage::CursorReceived {
                    surface: ClientSurfaceId::new(ClientId::new(ClientSourceId::new(0), 1), 1),
                    sequence: 1,
                },
            },
        ] {
            let queue = InputOutbox::default();
            queue.push(motion(1)).expect("before");
            queue.push(barrier).expect("barrier");
            queue.push(motion(3)).expect("after");
            assert_eq!(queue.state.lock().expect("state").records.len(), 3);
        }
    }

    #[tokio::test]
    async fn close_wakes_receiver_and_rejects_further_records() {
        let queue = InputOutbox::default();
        let mut waiting = Box::pin(queue.recv());
        assert!(poll_once(waiting.as_mut()).await.is_none());
        queue.close();
        assert!(
            poll_once(waiting.as_mut())
                .await
                .expect("closed wake")
                .is_err()
        );
        assert!(queue.push(motion(1)).is_err());
        assert!(queue.recv().await.is_err());
    }

    #[tokio::test]
    async fn full_tail_can_coalesce_but_discrete_overload_remains_bounded() {
        let queue = InputOutbox::default();
        for _ in 0..QUEUE_CAPACITY - 1 {
            queue
                .push(DestinationEnvelope {
                    session: HoistSessionId::new(1),
                    message: DestinationMessage::Reclaim,
                })
                .expect("control");
        }
        queue.push(motion(1)).expect("last slot");
        queue.push(motion(2)).expect("replace full tail");
        let error = queue
            .push(DestinationEnvelope {
                session: HoistSessionId::new(1),
                message: DestinationMessage::Reclaim,
            })
            .expect_err("bound");
        assert!(error.to_string().contains("coalesced_motions=1"));
        assert_eq!(
            queue.state.lock().expect("state").records.len(),
            QUEUE_CAPACITY
        );
    }

    #[tokio::test]
    async fn blocked_writer_does_not_hide_backlog_from_coalescing() {
        let queue = InputOutbox::default();
        let (mut writer, mut reader) = tokio::io::duplex(4);
        queue.push(motion(1)).expect("first");
        let mut writing = Box::pin(crate::peer::write_destination_control(&queue, &mut writer));
        assert!(poll_once(writing.as_mut()).await.is_none());
        for time in 2..4096 {
            queue.push(motion(time)).expect("backlog");
        }
        let (result, received) = tokio::join!(writing, async {
            let first: DestinationEnvelope = crate::framing::read_record(&mut reader)
                .await
                .expect("first wire record");
            let last: DestinationEnvelope = crate::framing::read_record(&mut reader)
                .await
                .expect("last wire record");
            queue.close();
            [first, last]
        });
        assert!(result.is_err());
        for (packet, time) in received.into_iter().zip([1, 4095]) {
            let DestinationMessage::Input(input) = packet.message else {
                panic!("input")
            };
            assert_eq!(input.time, time);
        }
    }
}
