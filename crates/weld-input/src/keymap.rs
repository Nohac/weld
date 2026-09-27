//! XKB compilation and physical-event resolution for native seat hosts.

use crate::{KeyboardKeyState, LinuxKeycode, RawSeatEvent, RawSeatEventKind, SeatModifiers};
use anyhow::{Context, Result, ensure};
use std::{collections::HashSet, fmt, sync::Arc};
use winit::keyboard::{Key, NamedKey, NativeKey};
use xkbcommon::xkb::{self, keysyms};

/// XKB rules, model, layout, variant and options selected by configuration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KeymapConfig {
    pub rules: String,
    pub model: String,
    pub layout: String,
    pub variant: String,
    pub options: String,
}

impl Default for KeymapConfig {
    fn default() -> Self {
        Self {
            rules: String::new(),
            model: String::new(),
            layout: "us".into(),
            variant: String::new(),
            options: String::new(),
        }
    }
}

/// Validated, owned XKB text shared by the shell interpreter and native clients.
#[derive(Clone, Eq, PartialEq)]
pub struct KeyboardKeymap(Arc<str>);

impl fmt::Debug for KeyboardKeymap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("KeyboardKeymap")
            .field("bytes", &self.0.len())
            .finish()
    }
}

impl KeyboardKeymap {
    pub fn compile(config: &KeymapConfig) -> Result<Self> {
        for value in [
            &config.rules,
            &config.model,
            &config.layout,
            &config.variant,
            &config.options,
        ] {
            ensure!(
                !value.contains('\0'),
                "keyboard configuration contains a NUL byte"
            );
        }
        let context = xkb::Context::new(xkb::CONTEXT_NO_ENVIRONMENT_NAMES);
        let keymap = xkb::Keymap::new_from_names(
            &context,
            &config.rules,
            &config.model,
            &config.layout,
            &config.variant,
            Some(config.options.clone()),
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .context("could not compile keyboard keymap")?;
        Ok(Self(
            keymap.get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1).into(),
        ))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Host-owned XKB state. Every physical transition enters here before filtering;
/// shortcut-consumed keys still affect subsequent shell modifier snapshots.
pub struct KeyboardMapper {
    keymap: KeyboardKeymap,
    state: xkb::State,
    pressed: HashSet<LinuxKeycode>,
    modifiers: SeatModifiers,
}

impl KeyboardMapper {
    pub fn new(keymap: KeyboardKeymap) -> Result<Self> {
        let context = xkb::Context::new(xkb::CONTEXT_NO_ENVIRONMENT_NAMES);
        let native = xkb::Keymap::new_from_string(
            &context,
            keymap.as_str().to_owned(),
            xkb::KEYMAP_FORMAT_TEXT_V1,
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .context("could not load compiled keyboard keymap")?;
        Ok(Self {
            keymap,
            state: xkb::State::new(&native),
            pressed: HashSet::new(),
            modifiers: SeatModifiers::default(),
        })
    }

    pub fn keymap(&self) -> &KeyboardKeymap {
        &self.keymap
    }
    pub fn has_pressed_keys(&self) -> bool {
        !self.pressed.is_empty()
    }

    pub fn resolve(&mut self, mut event: RawSeatEvent) -> RawSeatEvent {
        match &mut event.event {
            RawSeatEventKind::Keyboard {
                keycode,
                state,
                logical_key,
            } => {
                if let Some(code) = keycode.0.checked_add(8).map(xkb::Keycode::new) {
                    // Resolve before the transition, as XKB lock/group actions can
                    // change which symbol this very key produces after release.
                    *logical_key = Some(logical_key_from_sym(self.state.key_get_one_sym(code)));
                    match state {
                        KeyboardKeyState::Pressed if self.pressed.insert(*keycode) => {
                            self.state.update_key(code, xkb::KeyDirection::Down);
                        }
                        KeyboardKeyState::Released if self.pressed.remove(keycode) => {
                            self.state.update_key(code, xkb::KeyDirection::Up);
                        }
                        _ => {}
                    }
                }
                self.refresh_modifiers();
            }
            RawSeatEventKind::HostFocusLost => {
                for key in self.pressed.drain() {
                    if let Some(code) = key.0.checked_add(8) {
                        self.state
                            .update_key(xkb::Keycode::new(code), xkb::KeyDirection::Up);
                    }
                }
                self.refresh_modifiers();
            }
            _ => {}
        }
        event.modifiers = Some(self.modifiers);
        event
    }

    fn refresh_modifiers(&mut self) {
        self.modifiers = SeatModifiers {
            control: self
                .state
                .mod_name_is_active(xkb::MOD_NAME_CTRL, xkb::STATE_MODS_EFFECTIVE),
            alt: self
                .state
                .mod_name_is_active(xkb::MOD_NAME_ALT, xkb::STATE_MODS_EFFECTIVE),
            shift: self
                .state
                .mod_name_is_active(xkb::MOD_NAME_SHIFT, xkb::STATE_MODS_EFFECTIVE),
            super_key: self
                .state
                .mod_name_is_active(xkb::MOD_NAME_LOGO, xkb::STATE_MODS_EFFECTIVE),
        };
    }
}

fn logical_key_from_sym(sym: xkb::Keysym) -> Key {
    let named = match sym.raw() {
        keysyms::KEY_BackSpace => NamedKey::Backspace,
        keysyms::KEY_Tab | keysyms::KEY_ISO_Left_Tab => NamedKey::Tab,
        keysyms::KEY_Return | keysyms::KEY_KP_Enter => NamedKey::Enter,
        keysyms::KEY_Escape => NamedKey::Escape,
        keysyms::KEY_Delete | keysyms::KEY_KP_Delete => NamedKey::Delete,
        keysyms::KEY_Insert | keysyms::KEY_KP_Insert => NamedKey::Insert,
        keysyms::KEY_Home | keysyms::KEY_KP_Home => NamedKey::Home,
        keysyms::KEY_End | keysyms::KEY_KP_End => NamedKey::End,
        keysyms::KEY_Page_Up | keysyms::KEY_KP_Page_Up => NamedKey::PageUp,
        keysyms::KEY_Page_Down | keysyms::KEY_KP_Page_Down => NamedKey::PageDown,
        keysyms::KEY_Left | keysyms::KEY_KP_Left => NamedKey::ArrowLeft,
        keysyms::KEY_Right | keysyms::KEY_KP_Right => NamedKey::ArrowRight,
        keysyms::KEY_Up | keysyms::KEY_KP_Up => NamedKey::ArrowUp,
        keysyms::KEY_Down | keysyms::KEY_KP_Down => NamedKey::ArrowDown,
        keysyms::KEY_Control_L | keysyms::KEY_Control_R => NamedKey::Control,
        keysyms::KEY_Shift_L | keysyms::KEY_Shift_R => NamedKey::Shift,
        keysyms::KEY_Alt_L | keysyms::KEY_Alt_R => NamedKey::Alt,
        keysyms::KEY_Super_L | keysyms::KEY_Super_R => NamedKey::Super,
        keysyms::KEY_Caps_Lock => NamedKey::CapsLock,
        keysyms::KEY_Num_Lock => NamedKey::NumLock,
        keysyms::KEY_ISO_Level3_Shift => NamedKey::AltGraph,
        keysyms::KEY_F1 => NamedKey::F1,
        keysyms::KEY_F2 => NamedKey::F2,
        keysyms::KEY_F3 => NamedKey::F3,
        keysyms::KEY_F4 => NamedKey::F4,
        keysyms::KEY_F5 => NamedKey::F5,
        keysyms::KEY_F6 => NamedKey::F6,
        keysyms::KEY_F7 => NamedKey::F7,
        keysyms::KEY_F8 => NamedKey::F8,
        keysyms::KEY_F9 => NamedKey::F9,
        keysyms::KEY_F10 => NamedKey::F10,
        keysyms::KEY_F11 => NamedKey::F11,
        keysyms::KEY_F12 => NamedKey::F12,
        _ => {
            return match char::from_u32(xkb::keysym_to_utf32(sym)).filter(|c| !c.is_control()) {
                Some(character) => Key::Character(character.to_string().into()),
                None => Key::Unidentified(NativeKey::Xkb(sym.raw())),
            };
        }
    };
    Key::Named(named)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: u32, state: KeyboardKeyState) -> RawSeatEvent {
        RawSeatEvent::new(
            RawSeatEventKind::Keyboard {
                keycode: LinuxKeycode(code),
                logical_key: Some(Key::Character("host-value".into())),
                state,
            },
            0,
        )
    }

    #[test]
    fn layout_resolves_text_and_repeats_do_not_toggle_locks() {
        let keymap = KeyboardKeymap::compile(&KeymapConfig {
            layout: "de".into(),
            ..Default::default()
        })
        .expect("map");
        let mut mapper = KeyboardMapper::new(keymap).expect("mapper");
        let event = mapper.resolve(key(21, KeyboardKeyState::Pressed));
        assert!(
            matches!(event.event, RawSeatEventKind::Keyboard { logical_key: Some(Key::Character(ref value)), .. } if value == "z")
        );
        mapper.resolve(key(21, KeyboardKeyState::Released));
        for state in [
            KeyboardKeyState::Pressed,
            KeyboardKeyState::Repeated,
            KeyboardKeyState::Released,
        ] {
            mapper.resolve(key(58, state));
        }
        let event = mapper.resolve(key(21, KeyboardKeyState::Pressed));
        assert!(
            matches!(event.event, RawSeatEventKind::Keyboard { logical_key: Some(Key::Character(ref value)), .. } if value == "Z")
        );
        mapper.resolve(RawSeatEvent::new(RawSeatEventKind::HostFocusLost, 0));
        assert!(!mapper.has_pressed_keys());
        assert_eq!(
            mapper
                .resolve(key(u32::MAX, KeyboardKeyState::Released))
                .modifiers,
            Some(SeatModifiers::default())
        );
    }
}
