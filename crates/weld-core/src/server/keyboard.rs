//! Publish one configured keymap to native clients and shell input resolution.

use super::{ServerState, input_seat::InputSeat};
use crate::input::{KeyboardMapper, KeyboardSettings, RawSeatEvent, RawSeatEventKind};
use anyhow::Result;
use smithay::input::keyboard::{KeyboardHandle, xkb};

impl ServerState {
    pub(crate) fn resolve_input(&mut self, event: RawSeatEvent) -> RawSeatEvent {
        let event = self.keyboard_mapper.resolve(event);
        // Releases still update the active client after the press recipient has
        // closed, or when the shell consumes the corresponding key event.
        if matches!(
            event.event,
            RawSeatEventKind::Keyboard { .. } | RawSeatEventKind::HostFocusLost
        ) {
            let input = self.local_input.keyboard_binding.borrow().clone();
            if let Some(input) = input {
                self.synchronize_local_modifiers(&input);
            }
        }
        event
    }

    pub(super) fn synchronize_local_modifiers(&mut self, input: &InputSeat) {
        if input.controller.get().is_none()
            && let Some(keyboard) = input.native.get_keyboard()
        {
            self.apply_local_modifiers(&keyboard);
        }
    }

    pub(super) fn apply_local_modifiers(&mut self, keyboard: &KeyboardHandle<Self>) {
        let state = self.keyboard_mapper.xkb_state();
        let depressed = state.serialize_mods(xkb::STATE_MODS_DEPRESSED);
        let latched = state.serialize_mods(xkb::STATE_MODS_LATCHED);
        let locked = state.serialize_mods(xkb::STATE_MODS_LOCKED);
        let layout_depressed = state.serialize_layout(xkb::STATE_LAYOUT_DEPRESSED);
        let layout_latched = state.serialize_layout(xkb::STATE_LAYOUT_LATCHED);
        let layout_locked = state.serialize_layout(xkb::STATE_LAYOUT_LOCKED);
        keyboard.with_xkb_state(self, |mut context| {
            context.set_modifier_masks(
                depressed,
                latched,
                locked,
                layout_depressed,
                layout_latched,
                layout_locked,
            );
        });
    }

    pub(crate) fn synchronize_keyboard_settings(
        &mut self,
        settings: Option<KeyboardSettings>,
    ) -> Result<()> {
        if let Some(settings) = settings {
            self.set_legacy_key_repeat(settings.legacy_repeat);
            let keymap = settings
                .keymap
                .unwrap_or_else(|| self.default_keymap.clone());
            self.pending_keymap = (self.keyboard_mapper.keymap() != &keymap).then_some(keymap);
        }
        if self.pending_keymap.is_none() || self.keyboard_mapper.has_pressed_keys() {
            return Ok(());
        }
        let keyboards = self
            .input_seats()
            .filter_map(|input| input.native.get_keyboard())
            .collect::<Vec<_>>();
        if keyboards
            .iter()
            .any(|keyboard| !keyboard.pressed_keys().is_empty())
        {
            return Ok(());
        }
        let Some(keymap) = self.pending_keymap.as_ref() else {
            return Ok(());
        };
        // A reload waits for all local and client-visible holds to finish, so
        // releases use the same map as their presses. Both interpreters switch
        // between input batches, after the new map has compiled successfully.
        let mapper = KeyboardMapper::new(keymap.clone())?;
        let keymap_text = keymap.as_str().to_owned();
        for keyboard in keyboards {
            keyboard.set_keymap_from_string(self, keymap_text.clone())?;
        }
        self.keyboard_mapper = mapper;
        self.pending_keymap = None;
        tracing::info!("applied keyboard keymap");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        OutputId, OutputScale,
        dmabuf::DmabufSourceCache,
        input::{
            KeyboardKeyState, KeyboardKeymap, KeyboardRepeatMode, KeymapConfig, LegacyKeyRepeat,
            LinuxKeycode, RawSeatEventKind,
        },
        server::{
            OutputDescriptor, OutputMetrics, ServerOptions, ServerOutputDefinition,
            WaylandClientBridge,
        },
    };
    use calloop::{EventLoop, channel};
    use smithay::{
        backend::input::{InputTime, KeyEvent},
        input::keyboard::FilterResult,
        output::{PhysicalProperties, Subpixel},
        reexports::wayland_server::Display,
        utils::SERIAL_COUNTER,
    };
    use std::time::Instant;

    #[test]
    #[ignore = "native socket fixture requires XDG_RUNTIME_DIR"]
    fn native_and_shell_maps_switch_together_after_held_keys_release() {
        let event_loop = EventLoop::<ServerState>::try_new().expect("loop");
        let (_sender, receiver) = channel::channel();
        let socket = format!("weld-keymap-test-{}", std::process::id());
        let mut server = ServerState::new(
            &event_loop.handle(),
            Display::new().expect("display"),
            receiver,
            WaylandClientBridge::default(),
            ServerOptions {
                started_at: Instant::now(),
                seat_name: "keyboard-test",
                socket_name: Some(&socket),
                dmabuf_capabilities: None,
                dmabuf_sources: DmabufSourceCache::unavailable(),
                keyboard_repeat_mode: KeyboardRepeatMode::Client,
                initial_toplevel_size: None,
                outputs: vec![ServerOutputDefinition {
                    id: OutputId::new(1),
                    primary: true,
                    logical_position: (0, 0),
                    metrics: OutputMetrics::new(640, 480, OutputScale::new(1.0).expect("scale"))
                        .expect("metrics"),
                    descriptor: OutputDescriptor {
                        name: "test".into(),
                        physical_properties: PhysicalProperties {
                            size: (0, 0).into(),
                            subpixel: Subpixel::Unknown,
                            make: "Weld".into(),
                            model: "Test".into(),
                            serial_number: "test".into(),
                        },
                    },
                }],
            },
        )
        .expect("native seat");
        let swapped = KeyboardKeymap::compile(&KeymapConfig {
            options: "ctrl:swapcaps,altwin:swap_lalt_lwin".into(),
            ..Default::default()
        })
        .expect("swapped map");
        server
            .synchronize_keyboard_settings(Some(KeyboardSettings {
                keymap: Some(swapped.clone()),
                legacy_repeat: LegacyKeyRepeat::Disabled,
            }))
            .expect("settings");
        let keyboard = server.local_input.native.get_keyboard().expect("keyboard");
        let event = |state| {
            RawSeatEvent::new(
                RawSeatEventKind::Keyboard {
                    keycode: LinuxKeycode(58),
                    logical_key: None,
                    state,
                },
                1,
            )
        };
        let press = server.resolve_input(event(KeyboardKeyState::Pressed));
        assert!(press.modifiers.expect("modifiers").control);
        keyboard.input::<(), _>(
            &mut server,
            66_u32.into(),
            KeyEvent::Pressed,
            SERIAL_COUNTER.next_serial(),
            InputTime::from_millis(1),
            |_, _, _| FilterResult::Forward,
        );
        assert!(keyboard.modifier_state().ctrl);
        server
            .synchronize_keyboard_settings(Some(KeyboardSettings::default()))
            .expect("defer reload");
        assert_eq!(server.keyboard_mapper.keymap(), &swapped);
        server.resolve_input(event(KeyboardKeyState::Released));
        server
            .synchronize_keyboard_settings(None)
            .expect("client still holding");
        assert_eq!(server.keyboard_mapper.keymap(), &swapped);
        keyboard.input::<(), _>(
            &mut server,
            66_u32.into(),
            KeyEvent::Released,
            SERIAL_COUNTER.next_serial(),
            InputTime::from_millis(2),
            |_, _, _| FilterResult::Forward,
        );
        server
            .synchronize_keyboard_settings(None)
            .expect("apply deferred map");
        assert_eq!(server.keyboard_mapper.keymap(), &server.default_keymap);
        assert!(
            !server
                .resolve_input(event(KeyboardKeyState::Pressed))
                .modifiers
                .expect("modifiers")
                .control
        );
        keyboard.input::<(), _>(
            &mut server,
            66_u32.into(),
            KeyEvent::Pressed,
            SERIAL_COUNTER.next_serial(),
            InputTime::from_millis(3),
            |_, _, _| FilterResult::Forward,
        );
        assert!(!keyboard.modifier_state().ctrl);
        assert!(keyboard.modifier_state().caps_lock);
    }
}
