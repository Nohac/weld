//! Host input filtering and refresh-paced application buffering.

use std::collections::VecDeque;

use bevy::ecs::world::World;

use super::{
    filter_global_shortcut_event, filter_pointer_shortcut_event, filter_virtual_terminal_event,
    raw::{RawSeatEvent, RawSeatEventKind},
};

pub(super) const INPUT_BURST_CAPACITY: usize = 64;

/// Refresh-paced view of raw seat input for application systems.
///
/// Adjacent absolute pointer motion has one observable result at the next
/// application update, so only its latest position and timestamp are retained.
/// Every discrete transition remains an ordering barrier and the queue grows
/// rather than dropping input when a burst exceeds its initial capacity.
pub(crate) struct ApplicationInputBuffer {
    events: VecDeque<RawSeatEvent>,
}

impl Default for ApplicationInputBuffer {
    fn default() -> Self {
        Self {
            events: VecDeque::with_capacity(INPUT_BURST_CAPACITY),
        }
    }
}

impl ApplicationInputBuffer {
    pub(crate) fn enqueue(&mut self, world: &mut World, event: RawSeatEvent) -> bool {
        let consumed = filter_global_shortcut_event(world, &event)
            | filter_virtual_terminal_event(world, &event)
            | filter_pointer_shortcut_event(world, &event);
        if let RawSeatEventKind::PointerButton {
            position,
            button,
            state,
        } = &event.event
        {
            tracing::debug!(
                target: "weld_input_diag",
                time = event.time, ?position, ?button, ?state, consumed,
                "host pointer button filtered"
            );
        }
        if !matches!(
            event.event,
            RawSeatEventKind::Keyboard {
                state: weld_core::input::KeyboardKeyState::Repeated,
                ..
            }
        ) {
            self.push(event);
        }
        !consumed
    }

    fn push(&mut self, mut event: RawSeatEvent) {
        if let Some(previous) = self.events.back_mut()
            && let (
                RawSeatEventKind::PointerMotion {
                    relative: older, ..
                },
                RawSeatEventKind::PointerMotion {
                    relative: newer, ..
                },
            ) = (&previous.event, &mut event.event)
            && let Some(merged) = weld_client::RelativeMotion::coalesce(*newer, *older)
        {
            *newer = merged;
            *previous = event;
            return;
        }
        self.events.push_back(event);
    }

    pub(super) fn events_mut(&mut self) -> &mut VecDeque<RawSeatEvent> {
        &mut self.events
    }

    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.events.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::raw::{ButtonState, InputPosition, LinuxButtonCode, RawSeatEventKind};

    fn motion(x: f64, time: u32) -> RawSeatEvent {
        RawSeatEvent::new(
            RawSeatEventKind::PointerMotion {
                relative: None,
                position: InputPosition::new(x, 20.0),
            },
            time,
        )
    }

    fn button(state: ButtonState, time: u32) -> RawSeatEvent {
        RawSeatEvent::new(
            RawSeatEventKind::PointerButton {
                position: None,
                button: LinuxButtonCode(0x117),
                state,
            },
            time,
        )
    }

    #[test]
    fn adjacent_pointer_motion_keeps_only_the_latest_observation() {
        let mut input = ApplicationInputBuffer::default();
        input.push(motion(10.0, 1));
        input.push(motion(20.0, 2));
        input.push(motion(30.0, 3));

        assert_eq!(input.events, VecDeque::from([motion(30.0, 3)]));
    }

    #[test]
    fn application_batch_retains_accumulated_relative_motion() {
        use weld_client::{InputDelta, RelativeMotion};
        let mut input = ApplicationInputBuffer::default();
        for time in 1..=3 {
            let mut event = motion(f64::from(time), time);
            if let RawSeatEventKind::PointerMotion { relative, .. } = &mut event.event {
                *relative = Some(RelativeMotion {
                    delta: InputDelta::new(2.0, -1.0),
                    unaccelerated: InputDelta::new(1.0, -0.5),
                    time_micros: u64::from(time) * 1000,
                });
            }
            input.push(event);
        }
        input.push(motion(4.0, 4));
        assert_eq!(
            input.len(),
            2,
            "absolute motion remains an ordering barrier"
        );
        assert!(
            matches!(input.events[0].event, RawSeatEventKind::PointerMotion { position, relative: Some(relative) }
            if position.x == 3.0 && relative.delta == InputDelta::new(6.0, -3.0)
            && relative.unaccelerated == InputDelta::new(3.0, -1.5) && relative.time_micros == 3000)
        );
    }

    #[test]
    fn discrete_input_preserves_the_motion_ordering_barrier() {
        let mut input = ApplicationInputBuffer::default();
        input.push(motion(10.0, 1));
        input.push(button(ButtonState::Pressed, 2));
        input.push(motion(20.0, 3));
        input.push(motion(30.0, 4));
        input.push(button(ButtonState::Released, 5));

        assert_eq!(
            input.events,
            VecDeque::from([
                motion(10.0, 1),
                button(ButtonState::Pressed, 2),
                motion(30.0, 4),
                button(ButtonState::Released, 5),
            ])
        );
    }

    #[test]
    fn input_bursts_grow_without_dropping_discrete_transitions() {
        let mut input = ApplicationInputBuffer::default();
        for time in 0..(INPUT_BURST_CAPACITY as u32 * 2) {
            let state = if time % 2 == 0 {
                ButtonState::Pressed
            } else {
                ButtonState::Released
            };
            input.push(button(state, time));
        }

        assert_eq!(input.events.len(), INPUT_BURST_CAPACITY * 2);
        assert_eq!(input.events.front().map(|event| event.time), Some(0));
        assert_eq!(
            input.events.back().map(|event| event.time),
            Some(INPUT_BURST_CAPACITY as u32 * 2 - 1)
        );
    }
}
