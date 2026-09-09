//! Keyboard repeat policy, independent of native keyboard state and timers.

use std::collections::HashMap;

use weld_client::{ClientSurfaceId, KeyboardKeyState, LinuxKeycode};

/// Repeat cadence owner for a native host's keyboard. Fixed at startup.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KeyboardRepeatMode {
    /// Applications generate repeats. Use when no upstream cadence is available.
    Client,
    /// Input supplies explicit repeats; keyboard-v10 applications must not generate them.
    Compositor,
}

/// Fallback for pre-v10 keyboards and input-method grabs in compositor-repeat mode.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum LegacyKeyRepeat {
    /// Retain client timers. Network-delayed releases can produce unwanted repeats.
    #[default]
    Client,
    /// Disable legacy timers without emulating repeats as press/release pairs.
    Disabled,
}

struct HeldKey {
    surface: ClientSurfaceId,
    repeat_allowed: bool,
}

/// Remembers Weld's addressed press context, not Smithay's XKB state.
#[derive(Default)]
pub(crate) struct KeyboardRepeatTracker {
    held: HashMap<LinuxKeycode, HeldKey>,
}

impl KeyboardRepeatTracker {
    pub(crate) fn observe(
        &mut self,
        surface: ClientSurfaceId,
        keycode: LinuxKeycode,
        state: KeyboardKeyState,
    ) -> bool {
        match state {
            KeyboardKeyState::Pressed => {
                self.held.insert(
                    keycode,
                    HeldKey {
                        surface,
                        repeat_allowed: true,
                    },
                );
                true
            }
            KeyboardKeyState::Released => {
                self.held.remove(&keycode);
                true
            }
            KeyboardKeyState::Repeated => self
                .held
                .get(&keycode)
                .is_some_and(|held| held.surface == surface && held.repeat_allowed),
        }
    }

    pub(crate) fn focus_changed(&mut self) {
        for held in self.held.values_mut() {
            held.repeat_allowed = false;
        }
    }

    pub(crate) fn clear(&mut self) {
        self.held.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use weld_client::{ClientId, ClientSourceId};

    fn surface(local: u64) -> ClientSurfaceId {
        ClientSurfaceId::new(ClientId::new(ClientSourceId::new(0), 0), local)
    }

    #[test]
    fn repeat_requires_the_original_live_press_context() {
        let mut tracker = KeyboardRepeatTracker::default();
        let key = LinuxKeycode(30);
        assert!(!tracker.observe(surface(1), key, KeyboardKeyState::Repeated));
        assert!(tracker.observe(surface(1), key, KeyboardKeyState::Pressed));
        assert!(!tracker.observe(surface(2), key, KeyboardKeyState::Repeated));
        assert!(tracker.observe(surface(1), key, KeyboardKeyState::Repeated));
        assert_eq!(tracker.held.len(), 1);
        assert!(tracker.observe(surface(1), key, KeyboardKeyState::Released));
        assert!(!tracker.observe(surface(1), key, KeyboardKeyState::Repeated));
    }

    #[test]
    fn returning_focus_does_not_restart_a_hold_and_cleanup_cancels_it() {
        let mut tracker = KeyboardRepeatTracker::default();
        let key = LinuxKeycode(30);
        tracker.observe(surface(1), key, KeyboardKeyState::Pressed);
        tracker.focus_changed();
        tracker.focus_changed();
        assert!(!tracker.observe(surface(1), key, KeyboardKeyState::Repeated));
        assert_eq!(
            tracker.held.len(),
            1,
            "focus loss preserves release bookkeeping"
        );
        tracker.observe(surface(1), key, KeyboardKeyState::Released);
        tracker.observe(surface(1), key, KeyboardKeyState::Pressed);
        assert!(tracker.observe(surface(1), key, KeyboardKeyState::Repeated));
        tracker.clear();
        assert!(!tracker.observe(surface(1), key, KeyboardKeyState::Repeated));
    }
}
