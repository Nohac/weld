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
        self.push(event);
        !consumed
    }

    fn push(&mut self, event: RawSeatEvent) {
        let adjacent_motion = matches!(event.event, RawSeatEventKind::PointerMotion { .. })
            && self.events.back().is_some_and(|previous| {
                matches!(previous.event, RawSeatEventKind::PointerMotion { .. })
            });
        if adjacent_motion {
            if let Some(previous) = self.events.back_mut() {
                *previous = event;
            }
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
