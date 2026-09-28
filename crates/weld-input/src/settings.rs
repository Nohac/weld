use crate::KeyboardKeymap;

/// Repeat cadence owner for a native host's keyboard. Fixed at startup.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KeyboardRepeatMode {
    /// Applications generate repeats. Use when no upstream cadence is available.
    Client,
    /// Input supplies explicit repeats; keyboard-v10 applications use that cadence.
    Compositor,
}

/// Fallback for pre-v10 keyboards in compositor-repeat mode.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum LegacyKeyRepeat {
    #[default]
    Client,
    Disabled,
    /// Emit wire release/press pairs for upstream repeats.
    Emulated,
}

/// Live keyboard configuration published to the native host.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
#[cfg_attr(feature = "bevy", derive(bevy::ecs::resource::Resource))]
pub struct KeyboardSettings {
    pub legacy_repeat: LegacyKeyRepeat,
    /// A compiled configuration, or the host's default US keymap.
    pub keymap: Option<KeyboardKeymap>,
}

#[cfg(feature = "bevy")]
mod integration {
    use super::KeyboardSettings;
    use bevy::{
        app::App,
        ecs::{
            change_detection::DetectChanges,
            system::{Res, SystemState},
            world::World,
        },
    };

    /// Native publication history for the [`World`] supplied at construction.
    /// Keep this reader in the host alongside its application.
    pub struct KeyboardSettingsReader {
        reader: SystemState<Res<'static, KeyboardSettings>>,
        value: Option<KeyboardSettings>,
    }

    impl KeyboardSettingsReader {
        pub fn new(world: &mut World) -> Self {
            Self {
                reader: SystemState::new(world),
                value: None,
            }
        }

        /// Read initial or changed settings from this reader's original world.
        pub fn take(&mut self, world: &World) -> Option<KeyboardSettings> {
            // SystemState advances its change-detection boundary on each
            // read, including host polls between application updates.
            let settings = self.reader.get(world).ok()?;
            if !settings.is_changed() || self.value.as_ref() == Some(&*settings) {
                return None;
            }
            let settings = settings.clone();
            self.value = Some(settings.clone());
            Some(settings)
        }
    }

    pub fn register_keyboard_settings(app: &mut App) {
        app.init_resource::<KeyboardSettings>();
    }
}

#[cfg(feature = "bevy")]
pub use integration::*;
