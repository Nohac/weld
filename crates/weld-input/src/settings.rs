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
        ecs::{resource::Resource, world::World},
    };

    #[derive(Default, Resource)]
    struct PublishedSettings(Option<KeyboardSettings>);

    pub fn register_keyboard_settings(app: &mut App) {
        app.init_resource::<KeyboardSettings>()
            .init_resource::<PublishedSettings>();
    }

    /// Drain initial or changed settings once per native publication boundary.
    pub fn take_keyboard_settings(world: &mut World) -> Option<KeyboardSettings> {
        let settings = world.get_resource::<KeyboardSettings>()?.clone();
        let mut published = world.get_resource_mut::<PublishedSettings>()?;
        if published.0.as_ref() == Some(&settings) {
            return None;
        }
        published.0 = Some(settings.clone());
        Some(settings)
    }
}

#[cfg(feature = "bevy")]
pub use integration::*;
